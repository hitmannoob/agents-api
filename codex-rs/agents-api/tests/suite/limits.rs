use super::*;
use pretty_assertions::assert_eq;

/// A function tool whose schema is a few KiB, like real definitions.
fn tool(index: usize) -> Value {
    let properties: serde_json::Map<String, Value> = (0..20)
        .map(|field| (format!("field_{field}"), json!({"type":"string","description":format!("Field {field} of tool {index}. {}", "Detail. ".repeat(10))})))
        .collect();
    json!({"type":"function","name":format!("tool_{index}"),"description":"A tool with a large schema.","parameters":{"type":"object","properties":properties}})
}

#[tokio::test]
async fn agents_carry_long_instructions_many_tools_and_long_input() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 60), async {
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let provider = create_mock_responses_server_repeating_assistant("finished").await;
        MockResponsesConfig::new(&provider.uri()).with_root_config("features.plugins = false").write(home.path())?;
        let api = AgentsApi::new(backend(home.path()).await?, AbsolutePathBuf::from_absolute_path(data.path())?, TOKEN.into()).await?;
        let (base, server) = capabilities::serve(&api).await?;
        let client = reqwest::Client::new();
        let post = |path: &str, body: &Value| client.post(format!("{base}{path}")).bearer_auth(TOKEN).json(body).send();
        let error = |message: &str| json!({"error":{"message":message,"type":"invalid_request_error","param":null,"code":null}});

        // 200 KB of instructions and 128 tools of a few KiB each fit; one
        // tool or one instruction byte more does not.
        let instructions = format!("{}END-OF-INSTRUCTIONS", "Follow the house rules. ".repeat(8_000));
        let tools: Vec<Value> = (0..128).map(tool).collect();
        assert!(tools.iter().all(|tool| tool.to_string().len() > 2048) && instructions.len() > 190_000);
        let agent = request(&client, reqwest::Method::POST, &format!("{base}/agents"),
            json!({"model":"mock-model","instructions":instructions,"tools":tools})).await?;
        let mut too_many = tools.clone();
        too_many.push(tool(128));
        let response = post("/agents", &json!({"model":"mock-model","tools":too_many})).await?;
        assert_eq!((response.status(), response.json::<Value>().await?), (reqwest::StatusCode::BAD_REQUEST, error("invalid or oversized agent capabilities")));
        let response = post("/agents", &json!({"model":"mock-model","instructions":"i".repeat((1 << 20) + 1)})).await?;
        assert_eq!((response.status(), response.json::<Value>().await?), (reqwest::StatusCode::BAD_REQUEST, error("model must be 1-256 bytes; instructions must be at most 1 MiB")));

        // A 500 KB message runs, and the model receives all of it.
        let input = format!("{}END-OF-INPUT", "Please read this document. ".repeat(20_000));
        let session = request(&client, reqwest::Method::POST, &format!("{base}/agents/sessions"),
            json!({"agent_id":agent["id"],"environment":{"type":"none"},"input":input})).await?;
        let url = format!("{base}/agents/sessions/{}", session["id"].as_str().context("session id")?);
        idle(&client, &url, /*expected_turns*/ 1).await?;
        let captures = provider.received_requests().await.context("captures")?;
        let sent: Value = serde_json::from_slice(&captures.iter().find(|r| r.url.path().ends_with("/responses")).context("request")?.body)?;
        let functions = sent["tools"].as_array().into_iter().flatten().filter(|tool| tool["type"] == "function").count();
        assert_eq!((sent.to_string().contains("END-OF-INSTRUCTIONS"), sent.to_string().contains("END-OF-INPUT"), functions), (true, true, 128));
        let message = |text: String| json!({"events":[{"type":"agent.session.input.message","input":text}]});
        let response = post(&format!("{}/events", url.trim_start_matches(&base)), &message("x".repeat((1 << 20) + 1))).await?;
        assert_eq!((response.status(), response.json::<Value>().await?),
            (reqwest::StatusCode::BAD_REQUEST, error("input must contain non-blank text or an image, with at most 1048576 text bytes")));

        // Idempotency keys keep a digest, not the request, and a retry that
        // orders its fields differently still replays.
        let events = format!("{url}/events");
        let keyed = |body: Value| client.post(&events).bearer_auth(TOKEN).header("Idempotency-Key", "large-retry").json(&body).send();
        let first = keyed(json!({"events":[{"type":"agent.session.input.message","input":"again"}]})).await?;
        assert_eq!(first.status(), reqwest::StatusCode::ACCEPTED);
        let reordered = keyed(serde_json::from_str(r#"{"events":[{"input":"again","type":"agent.session.input.message"}]}"#)?).await?;
        assert_eq!(reordered.status(), reqwest::StatusCode::ACCEPTED);
        let different = keyed(json!({"events":[{"type":"agent.session.input.message","input":"other"}]})).await?;
        assert_eq!(different.status(), reqwest::StatusCode::BAD_REQUEST);
        idle(&client, &url, /*expected_turns*/ 2).await?;
        let pool = codex_state::SqliteConfig::from_sqlite_home(AbsolutePathBuf::from_absolute_path(data.path())?)
            .open_read_write_pool(data.path().join("agents-api.sqlite").as_path()).await?;
        let stored: String = sqlx::query_scalar("SELECT request FROM input_requests WHERE key = 'large-retry'").fetch_one(&pool).await?;
        assert!(stored.starts_with("sha256:") && stored.len() == 7 + 64, "{stored}");
        pool.close().await;

        // The prototype's routes keep their 16 KiB bodies.
        let response = post("/prototype/agents", &json!({"model":"mock-model","instructions":"p".repeat(20_000)})).await?;
        assert_eq!(response.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);

        server.abort();
        api.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    }).await?
}
