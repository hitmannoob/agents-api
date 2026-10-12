use super::*;
use codex_agents_api::projects::Admin;
use codex_utils_cargo_bin::find_resource;
use pretty_assertions::assert_eq;

/// Project keys authenticate as their project, hold only their permissions,
/// and stop working once revoked; the operator token keeps every permission.
/// Keys are created through `Admin` while the server is serving, as the CLI
/// does.
#[tokio::test]
async fn project_keys_authenticate_and_hold_only_their_permissions() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 60), async {
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let provider = create_mock_responses_server_repeating_assistant("unused").await;
        MockResponsesConfig::new(&provider.uri()).write(home.path())?;
        let directory = AbsolutePathBuf::from_absolute_path(data.path())?;
        let api = AgentsApi::new(backend(home.path()).await?, directory.clone(), TOKEN.into()).await?;
        let (base, _server) = capabilities::serve(&api).await?;
        let admin = Admin::open(directory).await?;
        let project = admin.create_project("Research").await?;
        let key = |permissions: &[&str]| {
            let permissions: Vec<String> = permissions.iter().map(|&permission| permission.to_owned()).collect();
            let admin = &admin;
            let project = project.id.clone();
            async move { anyhow::Ok(admin.create_key(&project, "", &permissions).await?) }
        };
        let reader = key(&["api.agents.read"]).await?;
        let writer = key(&["api.agents.write"]).await?;
        let client = reqwest::Client::new();
        let send = |method: reqwest::Method, path: &str, secret: &str| {
            let mut builder = client
                .request(method.clone(), format!("{base}{path}"))
                .bearer_auth(secret)
                .header("OpenAI-Beta", "agents=v1");
            if method == reqwest::Method::POST {
                builder = builder.json(&json!({"model": "mock-model"}));
            }
            async move {
                let response = builder.send().await?;
                let status = response.status().as_u16();
                let body: Value = response.json().await?;
                anyhow::Ok((status, body))
            }
        };
        let error = |status: u16, message: &str| {
            (status, json!({"error":{"message":message,"type":"invalid_request_error","param":null,"code":null}}))
        };

        assert_eq!(send(reqwest::Method::GET, "/agents", &reader.value).await?.0, 200);
        assert_eq!(
            send(reqwest::Method::POST, "/agents", &reader.value).await?,
            error(403, "this API key lacks the api.agents.write permission")
        );
        assert_eq!(send(reqwest::Method::POST, "/agents", &writer.value).await?.0, 200);
        assert_eq!(
            send(reqwest::Method::GET, "/agents", &writer.value).await?,
            error(403, "this API key lacks the api.agents.read permission")
        );
        // Starting model work also needs the Responses permission.
        assert_eq!(
            send(reqwest::Method::POST, "/agents/sessions", &writer.value).await?,
            error(403, "this API key lacks the api.responses.write permission")
        );
        assert_eq!(
            send(reqwest::Method::GET, "/vaults", &reader.value).await?,
            error(403, "this API key lacks the api.vaults.read permission")
        );
        assert_eq!(
            send(reqwest::Method::GET, "/agents", "sk-proj-unknown").await?,
            error(401, "bearer token required")
        );
        // The operator token holds every permission.
        for path in ["/agents", "/vaults", "/files", "/webhook_endpoints"] {
            assert_eq!(send(reqwest::Method::GET, path, TOKEN).await?.0, 200, "{path}");
        }

        // Revocation takes effect on the next request, without a restart.
        admin.revoke_key(&reader.key.id).await?;
        assert_eq!(
            send(reqwest::Method::GET, "/agents", &reader.value).await?,
            error(401, "bearer token required")
        );
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

/// Another project's agents and sessions, and everything under a session,
/// read as missing: every inventoried operation that names one answers 404,
/// lists leave them out, their cursors are invalid, and a session cannot be
/// created from them. Nothing is changed for their owner.
#[tokio::test]
async fn projects_cannot_see_or_change_each_others_agents_and_sessions() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 60), async {
        let inventory: Value = serde_json::from_str(&std::fs::read_to_string(find_resource!(
            "CONTRACT_INVENTORY.json"
        )?)?)?;
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let provider = create_mock_responses_server_repeating_assistant("done").await;
        MockResponsesConfig::new(&provider.uri()).write(home.path())?;
        let directory = AbsolutePathBuf::from_absolute_path(data.path())?;
        let api =
            AgentsApi::new(backend(home.path()).await?, directory.clone(), TOKEN.into()).await?;
        let (base, _server) = capabilities::serve(&api).await?;
        let admin = Admin::open(directory).await?;
        let other = admin.create_project("Other").await?;
        let all: Vec<String> = codex_agents_api::projects::PERMISSIONS
            .iter()
            .map(|&permission| permission.to_owned())
            .collect();
        let outsider = admin.create_key(&other.id, "", &all).await?.value;
        let client = reqwest::Client::new();

        // The default project (the operator token) owns an agent and a session.
        let agent = request(
            &client,
            reqwest::Method::POST,
            &format!("{base}/agents"),
            json!({"model":"mock-model"}),
        )
        .await?;
        let agent_id = agent["id"].as_str().context("agent id")?.to_owned();
        let session = request(
            &client,
            reqwest::Method::POST,
            &format!("{base}/agents/sessions"),
            json!({"agent_id":agent_id,"environment":{"type":"none"},"input":"hello"}),
        )
        .await?;
        let session_id = session["id"].as_str().context("session id")?.to_owned();
        let session_url = format!("{base}/agents/sessions/{session_id}");
        idle(&client, &session_url, /*expected_turns*/ 1).await?;

        let send = |method: reqwest::Method, url: String, body: Value| {
            let request = client
                .request(method, url)
                .bearer_auth(&outsider)
                .header("OpenAI-Beta", "agents=v1")
                .json(&body);
            async move {
                let response = request.send().await?;
                let status = response.status().as_u16();
                anyhow::Ok((status, response.json::<Value>().await?))
            }
        };
        let swept = sweep(
            &client,
            &base,
            &outsider,
            &inventory,
            &[
                ("{agent_id}", &agent_id, "agent not found"),
                ("{session_id}", &session_id, "session not found"),
            ],
        )
        .await?;
        assert!(swept >= 15, "{swept}");

        // Lists leave the other project's rows out, and its IDs are not cursors.
        for (path, cursor) in [
            ("/agents", agent_id.as_str()),
            ("/agents/sessions", session_id.as_str()),
        ] {
            let (status, page) =
                send(reqwest::Method::GET, format!("{base}{path}"), Value::Null).await?;
            assert_eq!((status, page["data"].clone()), (200, json!([])), "{path}");
            let (status, _) = send(
                reqwest::Method::GET,
                format!("{base}{path}?after={cursor}"),
                Value::Null,
            )
            .await?;
            assert_eq!(status, 400, "{path} cursor");
        }
        let (status, page) = send(
            reqwest::Method::GET,
            format!("{base}/agents/sessions?agent_id={agent_id}"),
            Value::Null,
        )
        .await?;
        assert_eq!((status, page["data"].clone()), (200, json!([])));
        let (status, body) = send(
            reqwest::Method::POST,
            format!("{base}/agents/sessions"),
            json!({"agent_id":agent_id,"environment":{"type":"none"},"input":"hello"}),
        )
        .await?;
        assert_eq!(
            (status, body["error"]["message"].clone()),
            (404, json!("agent not found"))
        );

        // The outsider's own resources stay out of the owner's lists.
        let (status, own) = send(
            reqwest::Method::POST,
            format!("{base}/agents"),
            json!({"model":"mock-model"}),
        )
        .await?;
        assert_eq!(status, 200);
        let listed = request(
            &client,
            reqwest::Method::GET,
            &format!("{base}/agents"),
            Value::Null,
        )
        .await?;
        let ids: Vec<&Value> = listed["data"]
            .as_array()
            .context("agents")?
            .iter()
            .map(|agent| &agent["id"])
            .collect();
        assert_eq!(ids, vec![&agent["id"]]);
        assert_ne!(own["id"], agent["id"]);

        // The owner's agent and session survived the outsider's deletes.
        request(
            &client,
            reqwest::Method::GET,
            &format!("{base}/agents/{agent_id}"),
            Value::Null,
        )
        .await?;
        assert_eq!(
            request(&client, reqwest::Method::GET, &session_url, Value::Null).await?["id"],
            json!(session_id)
        );
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

/// Send every implemented inventoried operation that names one of `roots`
/// (placeholder, another project's ID, and its not-found message) with
/// `secret`, filling other path parameters with placeholders. Each must
/// answer 404 with that root's message; the most specific root in a path
/// decides it. Returns how many operations were sent.
pub(super) async fn sweep(
    client: &reqwest::Client,
    base: &str,
    secret: &str,
    inventory: &Value,
    roots: &[(&str, &str, &str)],
) -> anyhow::Result<usize> {
    let mut observed = Vec::new();
    let mut expected = Vec::new();
    for operation in inventory["operations"].as_array().context("operations")? {
        let template = operation["path"].as_str().context("path")?;
        let named: Vec<_> = roots
            .iter()
            .filter(|(placeholder, ..)| template.contains(placeholder))
            .collect();
        let Some((_, _, message)) = named.first() else {
            continue;
        };
        if operation["status"] == "missing" {
            continue;
        }
        let mut path = template.to_owned();
        for (placeholder, id, _) in &named {
            path = path.replace(placeholder, id);
        }
        let path = path
            .split('/')
            .map(|segment| {
                if segment.starts_with('{') {
                    "placeholder"
                } else {
                    segment
                }
            })
            .collect::<Vec<_>>()
            .join("/");
        let method = reqwest::Method::from_bytes(
            operation["method"].as_str().context("method")?.as_bytes(),
        )?;
        let response = client
            .request(method, format!("{base}{path}"))
            .bearer_auth(secret)
            .header("OpenAI-Beta", "agents=v1")
            .json(&json!({}))
            .send()
            .await?;
        let status = response.status().as_u16();
        let body: Value = response.json().await?;
        observed
            .push(json!({"id":operation["id"],"status":status,"message":body["error"]["message"]}));
        expected.push(json!({"id":operation["id"],"status":404,"message":message}));
    }
    assert_eq!(observed, expected);
    Ok(observed.len())
}

/// Another project's vaults (with their credentials), files, and
/// environments read as missing, stay out of lists, are not cursors, and
/// cannot be referenced from the caller's own sessions or environments.
#[tokio::test]
async fn projects_cannot_reach_each_others_vaults_files_or_environments() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 60), async {
        let inventory: Value = serde_json::from_str(&std::fs::read_to_string(find_resource!("CONTRACT_INVENTORY.json")?)?)?;
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let workspace_directory = workspace.path().to_str().context("workspace")?;
        let provider = create_mock_responses_server_repeating_assistant("unused").await;
        MockResponsesConfig::new(&provider.uri()).write(home.path())?;
        let directory = AbsolutePathBuf::from_absolute_path(data.path())?;
        let api = AgentsApi::new(backend(home.path()).await?, directory.clone(), TOKEN.into()).await?;
        let (base, _server) = capabilities::serve(&api).await?;
        api.configure_environments("an-environment-key-for-project-checks".into(), format!("{}/registry", base.trim_end_matches("/v1")))?;
        let admin = Admin::open(directory).await?;
        let other = admin.create_project("Other").await?;
        let all: Vec<String> = codex_agents_api::projects::PERMISSIONS.iter().map(|&permission| permission.to_owned()).collect();
        let outsider = admin.create_key(&other.id, "", &all).await?.value;
        let client = reqwest::Client::new();
        let self_hosted = json!({"agent":{"model":"mock-model"},"environment":{"type":"self_hosted","workspace_directory":workspace_directory}});

        // The default project owns a vault, a file, and an environment.
        let vault = request(&client, reqwest::Method::POST, &format!("{base}/vaults"), json!({"name":"owned"})).await?;
        let vault_id = vault["id"].as_str().context("vault id")?.to_owned();
        let file = files::upload(&client, &base, &[("purpose", "user_data")], "owned.txt", b"owned").await?.error_for_status()?.json::<Value>().await?;
        let file_id = file["id"].as_str().context("file id")?.to_owned();
        let session = request(&client, reqwest::Method::POST, &format!("{base}/agents/sessions"), self_hosted.clone()).await?;
        let environment_id = session["environment"]["id"].as_str().context("environment id")?.to_owned();

        let swept = sweep(&client, &base, &outsider, &inventory, &[
            ("{vault_id}", &vault_id, "vault not found"),
            ("{file_id}", &file_id, "file not found"),
            ("{environment_id}", &environment_id, "environment not found"),
        ]).await?;
        assert_eq!(swept, 13);

        let send = |method: reqwest::Method, url: String, body: Value| {
            let request = client.request(method, url).bearer_auth(&outsider).header("OpenAI-Beta", "agents=v1").json(&body);
            async move {
                let response = request.send().await?;
                let status = response.status().as_u16();
                anyhow::Ok((status, response.json::<Value>().await?))
            }
        };
        for (path, cursor) in [("/vaults", vault_id.as_str()), ("/files", file_id.as_str())] {
            let (status, page) = send(reqwest::Method::GET, format!("{base}{path}"), Value::Null).await?;
            assert_eq!((status, page["data"].clone()), (200, json!([])), "{path}");
            let (status, _) = send(reqwest::Method::GET, format!("{base}{path}?after={cursor}"), Value::Null).await?;
            assert_eq!(status, 400, "{path} cursor");
        }
        // Sessions attach only their own project's vaults.
        let mut with_vault = self_hosted.clone();
        with_vault["vault_ids"] = json!([vault_id]);
        let (status, body) = send(reqwest::Method::POST, format!("{base}/agents/sessions"), with_vault).await?;
        assert_eq!((status, body["error"]["message"].clone()), (400, json!(format!("vault {vault_id} not found"))));
        // Environment files copy only the project's own uploads.
        let (status, own) = send(reqwest::Method::POST, format!("{base}/agents/sessions"), self_hosted).await?;
        assert_eq!(status, 200, "{own}");
        let own_environment = own["environment"]["id"].as_str().context("own environment id")?;
        let (status, body) = send(reqwest::Method::POST, format!("{base}/agents/environments/{own_environment}/files"),
            json!({"type":"file_id","path":"copied.txt","file_id":file_id})).await?;
        assert_eq!((status, body["error"]["message"].clone()), (400, json!(format!("file {file_id} not found"))));

        // The owner still has everything.
        request(&client, reqwest::Method::GET, &format!("{base}/vaults/{vault_id}"), Value::Null).await?;
        request(&client, reqwest::Method::GET, &format!("{base}/files/{file_id}"), Value::Null).await?;
        request(&client, reqwest::Method::GET, &format!("{base}/agents/environments/{environment_id}"), Value::Null).await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
