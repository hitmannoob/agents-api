use super::*;
use codex_utils_cargo_bin::find_resource;
use pretty_assertions::assert_eq;

/// Encode Responses stream events as an SSE body.
fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap_or_default()
            )
        })
        .collect()
}

fn function(name: &str, defer_loading: bool) -> Value {
    json!({"type":"function","name":name,"description":format!("Look up the status of an order ({name})."),
        "parameters":{"type":"object","properties":{}},"defer_loading":defer_loading})
}

/// The function tools a model request offered, by name, and the request's
/// tool_search tools.
fn offered(request: &Value) -> (Vec<String>, usize) {
    let tools = request["tools"].as_array().cloned().unwrap_or_default();
    let names = tools
        .iter()
        .filter(|tool| tool["type"] == "function")
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect();
    (
        names,
        tools
            .iter()
            .filter(|tool| tool["type"] == "tool_search")
            .count(),
    )
}

#[tokio::test]
async fn deferred_functions_are_found_through_tool_search() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 60), async {
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let provider = create_mock_responses_server_repeating_assistant("shipped").await;
        // The model searches for the deferred function, then calls it in the
        // namespace the search returned.
        Mock::given(body_string_contains("find-the-order"))
            .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_raw(sse(&[
                json!({"type":"response.created","response":{"id":"search"}}),
                json!({"type":"response.output_item.done","item":{"type":"tool_search_call","call_id":"search-1","execution":"client","arguments":{"query":"order status","limit":8}}}),
                json!({"type":"response.completed","response":{"id":"search","status":"completed","output":[]}}),
            ]), "text/event-stream"))
            .with_priority(/*p*/ 2).up_to_n_times(/*n*/ 1).mount(&provider).await;
        Mock::given(body_string_contains("tool_search_output"))
            .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_raw(sse(&[
                json!({"type":"response.created","response":{"id":"call"}}),
                json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"deferred-call","namespace":"deferred","name":"lookup_order","arguments":"{}"}}),
                json!({"type":"response.completed","response":{"id":"call","status":"completed","output":[]}}),
            ]), "text/event-stream"))
            .with_priority(/*p*/ 1).up_to_n_times(/*n*/ 1).mount(&provider).await;
        // The catalog marks mock-model as able to search tools.
        let catalog = home.path().join("models.json");
        std::fs::copy(find_resource!("tests/fixtures/model_catalog.json")?, &catalog)?;
        MockResponsesConfig::new(&provider.uri())
            .with_root_config(&format!("features.plugins = false\nmodel_catalog_json = {}", serde_json::to_string(&catalog)?))
            .write(home.path())?;
        let api = AgentsApi::new(backend(home.path()).await?, AbsolutePathBuf::from_absolute_path(data.path())?, TOKEN.into()).await?;
        let (base, server) = capabilities::serve(&api).await?;
        let client = reqwest::Client::new();
        let sessions = format!("{base}/agents/sessions");

        // Deferring a function needs a tool_search tool.
        let refused = client.post(&sessions).bearer_auth(TOKEN).json(&json!({"agent":{"model":"mock-model","tools":[function("lookup_order", true)]},
            "environment":{"type":"none"},"input":"hello"})).send().await?;
        assert_eq!((refused.status(), refused.json::<Value>().await?["error"]["message"].clone()),
            (reqwest::StatusCode::BAD_REQUEST, json!("functions with defer_loading need a tool_search tool in agent.tools")));

        // The deferred function is not offered up front; tool search finds it,
        // and its call waits for the application's result like any other.
        let tools = json!([{"type":"tool_search"}, function("lookup_order", true), function("eager_tool", false)]);
        let session = request(&client, reqwest::Method::POST, &sessions,
            json!({"agent":{"model":"mock-model","tools":tools},"environment":{"type":"none"},"input":"find-the-order"})).await?;
        let url = format!("{sessions}/{}", session["id"].as_str().context("session id")?);
        let pending = loop {
            let current = request(&client, reqwest::Method::GET, &url, Value::Null).await?;
            match current["status"].as_str() {
                Some("requires_action") => break current,
                Some("idle" | "failed") => anyhow::bail!("the deferred function was never called: {current}"),
                _ => tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await,
            }
        };
        let action = pending["required_actions"][0].clone();
        assert_eq!((&action["type"], &action["name"], &action["call_id"]), (&json!("function_call"), &json!("lookup_order"), &json!("deferred-call")));
        let submitted = client.post(format!("{url}/events")).bearer_auth(TOKEN).json(&json!({"events":[{"type":"agent.session.input.tool_result",
            "turn_id":action["turn_id"],"call_id":"deferred-call","success":true,"output":"in transit"}]})).send().await?;
        assert_eq!(submitted.status(), reqwest::StatusCode::ACCEPTED);
        idle(&client, &url, /*expected_turns*/ 1).await?;
        let captures = provider.received_requests().await.context("captures")?;
        let bodies: Vec<Value> = captures.iter().filter(|r| r.url.path().ends_with("/responses"))
            .map(|r| serde_json::from_slice(&r.body)).collect::<Result<_, _>>()?;
        let first = bodies.iter().find(|body| body.to_string().contains("find-the-order")).context("first request")?;
        assert_eq!(offered(first), (vec!["eager_tool".to_owned()], 1));
        let searched = bodies.iter().find(|body| body.to_string().contains("tool_search_output")).context("search follow-up")?;
        let output = searched["input"].as_array().into_iter().flatten().find(|item| item["type"] == "tool_search_output").context("search output")?;
        assert_eq!((&output["tools"][0]["name"], &output["tools"][0]["tools"][0]["name"]), (&json!("deferred"), &json!("lookup_order")));
        assert!(bodies.iter().any(|body| body.to_string().contains("in transit")), "the result reached the model");

        // A model that cannot search tools gets deferred functions up front,
        // so they stay callable.
        let plain = request(&client, reqwest::Method::POST, &sessions,
            json!({"agent":{"model":"plain-model","tools":tools},"environment":{"type":"none"},"input":"no-search-here"})).await?;
        idle(&client, &format!("{sessions}/{}", plain["id"].as_str().context("plain id")?), /*expected_turns*/ 1).await?;
        let captures = provider.received_requests().await.context("captures")?;
        let plain_request: Value = serde_json::from_slice(&captures.iter().rev().find(|r| String::from_utf8_lossy(&r.body).contains("no-search-here")).context("plain request")?.body)?;
        let (mut names, searches) = offered(&plain_request);
        names.sort();
        assert_eq!((names, searches), (vec!["eager_tool".to_owned(), "lookup_order".to_owned()], 0));

        server.abort();
        api.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    }).await?
}
