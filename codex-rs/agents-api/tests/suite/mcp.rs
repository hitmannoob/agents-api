use super::*;
use axum::response::IntoResponse;
use pretty_assertions::assert_eq;
use std::sync::Mutex;

/// A minimal HTTP MCP server with an allowed `lookup` tool and a `secret` tool
/// that no session selects. It records each tool call, headers, and metadata.
pub(super) async fn mcp_server() -> anyhow::Result<(
    String,
    Arc<Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<std::io::Result<()>>,
)> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&calls);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/mcp", listener.local_addr()?);
    let router = axum::Router::new().route("/mcp", axum::routing::post(move |headers: axum::http::HeaderMap, axum::Json(message): axum::Json<Value>| {
        let recorded = Arc::clone(&recorded);
        async move {
            let result = match message["method"].as_str() {
                Some("initialize") => json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "fixture", "version": "1"}}),
                Some("notifications/initialized") => return axum::http::StatusCode::ACCEPTED.into_response(),
                Some("tools/list") => json!({"tools": [
                    {"name": "lookup", "description": "Allowed lookup", "inputSchema": {"type": "object", "properties": {}}},
                    {"name": "secret", "description": "Unselected lookup", "inputSchema": {"type": "object", "properties": {}}}
                ]}),
                Some("tools/call") => {
                    let tenant = headers.get("x-tenant").and_then(|value| value.to_str().ok()).map(str::to_owned);
                    let authorization = headers.get("authorization").and_then(|value| value.to_str().ok()).map(str::to_owned);
                    recorded.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(json!({"tool": message["params"]["name"], "tenant": tenant,
                            "authorization": authorization, "meta": message["params"]["_meta"]}));
                    json!({"content": [{"type": "text", "text": "mcp-result-731"}]})
                }
                _ => json!({}),
            };
            axum::Json(json!({"jsonrpc": "2.0", "id": message["id"], "result": result})).into_response()
        }
    }));
    Ok((
        url,
        calls,
        tokio::spawn(async move { axum::serve(listener, router).await }),
    ))
}

fn mcp(label: &str, transport: Value, extra: Value) -> Value {
    let mut tool = json!({"type":"mcp","server_label":label,"transport":transport});
    if let (Some(tool), Value::Object(extra)) = (tool.as_object_mut(), extra) {
        tool.extend(extra);
    }
    tool
}

fn session(tools: Value, input: &str) -> Value {
    json!({"agent":{"model":"mock-model","tools":tools},"environment":{"type":"none"},"input":input})
}

fn tool_names(request: &Value) -> String {
    request["tools"].to_string()
}

#[tokio::test]
async fn public_mcp_servers_are_scoped_filtered_and_egress_checked() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 90), async {
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let (mcp_url, calls, mcp_task) = mcp_server().await?;
        let provider = create_mock_responses_server_repeating_assistant("finished").await;
        Mock::given(body_string_contains("use-mcp"))
            .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_raw(capabilities::call("mcp__warehouse.lookup", "mcp-call"), "text/event-stream"))
            .with_priority(/*p*/ 1).up_to_n_times(/*n*/ 1).mount(&provider).await;
        MockResponsesConfig::new(&provider.uri()).with_root_config("features.plugins = false").with_extra_config(&format!(
            "[mcp_servers.configured]\nurl = {mcp_url:?}\nenabled = false\n"
        )).write(home.path())?;
        let start = || async {
            let api = AgentsApi::new(backend(home.path()).await?, AbsolutePathBuf::from_absolute_path(data.path())?, TOKEN.into()).await?;
            api.allow_mcp_hosts(["127.0.0.1".to_string()]);
            Ok::<_, anyhow::Error>(api)
        };
        let api = start().await?;
        let (base, server) = capabilities::serve(&api).await?;
        let client = reqwest::Client::new();
        let sessions = format!("{base}/agents/sessions");

        // Unreachable or unsupported servers are rejected before any session exists.
        let http = |url: &str| json!({"type":"http","server_url":url});
        for (tools, reason) in [
            (json!([mcp("internal", http("https://10.0.0.1/mcp"), json!({}))]), "private address"),
            (json!([mcp("metadata", http("https://169.254.169.254/latest"), json!({}))]), "link-local metadata address"),
            (json!([mcp("loopback", http("https://localhost:1/mcp"), json!({}))]), "loopback name"),
            (json!([mcp("plain", http("http://93.184.216.34/mcp"), json!({}))]), "plain http"),
            (json!([mcp("configured", http(&mcp_url), json!({}))]), "worker server label"),
            (json!([mcp("local", json!({"type":"stdio","command":"server","cwd":"/"}), json!({}))]), "stdio"),
            (json!([mcp("remote", http(&mcp_url), json!({"connection_origin":"environment"}))]), "environment origin"),
            (json!([mcp("vaulted", http(&mcp_url), json!({"credential_id":"cred_1"}))]), "vault credential"),
            (json!([mcp("secret", json!({"type":"http","server_url":mcp_url,"headers":{"Authorization":"Bearer x"}}), json!({}))]), "authorization header"),
        ] {
            let response = client.post(&sessions).bearer_auth(TOKEN).json(&session(tools, "rejected")).send().await?;
            assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST, "{reason}");
        }
        assert_eq!(request(&client, reqwest::Method::GET, &sessions, Value::Null).await?["data"], json!([]));

        // An allowed server exposes only its selected tools, with its headers,
        // and only to the session that configured it.
        let warehouse = json!([mcp("warehouse", json!({"type":"http","server_url":mcp_url,"headers":{"x-tenant":"tenant-a"}}), json!({"allowed_tools":["lookup"]}))]);
        let scoped = request(&client, reqwest::Method::POST, &sessions, session(warehouse, "use-mcp")).await?;
        let plain = request(&client, reqwest::Method::POST, &sessions, session(json!([]), "no-mcp")).await?;
        let scoped_url = format!("{sessions}/{}", scoped["id"].as_str().context("scoped id")?);
        idle(&client, &scoped_url, /*expected_turns*/ 1).await?;
        idle(&client, &format!("{sessions}/{}", plain["id"].as_str().context("plain id")?), /*expected_turns*/ 1).await?;
        let recorded = calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(json!({"tool":recorded[0]["tool"],"tenant":recorded[0]["tenant"],"authorization":recorded[0]["authorization"]}), json!({"tool":"lookup","tenant":"tenant-a","authorization":null}));
        let items = request(&client, reqwest::Method::GET, &format!("{scoped_url}/items?order=asc"), Value::Null).await?;
        let call = items["data"].as_array().context("items")?.iter().find(|item| item["type"] == "mcp_call").context("mcp_call item")?;
        assert_eq!(json!({"server_label":call["server_label"],"name":call["name"],"arguments":call["arguments"],"output":call["output"],"error":call["error"],"status":call["status"]}),
            json!({"server_label":"warehouse","name":"lookup","arguments":{},"output":[{"type":"text","text":"mcp-result-731"}],"error":null,"status":"completed"}));
        let captures = provider.received_requests().await.context("captures")?;
        let bodies = captures.iter().filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok()).collect::<Vec<_>>();
        let with_mcp = bodies.iter().find(|body| body.to_string().contains("use-mcp")).context("scoped request")?;
        let without_mcp = bodies.iter().find(|body| body.to_string().contains("no-mcp")).context("plain request")?;
        assert!(tool_names(with_mcp).contains("lookup") && !tool_names(with_mcp).contains("secret"), "{}", tool_names(with_mcp));
        // Resource tools would reach past `allowed_tools`, so none are offered.
        assert!(!tool_names(with_mcp).contains("mcp_resource"), "{}", tool_names(with_mcp));
        assert!(!tool_names(without_mcp).contains("warehouse"), "{}", tool_names(without_mcp));
        assert!(bodies.iter().any(|body| body["input"].to_string().contains("mcp-result-731")));
        // A required server that cannot initialize fails the turn, not the request.
        let required = json!([mcp("down", json!({"type":"http","server_url":"http://127.0.0.1:1/mcp"}), json!({"required":true}))]);
        let down = request(&client, reqwest::Method::POST, &sessions, session(required, "required-down")).await?;
        let down_url = format!("{sessions}/{}", down["id"].as_str().context("down id")?);
        let turns = request(&client, reqwest::Method::GET, &format!("{down_url}/turns"), Value::Null).await?;
        let turn = &turns["data"][0];
        assert_eq!(json!({"session":down["status"],"turns":turns["data"].as_array().map(Vec::len),"status":turn["status"],"error":turn["error"]}),
            json!({"session":"idle","turns":1,"status":"failed","error":{"code":"connection_failed","message":"a required MCP server failed to initialize"}}));
        server.abort();
        api.shutdown().await?;

        // Cold resume re-establishes the session's server.
        let api = start().await?;
        let (base, server) = capabilities::serve(&api).await?;
        let scoped_url = format!("{base}/agents/sessions/{}", scoped["id"].as_str().context("scoped id")?);
        let response = client.post(format!("{scoped_url}/events")).bearer_auth(TOKEN)
            .json(&json!({"events":[{"type":"agent.session.input.message","input":"after-restart"}]})).send().await?;
        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
        idle(&client, &scoped_url, /*expected_turns*/ 2).await?;
        let captures = provider.received_requests().await.context("captures")?;
        let resumed = captures.iter().rev().filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
            .find(|body| body.to_string().contains("after-restart")).context("resumed request")?;
        assert!(tool_names(&resumed).contains("lookup") && !tool_names(&resumed).contains("secret"), "{}", tool_names(&resumed));
        server.abort();
        api.shutdown().await?;
        mcp_task.abort();
        Ok::<_, anyhow::Error>(())
    }).await?
}

#[tokio::test]
async fn mcp_inline_authorization_and_metadata_survive_session_snapshots() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 120), async {
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let (mcp_url, calls, mcp_task) = mcp_server().await?;
        let provider = create_mock_responses_server_repeating_assistant("finished").await;
        for (input, call_id) in [
            ("inline-first", "inline-call-1"),
            ("inline-basic", "inline-call-2"),
            ("inline-resume", "inline-call-3"),
            ("inline-null", "inline-call-4"),
        ] {
            Mock::given(move |request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap_or_default();
                body["input"]
                    .as_array()
                    .and_then(|items| items.last())
                    .is_some_and(|item| item.to_string().contains(input))
            })
            .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_raw(
                capabilities::call("mcp__warehouse.lookup", call_id),
                "text/event-stream",
            ))
            .with_priority(/*p*/ 1)
            .up_to_n_times(/*n*/ 1)
            .mount(&provider)
            .await;
        }
        MockResponsesConfig::new(&provider.uri())
            .with_root_config("features.plugins = false")
            .write(home.path())?;
        let start = |configured: bool| {
            let home = home.path().to_path_buf();
            let data = data.path().to_path_buf();
            async move {
                let api = AgentsApi::new(
                    backend(&home).await?,
                    AbsolutePathBuf::from_absolute_path(&data)?,
                    TOKEN.into(),
                )
                .await?;
                api.allow_mcp_hosts(["127.0.0.1".to_owned()]);
                if configured {
                    api.configure_vault("inline-mcp-secrets-passphrase-for-tests".into())
                        .await?;
                }
                Ok::<_, anyhow::Error>(api)
            }
        };
        let api = start(false).await?;
        let (base, server) = capabilities::serve(&api).await?;
        let client = reqwest::Client::builder()
            .default_headers(
                [("openai-beta".parse()?, "agents=v1".parse()?)]
                    .into_iter()
                    .collect(),
            )
            .build()?;
        let agents = format!("{base}/agents");
        let sessions = format!("{base}/agents/sessions");
        let metadata =
            json!({"tenant":"tenant-a","context":{"region":"west","flags":[true,2,"ok"]}});
        let tools = |authorization: Value| {
            json!([mcp(
                "warehouse",
                json!({"type":"http","server_url":mcp_url,
            "authorization":authorization,"headers":{"x-tenant":"tenant-a"}}),
                json!({"allowed_tools":["lookup"],"request_metadata":metadata})
            )])
        };
        let agent = json!({"model":"mock-model","tools":tools(json!("inline-token-731"))});
        for (url, body) in [
            (&agents, agent.clone()),
            (
                &sessions,
                json!({"agent":agent,"environment":{"type":"none"},"input":"disabled"}),
            ),
        ] {
            let response = client
                .post(url)
                .bearer_auth(TOKEN)
                .json(&body)
                .send()
                .await?;
            assert_eq!(response.status(), reqwest::StatusCode::NOT_IMPLEMENTED);
            assert!(
                response.json::<Value>().await?["error"]["message"]
                    .as_str()
                    .context("error")?
                    .contains("vault passphrase")
            );
        }
        api.configure_vault("inline-mcp-secrets-passphrase-for-tests".into())
            .await?;

        // Conflicts and null metadata fail before storing an agent or a session.
        let mut conflict = tools(json!("inline-token-731"));
        conflict[0]["credential_id"] = json!("cred_explicit");
        let mut null_metadata = tools(Value::Null);
        null_metadata[0]["request_metadata"] = json!({"context":{"flags":[true,null]}});
        for (tools, reason) in [(conflict, "credential_id"), (null_metadata, "JSON null")] {
            let agent = json!({"model":"mock-model","tools":tools});
            for (url, body) in [
                (&agents, agent.clone()),
                (
                    &sessions,
                    json!({"agent":agent,"environment":{"type":"none"},"input":"rejected"}),
                ),
            ] {
                let response = client
                    .post(url)
                    .bearer_auth(TOKEN)
                    .json(&body)
                    .send()
                    .await?;
                assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
                assert!(
                    response.json::<Value>().await?["error"]["message"]
                        .as_str()
                        .context("error")?
                        .contains(reason)
                );
            }
        }

        let saved = request(&client, reqwest::Method::POST, &agents, agent).await?;
        let agent_id = saved["id"].as_str().context("agent id")?.to_owned();
        let agent_url = format!("{agents}/{agent_id}");
        let mut responses = vec![saved];
        responses.push(request(&client, reqwest::Method::GET, &agent_url, Value::Null).await?);
        responses.push(request(&client, reqwest::Method::GET, &agents, Value::Null).await?);
        // An update without tools preserves the secret.
        responses.push(
            request(
                &client,
                reqwest::Method::POST,
                &agent_url,
                json!({"instructions":"Use the lookup tool."}),
            )
            .await?,
        );
        let pool = codex_state::SqliteConfig::from_sqlite_home(
            AbsolutePathBuf::from_absolute_path(data.path())?,
        )
        .open_read_write_pool(data.path().join("agents-api.sqlite").as_path())
        .await?;
        let stored: String = sqlx::query_scalar("SELECT data FROM agents WHERE id = ?")
            .bind(&agent_id)
            .fetch_one(&pool)
            .await?;
        pool.close().await;
        assert!(!stored.contains("inline-token-731"), "{stored}");
        let stored: Value = serde_json::from_str(&stored)?;
        assert!(
            stored["config"]["tools"][0]["transport"]
                .get("authorization")
                .is_none()
        );

        // Inline authorization wins even when auto-matching would be ambiguous.
        let vault = request(
            &client,
            reqwest::Method::POST,
            &format!("{base}/vaults"),
            json!({"name":"warehouse"}),
        )
        .await?;
        let vault_url = format!(
            "{base}/vaults/{}",
            vault["id"].as_str().context("vault id")?
        );
        for token in ["vault-token-unused-1", "vault-token-unused-2"] {
            request(
                &client,
                reqwest::Method::POST,
                &format!("{vault_url}/credentials"),
                json!({"name":"warehouse",
                "auth":{"type":"static_bearer","token":token,"mcp_server_url":mcp_url}}),
            )
            .await?;
        }
        let first = request(
            &client,
            reqwest::Method::POST,
            &sessions,
            json!({"agent_id":agent_id,"vault_ids":[vault["id"]],
            "environment":{"type":"none"},"input":"inline-first"}),
        )
        .await?;
        let first_id = first["id"].as_str().context("session id")?.to_owned();
        let first_url = format!("{sessions}/{first_id}");
        responses.push(first);
        responses.push(idle(&client, &first_url, /*expected_turns*/ 1).await?);
        let captured = calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0]["authorization"], "Bearer inline-token-731");
        assert_eq!(captured[0]["tenant"], "tenant-a");
        for key in ["tenant", "context"] {
            assert_eq!(captured[0]["meta"][key], metadata[key]);
        }
        assert_eq!(responses[0]["tools"][0]["request_metadata"], metadata);
        assert_eq!(
            responses.last().context("session response")?["agent"]["tools"][0]["request_metadata"],
            metadata
        );

        // Agent rotation changes new sessions; existing sessions keep their snapshot.
        responses.push(
            request(
                &client,
                reqwest::Method::POST,
                &agent_url,
                json!({"tools":tools(json!("Basic inline-basic-732"))}),
            )
            .await?,
        );
        let second = request(
            &client,
            reqwest::Method::POST,
            &sessions,
            json!({"agent_id":agent_id,"vault_ids":[vault["id"]],
            "environment":{"type":"none"},"input":"inline-basic"}),
        )
        .await?;
        let second_id = second["id"]
            .as_str()
            .context("second session id")?
            .to_owned();
        responses.push(second);
        responses.push(
            idle(
                &client,
                &format!("{sessions}/{second_id}"),
                /*expected_turns*/ 1,
            )
            .await?,
        );
        assert_eq!(
            calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)[1]["authorization"],
            "Basic inline-basic-732"
        );
        request(&client, reqwest::Method::DELETE, &agent_url, Value::Null).await?;
        server.abort();
        api.shutdown().await?;

        // Cold resume reloads the session secret after the agent was deleted.
        let api = start(true).await?;
        let (base, server) = capabilities::serve(&api).await?;
        let first_url = format!("{base}/agents/sessions/{first_id}");
        let response = client
            .post(format!("{first_url}/events"))
            .bearer_auth(TOKEN)
            .json(
                &json!({"events":[{"type":"agent.session.input.message","input":"inline-resume"}]}),
            )
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
        responses.push(idle(&client, &first_url, /*expected_turns*/ 2).await?);
        let captured = calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(captured.len(), 3);
        assert_eq!(captured[2]["authorization"], "Bearer inline-token-731");
        assert_eq!(captured[2]["meta"]["context"], metadata["context"]);

        // Null authorization needs no secret and sends no Authorization header.
        let null = request(
            &client,
            reqwest::Method::POST,
            &format!("{base}/agents/sessions"),
            session(tools(Value::Null), "inline-null"),
        )
        .await?;
        let null_url = format!(
            "{base}/agents/sessions/{}",
            null["id"].as_str().context("null session id")?
        );
        responses.push(null);
        responses.push(idle(&client, &null_url, /*expected_turns*/ 1).await?);
        assert_eq!(
            calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)[3]["authorization"],
            Value::Null
        );
        for response in &responses {
            let agent = response
                .get("agent")
                .or_else(|| response.pointer("/data/0"))
                .unwrap_or(response);
            assert_eq!(
                agent["tools"][0]["transport"]["headers"],
                json!({"x-tenant":"tenant-a"})
            );
            let response = response.to_string();
            assert!(
                !response.contains("inline-token-731") && !response.contains("inline-basic-732"),
                "{response}"
            );
            assert!(!response.contains("\"authorization\""), "{response}");
        }
        for token in ["inline-token-731", "inline-basic-732"] {
            assert_eq!(
                vaults::files_containing(data.path(), token)?,
                Vec::<String>::new()
            );
            assert_eq!(
                vaults::files_containing(home.path(), token)?,
                Vec::<String>::new()
            );
            let captures = provider.received_requests().await.context("captures")?;
            assert!(
                !captures
                    .iter()
                    .any(|request| String::from_utf8_lossy(&request.body).contains(token)),
                "{token} sent to the model"
            );
        }
        for url in [
            first_url,
            format!("{base}/agents/sessions/{second_id}"),
            null_url,
        ] {
            request(&client, reqwest::Method::DELETE, &url, Value::Null).await?;
        }
        server.abort();
        api.shutdown().await?;
        mcp_task.abort();
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
