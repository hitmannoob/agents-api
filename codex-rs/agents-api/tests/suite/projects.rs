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

/// Another project's vaults (with their credentials), files, environments,
/// and skills read as missing, stay out of lists, are not cursors, and cannot
/// be referenced from the caller's own sessions or environments.
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
        let skill = skills::create(&client, &base, TOKEN, "owned").await?;
        let skill_id = skill["id"].as_str().context("skill id")?.to_owned();

        let swept = sweep(&client, &base, &outsider, &inventory, &[
            ("{vault_id}", &vault_id, "vault not found"),
            ("{file_id}", &file_id, "file not found"),
            ("{environment_id}", &environment_id, "environment not found"),
            ("{skill_id}", &skill_id, "skill not found"),
        ]).await?;
        assert_eq!(swept, 22);

        let send = |method: reqwest::Method, url: String, body: Value| {
            let request = client.request(method, url).bearer_auth(&outsider).header("OpenAI-Beta", "agents=v1").json(&body);
            async move {
                let response = request.send().await?;
                let status = response.status().as_u16();
                anyhow::Ok((status, response.json::<Value>().await?))
            }
        };
        for (path, cursor) in [("/vaults", vault_id.as_str()), ("/files", file_id.as_str()), ("/skills", skill_id.as_str())] {
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
        request(&client, reqwest::Method::GET, &format!("{base}/skills/{skill_id}"), Value::Null).await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

/// The permissions an inventoried operation needs, stated from its method and
/// path independently of the server's own mapping.
fn needed(operation: &Value) -> Vec<&'static str> {
    let path = operation["path"].as_str().unwrap_or_default();
    let read = operation["method"] == "GET";
    let pick = |read_permission, write_permission| {
        if read {
            read_permission
        } else {
            write_permission
        }
    };
    let base = if path.starts_with("/vaults") {
        pick("api.vaults.read", "api.vaults.write")
    } else if path.starts_with("/files") {
        pick("api.files.read", "api.files.write")
    } else if path.starts_with("/webhook_") {
        pick("api.webhooks.read", "api.webhooks.write")
    } else if path.starts_with("/skills") {
        pick("api.skills.read", "api.skills.write")
    } else {
        pick("api.agents.read", "api.agents.write")
    };
    // Creating a session and posting session events start model work.
    if matches!(operation["id"].as_str(), Some("SES-001" | "EVT-001")) {
        vec![base, "api.responses.write"]
    } else {
        vec![base]
    }
}

/// Every implemented operation refuses a key that lacks one of its
/// permissions with 403 in the public error shape, naming that permission,
/// and admits a key holding exactly its permissions.
#[tokio::test]
async fn every_operation_requires_its_permissions() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 60), async {
        let inventory: Value = serde_json::from_str(&std::fs::read_to_string(find_resource!("CONTRACT_INVENTORY.json")?)?)?;
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let provider = create_mock_responses_server_repeating_assistant("unused").await;
        MockResponsesConfig::new(&provider.uri()).write(home.path())?;
        let directory = AbsolutePathBuf::from_absolute_path(data.path())?;
        let api = AgentsApi::new(backend(home.path()).await?, directory.clone(), TOKEN.into()).await?;
        let (base, _server) = capabilities::serve(&api).await?;
        let admin = Admin::open(directory).await?;
        let project = admin.create_project("Permissions").await?;
        let client = reqwest::Client::new();
        let mut keys = std::collections::HashMap::<Vec<String>, String>::new();
        let mut observed = Vec::new();
        let mut expected = Vec::new();
        for operation in inventory["operations"].as_array().context("operations")? {
            if operation["status"] == "missing" {
                continue;
            }
            let needed = needed(operation);
            let path = operation["path"].as_str().context("path")?
                .split('/')
                .map(|segment| if segment.starts_with('{') { "placeholder" } else { segment })
                .collect::<Vec<_>>()
                .join("/");
            let method = reqwest::Method::from_bytes(operation["method"].as_str().context("method")?.as_bytes())?;
            // Holding everything else but one needed permission, then exactly
            // the needed ones.
            let mut cases: Vec<(Vec<String>, Option<&str>)> = needed.iter().map(|&missing| {
                let held = codex_agents_api::projects::PERMISSIONS.iter()
                    .filter(|&&permission| permission != missing)
                    .map(|&permission| permission.to_owned())
                    .collect();
                (held, Some(missing))
            }).collect();
            cases.push((needed.iter().map(|&permission| permission.to_owned()).collect(), None));
            for (held, missing) in cases {
                let secret = match keys.get(&held) {
                    Some(secret) => secret.clone(),
                    None => {
                        let secret = admin.create_key(&project.id, "", &held).await?.value;
                        keys.insert(held.clone(), secret.clone());
                        secret
                    }
                };
                let response = client.request(method.clone(), format!("{base}{path}")).bearer_auth(&secret)
                    .header("OpenAI-Beta", "agents=v1").json(&json!({})).send().await?;
                let status = response.status().as_u16();
                let body: Value = response.json().await.unwrap_or(Value::Null);
                match missing {
                    Some(missing) => {
                        observed.push(json!({"id":operation["id"],"missing":missing,"status":status,"error":body["error"]}));
                        expected.push(json!({"id":operation["id"],"missing":missing,"status":403,"error":{
                            "message":format!("this API key lacks the {missing} permission"),
                            "type":"invalid_request_error","param":null,"code":null}}));
                    }
                    None => {
                        let admitted = !matches!(status, 401 | 403);
                        observed.push(json!({"id":operation["id"],"admitted":admitted}));
                        expected.push(json!({"id":operation["id"],"admitted":true}));
                    }
                }
            }
        }
        assert_eq!(observed, expected);
        // 51 implemented operations, each refused and then admitted.
        assert!(observed.len() >= 104, "{}", observed.len());
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

/// Encode one ASCII character without changing the ID Axum hands the handler.
fn encoded_id(id: &str, middle: bool) -> String {
    assert!(id.is_ascii() && id.len() > 2);
    let index = if middle { id.len() / 2 } else { 0 };
    format!(
        "{}%{:02x}{}",
        &id[..index],
        id.as_bytes()[index],
        &id[index + 1..]
    )
}

#[tokio::test]
async fn percent_encoded_ids_cannot_cross_project_boundaries() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 120), async {
        let inventory: Value = serde_json::from_str(&std::fs::read_to_string(find_resource!("CONTRACT_INVENTORY.json")?)?)?;
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let provider = create_mock_responses_server_repeating_assistant("owned response").await;
        MockResponsesConfig::new(&provider.uri()).write(home.path())?;
        let directory = AbsolutePathBuf::from_absolute_path(data.path())?;
        let api = AgentsApi::new(backend(home.path()).await?, directory.clone(), TOKEN.into()).await?;
        api.configure_vault("project-isolation-test-passphrase-0123456789".into()).await?;
        api.allow_webhook_hosts(["127.0.0.1".to_owned()]);
        let (base, _server) = capabilities::serve(&api).await?;
        api.configure_environments("project-isolation-environment-key".into(), format!("{}/registry", base.trim_end_matches("/v1")))?;
        let admin = Admin::open(directory).await?;
        let other = admin.create_project("Other").await?;
        let all: Vec<String> = codex_agents_api::projects::PERMISSIONS.iter().map(|&p| p.to_owned()).collect();
        let outsider = admin.create_key(&other.id, "", &all).await?.value;
        let client = reqwest::Client::new();

        let agent = request(&client, reqwest::Method::POST, &format!("{base}/agents"), json!({"model":"mock-model","name":"owned","instructions":"private instructions"})).await?;
        let agent_id = agent["id"].as_str().context("agent id")?;
        let session = request(&client, reqwest::Method::POST, &format!("{base}/agents/sessions"), json!({"agent_id":agent_id,"environment":{"type":"none"},"input":"private input"})).await?;
        let session_id = session["id"].as_str().context("session id")?;
        let session_url = format!("{base}/agents/sessions/{session_id}");
        idle(&client, &session_url, /*expected_turns*/ 1).await?;
        let hosted = request(&client, reqwest::Method::POST, &format!("{base}/agents/sessions"), json!({"agent_id":agent_id,"environment":{"type":"self_hosted","workspace_directory":workspace.path()}})).await?;
        let environment_id = hosted["environment"]["id"].as_str().context("environment id")?;
        let vault = request(&client, reqwest::Method::POST, &format!("{base}/vaults"), json!({"name":"owned"})).await?;
        let vault_id = vault["id"].as_str().context("vault id")?;
        let credential = request(&client, reqwest::Method::POST, &format!("{base}/vaults/{vault_id}/credentials"), json!({"name":"owned","auth":{"type":"static_bearer","token":"owned-secret","mcp_server_url":"https://example.com/mcp"}})).await?;
        let credential_id = credential["id"].as_str().context("credential id")?;
        let file = files::upload(&client, &base, &[("purpose", "user_data")], "owned.txt", b"private file bytes").await?.error_for_status()?.json::<Value>().await?;
        let file_id = file["id"].as_str().context("file id")?;
        let skill = skills::create(&client, &base, TOKEN, "owned").await?;
        let skill_id = skill["id"].as_str().context("skill id")?;
        let hook = request(&client, reqwest::Method::POST, &format!("{base}/webhook_endpoints"), json!({"name":"owned","url":"http://127.0.0.1:9/hook","event_types":["batch.completed"]})).await?;
        let hook_id = hook["id"].as_str().context("webhook id")?;
        let prototype = request(&client, reqwest::Method::POST, &format!("{base}/sessions"), json!({"agentId":agent_id,"environment":{"type":"none"}})).await?;
        let prototype_id = prototype["id"].as_str().context("prototype id")?;

        // Compare every owned root and its saved children/content after both
        // mutation sweeps. No executor is needed: foreign environment file
        // requests must fail before checking connectivity or touching compute.
        let paths = vec![
            format!("/agents/{agent_id}"), format!("/agents/sessions/{session_id}"),
            format!("/agents/sessions/{session_id}/items"), format!("/agents/sessions/{session_id}/turns"),
            format!("/vaults/{vault_id}"), format!("/vaults/{vault_id}/credentials/{credential_id}"),
            format!("/files/{file_id}"), format!("/files/{file_id}/content"),
            format!("/skills/{skill_id}"), format!("/skills/{skill_id}/content"),
            format!("/skills/{skill_id}/versions/1"), format!("/skills/{skill_id}/versions/1/content"),
            format!("/webhook_endpoints/{hook_id}"), format!("/agents/environments/{environment_id}"),
            format!("/sessions/{prototype_id}"),
        ];
        let mut before = Vec::new();
        for path in &paths {
            before.push(client.get(format!("{base}{path}")).bearer_auth(TOKEN).send().await?.error_for_status()?.bytes().await?);
        }
        for middle in [false, true] {
            let encoded_agent = encoded_id(agent_id, middle);
            let encoded_session = encoded_id(session_id, middle);
            let encoded_vault = encoded_id(vault_id, middle);
            let encoded_file = encoded_id(file_id, middle);
            let encoded_skill = encoded_id(skill_id, middle);
            let encoded_hook = encoded_id(hook_id, middle);
            let encoded_environment = encoded_id(environment_id, middle);
            let swept = sweep(&client, &base, &outsider, &inventory, &[
                ("{session_id}", &encoded_session, "session not found"),
                ("{agent_id}", &encoded_agent, "agent not found"),
                ("{vault_id}", &encoded_vault, "vault not found"),
                ("{file_id}", &encoded_file, "file not found"),
                ("{skill_id}", &encoded_skill, "skill not found"),
                ("{webhook_endpoint_id}", &encoded_hook, "webhook endpoint not found"),
                ("{environment_id}", &encoded_environment, "environment not found"),
            ]).await?;
            assert!(swept >= 40, "only {swept} operations covered");
            // Actual child IDs and valid attack bodies cover paths whose
            // placeholder children/body would otherwise fail deserialization.
            for (method, path, body, message, public) in [
                (reqwest::Method::GET, format!("/vaults/{encoded_vault}/credentials/{}", encoded_id(credential_id, middle)), Value::Null, "vault not found", true),
                (reqwest::Method::POST, format!("/vaults/{encoded_vault}/credentials/{credential_id}"), json!({"auth":{"type":"static_bearer","token":"changed"}}), "vault not found", true),
                (reqwest::Method::DELETE, format!("/vaults/{encoded_vault}/credentials/{credential_id}"), Value::Null, "vault not found", true),
                (reqwest::Method::POST, format!("/agents/{encoded_agent}"), json!({"instructions":"changed"}), "agent not found", true),
                (reqwest::Method::POST, format!("/agents/sessions/{encoded_session}"), json!({"metadata":{"intruder":"here"}}), "session not found", true),
                (reqwest::Method::POST, format!("/agents/sessions/{encoded_session}/events"), json!({"events":[{"type":"message","role":"user","content":"intruder"}]}), "session not found", true),
                (reqwest::Method::POST, format!("/webhook_endpoints/{encoded_hook}"), json!({"name":"changed","url":"http://127.0.0.1:9/stolen"}), "webhook endpoint not found", true),
                (reqwest::Method::POST, format!("/agents/environments/{encoded_environment}/files"), json!({"type":"inline","path":workspace.path().join("intruder.txt"),"data":"aGFjaw=="}), "environment not found", true),
                (reqwest::Method::GET, format!("/skills/{encoded_skill}/versions/%31/content"), Value::Null, "skill not found", true),
                (reqwest::Method::GET, format!("/prototype/agents/{encoded_agent}"), Value::Null, "agent not found", false),
                (reqwest::Method::GET, format!("/sessions/{}", encoded_id(prototype_id, middle)), Value::Null, "session not found", false),
                (reqwest::Method::GET, format!("/sessions/{}/events", encoded_id(prototype_id, middle)), Value::Null, "session not found", false),
                (reqwest::Method::GET, format!("/sessions/{}/turns", encoded_id(prototype_id, middle)), Value::Null, "session not found", false),
                (reqwest::Method::POST, format!("/sessions/{}/input", encoded_id(prototype_id, middle)), json!({"input":"intruder"}), "session not found", false),
                (reqwest::Method::POST, format!("/sessions/{}/turns/t/cancel", encoded_id(prototype_id, middle)), json!({}), "session not found", false),
                (reqwest::Method::POST, format!("/sessions/{}/turns/t/tool-results", encoded_id(prototype_id, middle)), json!({"callId":"c","success":true,"output":"intruder"}), "session not found", false),
            ] {
                let response = client.request(method.clone(), format!("{base}{path}")).bearer_auth(&outsider).json(&body).send().await?;
                assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND, "{method} {path}");
                let actual: Value = response.json().await?;
                let expected = if public { json!({"error":{"message":message,"type":"invalid_request_error","param":null,"code":null}}) } else { json!({"error":message}) };
                assert_eq!(actual, expected, "{method} {path}");
            }
            // The same encoded IDs still resolve for their owner.
            let own = client.get(format!("{base}/files/{encoded_file}/content")).bearer_auth(TOKEN).send().await?.error_for_status()?.bytes().await?;
            assert_eq!(own.as_ref(), b"private file bytes");
            let own = request(&client, reqwest::Method::GET, &format!("{base}/agents/sessions/{encoded_session}"), Value::Null).await?;
            assert_eq!(own["id"], json!(session_id));
        }
        for (path, expected) in paths.iter().zip(before) {
            let actual = client.get(format!("{base}{path}")).bearer_auth(TOKEN).send().await?.error_for_status()?.bytes().await?;
            assert_eq!(actual, expected, "owner's resource changed: {path}");
        }
        assert!(!workspace.path().join("intruder.txt").exists());

        // Bad escapes, residual encoding, separators, UTF-8 failures, and
        // alternate slash/case spellings must never reach an owned resource.
        for path in [
            format!("/files/{file_id}%"), format!("/files/{file_id}%6"), format!("/files/{file_id}%GG"),
            format!("/files/{file_id}%2fcontent"), format!("/files/{file_id}%5ccontent"),
            format!("/files/%2566{}", &file_id[1..]), format!("/files/%ff{file_id}"),
            format!("/files/{file_id}/"), format!("/files//{file_id}"), format!("/Files/{file_id}"),
        ] {
            let response = client.get(format!("{base}{path}")).bearer_auth(&outsider).send().await?;
            assert!(matches!(response.status().as_u16(), 400 | 404), "{path}: {}", response.status());
        }
        Ok::<_, anyhow::Error>(())
    }).await?
}

#[tokio::test]
async fn prototype_sessions_reject_foreign_environments_at_create_and_turn_start()
-> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 60), async {
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let provider = create_mock_responses_server_repeating_assistant("unused").await;
        MockResponsesConfig::new(&provider.uri()).write(home.path())?;
        let directory = AbsolutePathBuf::from_absolute_path(data.path())?;
        let api = AgentsApi::new(backend(home.path()).await?, directory.clone(), TOKEN.into()).await?;
        let (base, _server) = capabilities::serve(&api).await?;
        api.configure_environments("prototype-ownership-environment-key".into(), format!("{}/registry", base.trim_end_matches("/v1")))?;
        let admin = Admin::open(directory.clone()).await?;
        let other = admin.create_project("Other").await?;
        let all: Vec<String> = codex_agents_api::projects::PERMISSIONS.iter().map(|&p| p.to_owned()).collect();
        let outsider = admin.create_key(&other.id, "", &all).await?.value;
        let client = reqwest::Client::new();
        let hosted_body = json!({"agent":{"model":"mock-model"},"environment":{"type":"self_hosted","workspace_directory":workspace.path()}});
        let owner = request(&client, reqwest::Method::POST, &format!("{base}/agents/sessions"), hosted_body.clone()).await?;
        let environment_id = owner["environment"]["id"].as_str().context("environment id")?;
        let send = |path: String, body: Value| client.post(format!("{base}{path}")).bearer_auth(&outsider).json(&body).send();
        let agent: Value = send("/agents".into(), json!({"model":"mock-model"})).await?.error_for_status()?.json().await?;
        let agent_id = agent["id"].as_str().context("agent id")?;
        let pool = codex_state::SqliteConfig::from_sqlite_home(directory)
            .open_read_write_pool(data.path().join("agents-api.sqlite").as_path()).await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE project_id = ?").bind(&other.id).fetch_one(&pool).await?;
        for id in [environment_id, "env_missing"] {
            let response = send("/sessions".into(), json!({"agentId":agent_id,"environment":{"type":"selfHosted","id":id,"cwd":workspace.path()}})).await?;
            assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
            assert_eq!(response.json::<Value>().await?, json!({"error":"environment not found"}));
        }
        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE project_id = ?").bind(&other.id).fetch_one(&pool).await?;
        assert_eq!(after, count, "rejected creations must not persist sessions");
        // A legitimate binding to this project's own environment remains valid.
        let own: Value = send("/agents/sessions".into(), hosted_body).await?.error_for_status()?.json().await?;
        let own_environment = own["environment"]["id"].as_str().context("own environment")?;
        let valid = send("/sessions".into(), json!({"agentId":agent_id,"environment":{"type":"selfHosted","id":own_environment,"cwd":workspace.path()}})).await?;
        assert_eq!(valid.status(), reqwest::StatusCode::CREATED);
        // Simulate an already-stored binding from before the fix. The caller
        // owns the session but its environment belongs to the other project.
        let stored: Value = send("/sessions".into(), json!({"agentId":agent_id,"environment":{"type":"none"}})).await?.error_for_status()?.json().await?;
        let id = stored["id"].as_str().context("stored session id")?;
        let foreign = json!({"type":"selfHosted","id":environment_id,"cwd":workspace.path()});
        sqlx::query("UPDATE sessions SET data = json_set(data, '$.environment', json(?)) WHERE id = ? AND project_id = ?")
            .bind(foreign.to_string()).bind(id).bind(&other.id).execute(&pool).await?;
        let before: (String, Option<String>) = sqlx::query_as("SELECT data, thread_id FROM sessions WHERE id = ?").bind(id).fetch_one(&pool).await?;
        let response = send(format!("/sessions/{id}/input"), json!({"input":"run a command"})).await?;
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        assert_eq!(response.json::<Value>().await?, json!({"error":"environment not found"}));
        let after: (String, Option<String>) = sqlx::query_as("SELECT data, thread_id FROM sessions WHERE id = ?").bind(id).fetch_one(&pool).await?;
        assert_eq!(after, before);
        assert!(after.1.is_none(), "rejection must precede worker thread creation");
        assert_eq!(request(&client, reqwest::Method::GET, &format!("{base}/agents/environments/{environment_id}"), Value::Null).await?["status"], json!("pending"));
        Ok::<_, anyhow::Error>(())
    }).await?
}

#[tokio::test]
async fn prototype_local_environments_require_the_operator_token() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 60), async {
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let provider = create_mock_responses_server_repeating_assistant("unused").await;
        MockResponsesConfig::new(&provider.uri()).write(home.path())?;
        let directory = AbsolutePathBuf::from_absolute_path(data.path())?;
        let api =
            AgentsApi::new(backend(home.path()).await?, directory.clone(), TOKEN.into()).await?;
        let (base, _server) = capabilities::serve(&api).await?;
        let admin = Admin::open(directory.clone()).await?;
        let other = admin.create_project("Other").await?;
        let all: Vec<String> = codex_agents_api::projects::PERMISSIONS
            .iter()
            .map(|&p| p.to_owned())
            .collect();
        let outsider = admin.create_key(&other.id, "", &all).await?.value;
        let default_key = admin
            .create_key(
                codex_agents_api::projects::DEFAULT_PROJECT,
                "non-operator",
                &all,
            )
            .await?
            .value;
        let client = reqwest::Client::new();
        let post = |secret: &str, path: &str, body: Value| {
            client
                .post(format!("{base}{path}"))
                .bearer_auth(secret)
                .json(&body)
                .send()
        };
        let operator_agent = request(
            &client,
            reqwest::Method::POST,
            &format!("{base}/agents"),
            json!({"model":"mock-model"}),
        )
        .await?;
        let other_agent: Value = post(&outsider, "/agents", json!({"model":"mock-model"}))
            .await?
            .error_for_status()?
            .json()
            .await?;
        let local = json!({"type":"local","cwd":home.path()});
        let pool = codex_state::SqliteConfig::from_sqlite_home(directory)
            .open_read_write_pool(data.path().join("agents-api.sqlite").as_path())
            .await?;
        for (secret, agent) in [(&outsider, &other_agent), (&default_key, &operator_agent)] {
            let response = post(
                secret,
                "/sessions",
                json!({"agentId":agent["id"],"environment":local}),
            )
            .await?;
            assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
            assert_eq!(
                response.json::<Value>().await?,
                json!({"error":"local environments require the operator token"})
            );
        }
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 0, "rejected local sessions must not be persisted");
        let accepted = post(
            TOKEN,
            "/sessions",
            json!({"agentId":operator_agent["id"],"environment":local}),
        )
        .await?;
        assert_eq!(accepted.status(), reqwest::StatusCode::CREATED);
        let accepted: Value = accepted.json().await?;
        let local_id = accepted["id"].as_str().context("local session id")?;
        // A default-project key is not an operator, even for existing sessions.
        let response = post(
            &default_key,
            &format!("/sessions/{local_id}/input"),
            json!({"input":"read host files"}),
        )
        .await?;
        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
        assert_eq!(
            response.json::<Value>().await?,
            json!({"error":"local environments require the operator token"})
        );
        // Previously stored local bindings in another project are also refused.
        let stored: Value = post(
            &outsider,
            "/sessions",
            json!({"agentId":other_agent["id"],"environment":{"type":"none"}}),
        )
        .await?
        .error_for_status()?
        .json()
        .await?;
        let id = stored["id"].as_str().context("stored session id")?;
        sqlx::query(
            "UPDATE sessions SET data = json_set(data, '$.environment', json(?)) WHERE id = ?",
        )
        .bind(local.to_string())
        .bind(id)
        .execute(&pool)
        .await?;
        let response = post(
            &outsider,
            &format!("/sessions/{id}/input"),
            json!({"input":"read host files"}),
        )
        .await?;
        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
        assert_eq!(
            response.json::<Value>().await?,
            json!({"error":"local environments require the operator token"})
        );
        let threads: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE thread_id IS NOT NULL")
                .fetch_one(&pool)
                .await?;
        assert_eq!(threads, 0);
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
