use super::*;
use codex_utils_cargo_bin::cargo_bin;
use codex_utils_cargo_bin::find_resource;
use pretty_assertions::assert_eq;

fn install_code_mode_host() -> anyhow::Result<()> {
    // Production ships the host beside codex-app-server; the in-process worker
    // looks beside this test executable instead.
    let executable = std::env::current_exe()?;
    let directory = executable.parent().context("test executable directory")?;
    let host = directory.join(format!(
        "codex-code-mode-host{}",
        std::env::consts::EXE_SUFFIX
    ));
    if !host.exists() {
        let temporary = tempfile::NamedTempFile::new_in(directory)?;
        std::fs::copy(cargo_bin("codex-code-mode-host")?, temporary.path())?;
        temporary.persist(host)?;
    }
    Ok(())
}

async fn pending(client: &reqwest::Client, url: &str) -> anyhow::Result<Value> {
    loop {
        let session = request(client, reqwest::Method::GET, url, Value::Null).await?;
        match session["status"].as_str() {
            Some("requires_action") => {
                assert_eq!(
                    session["required_actions"].as_array().map(Vec::len),
                    Some(1)
                );
                return Ok(session["required_actions"][0].clone());
            }
            Some("idle" | "failed") => {
                let turns = request(
                    client,
                    reqwest::Method::GET,
                    &format!("{url}/turns"),
                    Value::Null,
                )
                .await?;
                anyhow::bail!("the nested function was never called: {session}; turns={turns}")
            }
            _ => tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await,
        }
    }
}

#[tokio::test]
async fn programmatic_tool_calls_reach_the_application_and_respect_the_model() -> anyhow::Result<()>
{
    tokio::time::timeout(Duration::from_secs(/*secs*/ 90), async {
        install_code_mode_host()?;
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let provider = create_mock_responses_server_repeating_assistant("shipped").await;
        for (marker, call_id) in [("programmatic-order", "exec-order"), ("programmatic-cancel", "exec-cancel")] {
            Mock::given(body_string_contains(marker))
                .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_raw(sse(&[
                    json!({"type":"response.created","response":{"id":call_id}}),
                    json!({"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":call_id,"name":"exec",
                        "input":"const result = await tools.lookup_order({id: \"A1\"}); text(JSON.stringify(result));"}}),
                    json!({"type":"response.completed","response":{"id":call_id,"status":"completed","output":[]}}),
                ]), "text/event-stream"))
                .with_priority(/*p*/ 1).up_to_n_times(/*n*/ 1).mount(&provider).await;
        }
        let mut catalog: Value = serde_json::from_str(&std::fs::read_to_string(find_resource!("tests/fixtures/model_catalog.json")?)?)?;
        for (slug, mode) in [("code-only-model", "code_mode_only"), ("direct-model", "direct")] {
            let mut model = catalog["models"][0].clone();
            model["slug"] = json!(slug);
            model["tool_mode"] = json!(mode);
            catalog["models"].as_array_mut().context("models")?.push(model);
        }
        let catalog_path = home.path().join("models.json");
        std::fs::write(&catalog_path, catalog.to_string())?;
        MockResponsesConfig::new(&provider.uri()).with_root_config(&format!(
            "features.plugins = false\nfeatures.code_mode = true\nmodel_catalog_json = {}", serde_json::to_string(&catalog_path)?
        )).write(home.path())?;
        let api = AgentsApi::new(backend(home.path()).await?, AbsolutePathBuf::from_absolute_path(data.path())?, TOKEN.into()).await?;
        let (base, server) = capabilities::serve(&api).await?;
        let client = reqwest::Client::new();
        let sessions = format!("{base}/agents/sessions");

        for (model, enabled, message) in [
            ("code-only-model", false, "selected model calls tools from code; programmatic_tool_calling cannot be disabled"),
            ("direct-model", true, "selected model does not support programmatic tool calling"),
        ] {
            let refused = client.post(&sessions).bearer_auth(TOKEN).json(&json!({
                "agent":{"model":model,"tools":[{"type":"programmatic_tool_calling","enabled":enabled}]},
                "environment":{"type":"none"},"input":"refused-before-session"
            })).send().await?;
            assert_eq!((refused.status(), refused.json::<Value>().await?["error"]["message"].clone()),
                (reqwest::StatusCode::BAD_REQUEST, json!(message)));
        }
        assert_eq!(request(&client, reqwest::Method::GET, &sessions, Value::Null).await?["data"], json!([]));
        assert!(provider.received_requests().await.context("captures")?.is_empty());

        let function = json!({"type":"function","name":"lookup_order","description":"Look up an order's status.",
            "parameters":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}});
        let tools = json!([{"type":"programmatic_tool_calling","enabled":true}, function]);
        let session = request(&client, reqwest::Method::POST, &sessions, json!({
            "agent":{"model":"mock-model","tools":tools},"environment":{"type":"none"},"input":"programmatic-order"
        })).await?;
        let url = format!("{sessions}/{}", session["id"].as_str().context("session id")?);
        let action = pending(&client, &url).await?;
        assert_eq!(json!({"type":action["type"],"name":action["name"],"arguments":action["arguments"]}),
            json!({"type":"function_call","name":"lookup_order","arguments":{"id":"A1"}}));
        assert!(action["call_id"].as_str().context("call id")?.starts_with("exec-"));
        let submitted = client.post(format!("{url}/events")).bearer_auth(TOKEN).json(&json!({"events":[{
            "type":"agent.session.input.tool_result","turn_id":action["turn_id"],"call_id":action["call_id"],"success":true,"output":"in transit"
        }]})).send().await?;
        assert_eq!(submitted.status(), reqwest::StatusCode::ACCEPTED);
        idle(&client, &url, /*expected_turns*/ 1).await?;

        let captures = provider.received_requests().await.context("captures")?;
        let bodies = captures.iter().filter(|r| r.url.path().ends_with("/responses"))
            .map(|r| serde_json::from_slice::<Value>(&r.body)).collect::<Result<Vec<_>, _>>()?;
        assert_eq!(bodies.len(), 2);
        assert!(bodies[0]["tools"].as_array().context("tools")?.iter()
            .any(|tool| tool["type"] == "custom" && tool["name"] == "exec"));
        let output = bodies[1]["input"].as_array().context("follow-up input")?.iter()
            .find(|item| item["type"] == "custom_tool_call_output" && item["call_id"] == "exec-order")
            .context("exec output")?;
        assert!(output["output"].to_string().contains("in transit"), "{output}");
        let items = request(&client, reqwest::Method::GET, &format!("{url}/items?order=asc"), Value::Null).await?;
        let items = items["data"].as_array().context("items")?;
        let call = items.iter().find(|item| item["type"] == "function_call" && item["call_id"] == action["call_id"]).context("function call item")?;
        assert_eq!((&call["name"], &call["arguments"]), (&json!("lookup_order"), &json!({"id":"A1"})));
        let output = items.iter().find(|item| item["type"] == "function_call_output" && item["call_id"] == action["call_id"]).context("function output item")?;
        assert_eq!((&output["status"], &output["output"]), (&json!("completed"), &json!("in transit")));

        // An agent without the capability resets the worker's code-mode default.
        let plain = request(&client, reqwest::Method::POST, &sessions, json!({
            "agent":{"model":"mock-model","tools":[function]},"environment":{"type":"none"},"input":"plain-order"
        })).await?;
        idle(&client, &format!("{sessions}/{}", plain["id"].as_str().context("plain id")?), /*expected_turns*/ 1).await?;
        let captures = provider.received_requests().await.context("captures")?;
        let plain: Value = serde_json::from_slice(&captures.iter().find(|r| String::from_utf8_lossy(&r.body).contains("plain-order")).context("plain request")?.body)?;
        let offered = plain["tools"].as_array().context("plain tools")?;
        assert!(!offered.iter().any(|tool| tool["name"] == "exec"));
        assert!(offered.iter().any(|tool| tool["type"] == "function" && tool["name"] == "lookup_order"));

        // Cancellation interrupts a cell awaiting an application result.
        let cancelled = request(&client, reqwest::Method::POST, &sessions, json!({
            "agent":{"model":"mock-model","tools":tools},"environment":{"type":"none"},"input":"programmatic-cancel"
        })).await?;
        let url = format!("{sessions}/{}", cancelled["id"].as_str().context("cancelled id")?);
        pending(&client, &url).await?;
        let response = client.post(format!("{url}/events")).bearer_auth(TOKEN)
            .json(&json!({"events":[{"type":"agent.session.input.cancel"}]})).send().await?;
        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
        assert_eq!(idle(&client, &url, /*expected_turns*/ 1).await?["required_actions"], json!([]));
        let turns = request(&client, reqwest::Method::GET, &format!("{url}/turns"), Value::Null).await?;
        assert_eq!(turns["data"][0]["status"], "cancelled");
        assert_eq!(request(&client, reqwest::Method::DELETE, &url, Value::Null).await?,
            json!({"id":cancelled["id"],"object":"agent.session.deleted","deleted":true}));

        server.abort();
        api.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    }).await?
}
