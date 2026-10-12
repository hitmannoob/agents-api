use super::*;
use pretty_assertions::assert_eq;

/// A response that searches the web, then answers.
fn searched(query: &str) -> String {
    [
        json!({"type": "response.created", "response": {"id": "searched"}}),
        json!({"type": "response.output_item.added", "item": {"type": "web_search_call", "id": "search-1", "status": "in_progress"}}),
        json!({"type": "response.output_item.done", "item": {"type": "web_search_call", "id": "search-1", "status": "completed", "action": {"type": "search", "query": query}}}),
        json!({"type": "response.output_item.done", "item": {"type": "message", "role": "assistant", "id": "answer-1", "content": [{"type": "output_text", "text": "sunny"}]}}),
        json!({"type": "response.completed", "response": {"id": "searched", "status": "completed", "output": []}}),
    ].iter().map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap_or_default())).collect()
}

#[tokio::test]
async fn web_search_tools_reach_the_model_and_record_their_calls() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 60), async {
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let provider = create_mock_responses_server_repeating_assistant("finished").await;
        Mock::given(body_string_contains("search-the-web"))
            .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_raw(searched("weather seattle"), "text/event-stream"))
            .with_priority(/*p*/ 1).up_to_n_times(/*n*/ 1).mount(&provider).await;
        MockResponsesConfig::new(&provider.uri()).with_root_config("features.plugins = false").write(home.path())?;
        let api = AgentsApi::new(backend(home.path()).await?, AbsolutePathBuf::from_absolute_path(data.path())?, TOKEN.into()).await?;
        let (base, server) = capabilities::serve(&api).await?;
        api.configure_environments("an-environment-key-for-web-search".into(), format!("{}/registry", base.trim_end_matches("/v1")))?;
        let client = reqwest::Client::new();
        let sessions = format!("{base}/agents/sessions");
        let session = |tools: Value, input: &str| request(&client, reqwest::Method::POST, &sessions,
            json!({"agent":{"model":"mock-model","tools":tools},"environment":{"type":"none"},"input":input}));

        // Live search with every setting, cached search, and no search tool.
        let live = session(json!([{"type":"web_search","mode":"live","context_size":"high","allowed_domains":["example.com"],
            "location":{"country":"US","city":"Seattle","region":null,"timezone":"America/Los_Angeles"}}]), "search-the-web").await?;
        let cached = session(json!([{"type":"web_search","mode":"cached"}]), "cached-search").await?;
        let disabled = session(json!([{"type":"web_search","mode":"disabled"}]), "disabled-search").await?;
        let none = session(json!([]), "no-search").await?;
        for created in [&live, &cached, &disabled, &none] {
            idle(&client, &format!("{sessions}/{}", created["id"].as_str().context("session id")?), /*expected_turns*/ 1).await?;
        }

        // The model is offered the tool exactly as configured.
        let captures = provider.received_requests().await.context("captures")?;
        let offered = |input: &str| -> anyhow::Result<Vec<Value>> {
            let request = captures.iter().filter(|r| r.url.path().ends_with("/responses"))
                .map(|r| serde_json::from_slice::<Value>(&r.body)).collect::<Result<Vec<_>, _>>()?
                .into_iter().find(|request| request["input"].to_string().contains(input)).context("request")?;
            Ok(request["tools"].as_array().into_iter().flatten().filter(|tool| tool["type"] == "web_search").cloned().collect())
        };
        assert_eq!(offered("search-the-web")?, vec![json!({"type":"web_search","external_web_access":true,"search_context_size":"high",
            "filters":{"allowed_domains":["example.com"]},
            "user_location":{"type":"approximate","country":"US","city":"Seattle","timezone":"America/Los_Angeles"}})]);
        assert_eq!(offered("cached-search")?, vec![json!({"type":"web_search","external_web_access":false,"search_context_size":"medium"})]);
        assert_eq!((offered("disabled-search")?, offered("no-search")?), (vec![], vec![]));

        // The search is recorded as a web_search_call item before the answer.
        let items = request(&client, reqwest::Method::GET, &format!("{sessions}/{}/items?order=asc", live["id"].as_str().context("live id")?), Value::Null).await?;
        let call = items["data"].as_array().into_iter().flatten().find(|item| item["type"] == "web_search_call").context("web search item")?;
        assert_eq!(json!({"status":call["status"],"action":call["action"],"turn_id":call["turn_id"].is_string()}),
            json!({"status":"completed","action":{"type":"search","query":"weather seattle","queries":null},"turn_id":true}));

        // Codex upgrades cached search to live for unsandboxed threads, as
        // self-hosted sessions run, so cached is refused there.
        let refused = client.post(&sessions).bearer_auth(TOKEN).json(&json!({"agent":{"model":"mock-model","tools":[{"type":"web_search","mode":"cached"}]},
            "environment":{"type":"self_hosted","workspace_directory":workspace.path().to_str().context("workspace")?}})).send().await?;
        assert_eq!((refused.status(), refused.json::<Value>().await?["error"]["message"].clone()), (reqwest::StatusCode::BAD_REQUEST,
            json!("web_search mode cached is not available on self_hosted environments; use live")));

        server.abort();
        api.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    }).await?
}
