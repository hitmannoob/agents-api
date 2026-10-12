use super::*;
use pretty_assertions::assert_eq;
use std::io::Read;
use std::io::Write;

const BOUNDARY: &str = "agents-api-skill-boundary";

/// A skill ZIP holding one top-level folder named after the skill.
pub(super) fn skill_zip(
    name: &str,
    description: &str,
    extra: &[(&str, &[u8])],
) -> anyhow::Result<Vec<u8>> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    let manifest = format!("---\nname: {name}\ndescription: {description}\n---\n# {name}\n");
    let files = std::iter::once(("SKILL.md", manifest.as_bytes())).chain(extra.iter().copied());
    for (path, contents) in files {
        writer.start_file(format!("{name}/{path}"), options)?;
        writer.write_all(contents)?;
    }
    Ok(writer.finish()?.into_inner())
}

/// Upload files as the SDK does: one `files[]` part per file, plus fields.
pub(super) async fn upload(
    client: &reqwest::Client,
    url: &str,
    secret: &str,
    fields: &[(&str, &str)],
    files: &[(&str, &[u8])],
) -> anyhow::Result<reqwest::Response> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .bytes(),
        );
    }
    for (filename, contents) in files {
        body.extend(format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"files[]\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n").bytes());
        body.extend_from_slice(contents);
        body.extend(b"\r\n");
    }
    body.extend(format!("--{BOUNDARY}--\r\n").bytes());
    Ok(client
        .post(url)
        .bearer_auth(secret)
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(body)
        .send()
        .await?)
}

/// Create a skill from a ZIP and return its public object.
pub(super) async fn create(
    client: &reqwest::Client,
    base: &str,
    secret: &str,
    name: &str,
) -> anyhow::Result<Value> {
    let archive = skill_zip(name, "A skill for tests. Use when testing.", &[])?;
    Ok(upload(
        client,
        &format!("{base}/skills"),
        secret,
        &[],
        &[(&format!("{name}.zip"), &archive)],
    )
    .await?
    .error_for_status()?
    .json()
    .await?)
}

/// The (path, contents) entries of a ZIP.
fn entries(archive: &[u8]) -> anyhow::Result<Vec<(String, String)>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))?;
    let mut entries = Vec::new();
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        let mut contents = String::new();
        entry.read_to_string(&mut contents)?;
        entries.push((entry.name().to_owned(), contents));
    }
    Ok(entries)
}

#[tokio::test]
async fn skills_keep_numbered_versions_with_a_default() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 60), async {
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let provider = create_mock_responses_server_repeating_assistant("unused").await;
        MockResponsesConfig::new(&provider.uri()).write(home.path())?;
        let api = AgentsApi::new(backend(home.path()).await?, AbsolutePathBuf::from_absolute_path(data.path())?, TOKEN.into()).await?;
        let (base, _server) = capabilities::serve(&api).await?;
        let client = reqwest::Client::new();
        let get = |path: String| {
            let url = format!("{base}{path}");
            let client = &client;
            async move { request(client, reqwest::Method::GET, &url, Value::Null).await }
        };
        let refused = |response: reqwest::Response| async move {
            let status = response.status().as_u16();
            let body: Value = response.json().await?;
            anyhow::Ok((status, body["error"]["message"].as_str().unwrap_or_default().to_owned()))
        };

        // A ZIP upload creates version 1, which is both default and latest.
        let archive = skill_zip("pdf-tools", "Merge PDFs. Use when handling PDFs.", &[("scripts/merge.py", b"print(1)")])?;
        let skill = upload(&client, &format!("{base}/skills"), TOKEN, &[], &[("pdf-tools.zip", &archive)]).await?
            .error_for_status()?.json::<Value>().await?;
        let id = skill["id"].as_str().context("skill id")?.to_owned();
        assert!(id.starts_with("skill_"), "{id}");
        assert_eq!(skill, json!({"id":id,"object":"skill","name":"pdf-tools","description":"Merge PDFs. Use when handling PDFs.",
            "default_version":"1","latest_version":"1","created_at":skill["created_at"]}));
        assert_eq!(get(format!("/skills/{id}")).await?, skill);
        let content = client.get(format!("{base}/skills/{id}/content")).bearer_auth(TOKEN).send().await?.error_for_status()?;
        assert_eq!(content.headers()["content-type"], "application/zip");
        assert_eq!(entries(&content.bytes().await?)?, vec![
            ("pdf-tools/SKILL.md".to_owned(), "---\nname: pdf-tools\ndescription: Merge PDFs. Use when handling PDFs.\n---\n# pdf-tools\n".to_owned()),
            ("pdf-tools/scripts/merge.py".to_owned(), "print(1)".to_owned()),
        ]);

        // A directory upload names each file by its path.
        let manifest = b"---\nname: notes\ndescription: Keep notes. Use when asked to remember.\n---\n";
        let notes = upload(&client, &format!("{base}/skills"), TOKEN, &[], &[("notes/SKILL.md", manifest), ("notes/a.txt", b"a")]).await?
            .error_for_status()?.json::<Value>().await?;
        let notes_id = notes["id"].as_str().context("notes id")?.to_owned();
        let ids = |page: &Value| page["data"].as_array().into_iter().flatten().map(|item| item["id"].clone()).collect::<Vec<_>>();
        let newest = get("/skills?limit=1".into()).await?;
        assert_eq!((ids(&newest), &newest["has_more"], &newest["first_id"]), (vec![json!(notes_id)], &json!(true), &json!(notes_id)));
        assert_eq!(ids(&get(format!("/skills?after={notes_id}")).await?), vec![json!(id)]);
        assert_eq!(ids(&get("/skills?order=asc".into()).await?), vec![json!(id), json!(notes_id)]);

        // New versions are numbered in order; only `default` moves the default.
        let second = skill_zip("pdf-tools", "Merge and split PDFs.", &[])?;
        let version = upload(&client, &format!("{base}/skills/{id}/versions"), TOKEN, &[], &[("v2.zip", &second)]).await?
            .error_for_status()?.json::<Value>().await?;
        let version_id = version["id"].as_str().context("version id")?.to_owned();
        assert!(version_id.starts_with("skillver_"), "{version_id}");
        assert_eq!(version, json!({"id":version_id,"object":"skill.version","skill_id":id,"version":"2",
            "name":"pdf-tools","description":"Merge and split PDFs.","created_at":version["created_at"]}));
        let skill = get(format!("/skills/{id}")).await?;
        assert_eq!((&skill["default_version"], &skill["latest_version"], &skill["description"]),
            (&json!("1"), &json!("2"), &json!("Merge PDFs. Use when handling PDFs.")));
        let third = skill_zip("pdf-tools", "Merge, split, and fill PDFs.", &[])?;
        upload(&client, &format!("{base}/skills/{id}/versions"), TOKEN, &[("default", "true")], &[("v3.zip", &third)]).await?.error_for_status()?;
        let skill = get(format!("/skills/{id}")).await?;
        assert_eq!((&skill["default_version"], &skill["latest_version"], &skill["description"]),
            (&json!("3"), &json!("3"), &json!("Merge, split, and fill PDFs.")));
        let skill = request(&client, reqwest::Method::POST, &format!("{base}/skills/{id}"), json!({"default_version":"2"})).await?;
        assert_eq!((&skill["default_version"], &skill["description"]), (&json!("2"), &json!("Merge and split PDFs.")));
        let response = client.post(format!("{base}/skills/{id}")).bearer_auth(TOKEN).json(&json!({"default_version":"9"})).send().await?;
        assert_eq!(refused(response).await?, (400, "default_version 9 is not a version of this skill".to_owned()));

        let numbers = |page: &Value| page["data"].as_array().into_iter().flatten().map(|item| item["version"].clone()).collect::<Vec<_>>();
        let versions = get(format!("/skills/{id}/versions")).await?;
        assert_eq!(numbers(&versions), vec![json!("3"), json!("2"), json!("1")]);
        let first_id = versions["data"][2]["id"].as_str().context("first version id")?.to_owned();
        assert_eq!(numbers(&get(format!("/skills/{id}/versions?order=asc&after={first_id}")).await?), vec![json!("2"), json!("3")]);
        assert_eq!(get(format!("/skills/{id}/versions/2")).await?, version);
        let first = client.get(format!("{base}/skills/{id}/versions/1/content")).bearer_auth(TOKEN).send().await?.error_for_status()?;
        assert_eq!(entries(&first.bytes().await?)?.len(), 2);

        // The default cannot be deleted while other versions remain.
        let response = client.delete(format!("{base}/skills/{id}/versions/2")).bearer_auth(TOKEN).send().await?;
        assert_eq!(refused(response).await?, (400, "the default version cannot be deleted; set another default version first".to_owned()));
        assert_eq!(request(&client, reqwest::Method::DELETE, &format!("{base}/skills/{id}/versions/3"), Value::Null).await?["version"], json!("3"));
        assert_eq!(get(format!("/skills/{id}")).await?["latest_version"], json!("2"));
        assert_eq!(request(&client, reqwest::Method::DELETE, &format!("{base}/skills/{id}/versions/1"), Value::Null).await?,
            json!({"id":first_id,"object":"skill.version.deleted","deleted":true,"version":"1"}));
        for path in [format!("/skills/{id}/versions/1"), format!("/skills/{id}/versions/1/content"), format!("/skills/{id}/versions/0")] {
            let response = client.get(format!("{base}{path}")).bearer_auth(TOKEN).send().await?;
            assert_eq!(refused(response).await?, (404, "skill version not found".to_owned()), "{path}");
        }
        // Deleting the last version deletes the skill.
        request(&client, reqwest::Method::DELETE, &format!("{base}/skills/{id}/versions/2"), Value::Null).await?;
        let response = client.get(format!("{base}/skills/{id}")).bearer_auth(TOKEN).send().await?;
        assert_eq!(refused(response).await?, (404, "skill not found".to_owned()));

        // Uploads that break the contract are refused in the public error
        // shape and leave nothing behind.
        let no_manifest = {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            writer.start_file("broken/README.md", zip::write::SimpleFileOptions::default())?;
            writer.write_all(b"no manifest")?;
            writer.finish()?.into_inner()
        };
        let mut truncated = archive.clone();
        truncated.truncate(10);
        for (fields, files, message) in [
            (vec![], vec![("broken.zip", no_manifest.as_slice())], "the skill bundle needs a SKILL.md directly inside broken/"),
            (vec![], vec![("cut.zip", truncated.as_slice())], "the uploaded skill is not a readable ZIP archive"),
            (vec![("purpose", "assistants")], vec![("pdf-tools.zip", archive.as_slice())], "unknown field in skill upload: purpose"),
            (vec![("default", "yes")], vec![("pdf-tools.zip", archive.as_slice())], "unknown field in skill upload: default"),
        ] {
            let response = upload(&client, &format!("{base}/skills"), TOKEN, &fields, &files).await?;
            assert_eq!(refused(response).await?, (400, message.to_owned()));
        }
        let response = upload(&client, &format!("{base}/skills/{notes_id}/versions"), TOKEN, &[("default", "yes")], &[("pdf-tools.zip", &archive)]).await?;
        assert_eq!(refused(response).await?, (400, "default must be true or false".to_owned()));
        // The SDK sends a lone file that is not in a list as a JSON body.
        let response = client.post(format!("{base}/skills")).bearer_auth(TOKEN).json(&json!({})).send().await?;
        assert_eq!(refused(response).await?, (400, "files is required; upload the skill as multipart/form-data".to_owned()));
        let stored = || -> anyhow::Result<Vec<String>> {
            Ok(std::fs::read_dir(data.path().join("skills"))?
                .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
                .collect::<Result<_, _>>()?)
        };
        assert_eq!(stored()?.len(), 1, "{:?}", stored()?);

        // Contents changed on disk fail their integrity check.
        let notes_file = data.path().join("skills").join(&stored()?[0]);
        std::fs::write(&notes_file, b"tampered")?;
        let response = client.get(format!("{base}/skills/{notes_id}/content")).bearer_auth(TOKEN).send().await?;
        assert_eq!(response.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);

        assert_eq!(request(&client, reqwest::Method::DELETE, &format!("{base}/skills/{notes_id}"), Value::Null).await?,
            json!({"id":notes_id,"object":"skill.deleted","deleted":true}));
        assert_eq!(stored()?, Vec::<String>::new());
        assert_eq!(get("/skills".into()).await?, json!({"object":"list","data":[],"first_id":null,"last_id":null,"has_more":false}));
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
