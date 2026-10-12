use super::*;
use app_test_support::create_mock_responses_server_sequence_unchecked;
use codex_utils_cargo_bin::find_resource;
use pretty_assertions::assert_eq;

fn reply(id: &str, text: &str, total_tokens: i64) -> String {
    sse(&[
        json!({"type":"response.created","response":{"id":id}}),
        json!({"type":"response.output_item.done","item":{"type":"message","id":id,"role":"assistant",
            "content":[{"type":"output_text","text":text}]}}),
        json!({"type":"response.completed","response":{"id":id,"usage":{
            "input_tokens":total_tokens,"input_tokens_details":null,"output_tokens":0,"output_tokens_details":null,"total_tokens":total_tokens
        }}}),
    ])
}

#[tokio::test]
async fn automatic_compaction_survives_cold_resume() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 90), async {
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let compact_prompt = "Summarize this session for the agents-api compaction test.";
        let summary = "COMPACTED_ORDER_SUMMARY_731";
        let early_reply = "EARLY_ASSISTANT_TRANSCRIPT_731";
        let messages = ["first-user-message-731", "second-user-message-731", "resumed-user-message-731"];
        let provider = create_mock_responses_server_sequence_unchecked(vec![
            reply("first", early_reply, /*total_tokens*/ 70_000),
            reply("compact", summary, /*total_tokens*/ 200),
            reply("second", "SECOND_REPLY_731", /*total_tokens*/ 120),
            reply("resumed", "RESUMED_REPLY_731", /*total_tokens*/ 120),
        ]).await;
        let catalog = home.path().join("models.json");
        std::fs::copy(find_resource!("tests/fixtures/model_catalog.json")?, &catalog)?;
        MockResponsesConfig::new(&provider.uri()).with_root_config(&format!(
            "features.plugins = false\nmodel_catalog_json = {}\ncompact_prompt = {}\nmodel_auto_compact_token_limit = 10000",
            serde_json::to_string(&catalog)?, serde_json::to_string(compact_prompt)?
        )).with_provider_config("supports_websockets = false").write(home.path())?;
        let api = AgentsApi::new(backend(home.path()).await?, AbsolutePathBuf::from_absolute_path(data.path())?, TOKEN.into()).await?;
        let (base, server) = capabilities::serve(&api).await?;
        let client = reqwest::Client::new();
        let session = request(&client, reqwest::Method::POST, &format!("{base}/agents/sessions"), json!({
            "agent":{"model":"mock-model"},"environment":{"type":"none"},"input":messages[0]
        })).await?;
        let id = session["id"].as_str().context("session id")?;
        let url = format!("{base}/agents/sessions/{id}");
        idle(&client, &url, /*expected_turns*/ 1).await?;
        let response = client.post(format!("{url}/events")).bearer_auth(TOKEN).json(&json!({
            "events":[{"type":"agent.session.input.message","input":messages[1]}]
        })).send().await?;
        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
        idle(&client, &url, /*expected_turns*/ 2).await?;
        let captures = provider.received_requests().await.context("captures")?;
        let bodies = captures.iter().filter(|r| r.url.path().ends_with("/responses"))
            .map(|r| serde_json::from_slice::<Value>(&r.body)).collect::<Result<Vec<_>, _>>()?;
        assert_eq!(bodies.len(), 3);
        assert!(bodies[1]["input"].to_string().contains(compact_prompt), "{}", bodies[1]);
        assert!(bodies[1]["input"].to_string().contains(early_reply));
        assert!(bodies[2]["input"].to_string().contains(summary), "{}", bodies[2]);
        assert!(bodies[2]["input"].to_string().contains(messages[1]));
        assert!(!bodies[2]["input"].to_string().contains(early_reply));

        server.abort();
        api.shutdown().await?;
        let api = AgentsApi::new(backend(home.path()).await?, AbsolutePathBuf::from_absolute_path(data.path())?, TOKEN.into()).await?;
        let (base, server) = capabilities::serve(&api).await?;
        let url = format!("{base}/agents/sessions/{id}");
        let response = client.post(format!("{url}/events")).bearer_auth(TOKEN).json(&json!({
            "events":[{"type":"agent.session.input.message","input":messages[2]}]
        })).send().await?;
        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
        idle(&client, &url, /*expected_turns*/ 3).await?;
        let turns = request(&client, reqwest::Method::GET, &format!("{url}/turns?order=asc"), Value::Null).await?;
        let turns = turns["data"].as_array().context("turns")?;
        assert_eq!(turns.len(), 3);
        assert!(turns.iter().all(|turn| turn["status"] == "completed"));
        let captures = provider.received_requests().await.context("captures")?;
        let bodies = captures.iter().filter(|r| r.url.path().ends_with("/responses"))
            .map(|r| serde_json::from_slice::<Value>(&r.body)).collect::<Result<Vec<_>, _>>()?;
        assert_eq!(bodies.len(), 4);
        // Local compaction keeps recent user messages and a summary, replacing
        // earlier assistant replies. Cold resume must restore that replacement.
        let resumed = bodies[3]["input"].to_string();
        assert!(resumed.contains(summary), "{resumed}");
        assert!(!resumed.contains(early_reply), "{resumed}");
        assert!(resumed.contains("SECOND_REPLY_731"));
        for message in messages {
            assert!(resumed.contains(message), "{resumed}");
        }

        let items = request(&client, reqwest::Method::GET, &format!("{url}/items?order=asc"), Value::Null).await?;
        assert_eq!(items["has_more"], false);
        let items = items["data"].as_array().context("items")?;
        // Public records are the complete application history, independent of
        // the worker's compacted model context; compaction has no public item.
        assert!(items.iter().all(|item| item["type"] == "message"), "{items:?}");
        let users = items.iter().filter(|item| item["role"] == "user")
            .map(|item| item["content"][0]["text"].clone()).collect::<Vec<_>>();
        assert_eq!(users, messages.map(|message| json!(message)).to_vec());
        assert!(items.iter().any(|item| item["role"] == "assistant" && item["content"].to_string().contains(early_reply)));
        assert!(!items.iter().any(|item| item["content"].to_string().contains(summary)));

        server.abort();
        api.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    }).await?
}
