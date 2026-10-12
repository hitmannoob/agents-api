use super::*;
use codex_agents_api::projects::Admin;
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
