use super::*;
use pretty_assertions::assert_eq;

/// A span attribute's value, as JSON: strings as strings, integers as numbers.
pub(super) fn attribute(span: &Value, key: &str) -> Value {
    let Some(value) = span["attributes"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|attribute| attribute["key"] == key)
        .map(|attribute| &attribute["value"])
    else {
        return Value::Null;
    };
    match (&value["stringValue"], &value["intValue"]) {
        (Value::String(text), _) => json!(text),
        (_, Value::String(number)) => json!(number.parse::<i64>().unwrap_or_default()),
        _ => value.clone(),
    }
}

pub(super) fn spans(trace: &Value) -> Vec<Value> {
    trace["otlp"]["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn nanos(span: &Value, field: &str) -> u128 {
    span[field]
        .as_str()
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

#[tokio::test]
async fn finished_turns_export_as_otlp_traces() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 90), async {
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let provider = create_mock_responses_server_repeating_assistant("finished").await;
        // Generations are recorded from each response's reported usage.
        let usage = json!({"input_tokens":12,"input_tokens_details":null,"output_tokens":3,"output_tokens_details":null,"total_tokens":15});
        let respond = |item: Value| sse(&[
            json!({"type":"response.created","response":{"id":"traced"}}),
            json!({"type":"response.output_item.done","item":item}),
            json!({"type":"response.completed","response":{"id":"traced","usage":usage}}),
        ]);
        Mock::given(body_string_contains("call-lookup"))
            .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_raw(respond(json!({"type":"function_call","call_id":"trace-call","name":"lookup","arguments":"{}"})), "text/event-stream"))
            .with_priority(/*p*/ 1).up_to_n_times(/*n*/ 1).mount(&provider).await;
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_raw(respond(json!({"type":"message","role":"assistant","id":"answer","content":[{"type":"output_text","text":"finished"}]})), "text/event-stream"))
            .with_priority(/*p*/ 2).mount(&provider).await;
        Mock::given(body_string_contains("stall-turn"))
            .respond_with(ResponseTemplate::new(/*s*/ 200).set_delay(Duration::from_secs(/*secs*/ 60)))
            .with_priority(/*p*/ 1).mount(&provider).await;
        MockResponsesConfig::new(&provider.uri()).with_root_config("features.plugins = false").write(home.path())?;
        let api = AgentsApi::new(backend(home.path()).await?, AbsolutePathBuf::from_absolute_path(data.path())?, TOKEN.into()).await?;
        let (base, server) = capabilities::serve(&api).await?;
        let client = reqwest::Client::new();
        let sessions = format!("{base}/agents/sessions");
        let session = request(&client, reqwest::Method::POST, &sessions, json!({
            "agent":{"model":"mock-model","instructions":"Look things up.","tools":[{"type":"function","name":"lookup","description":"Look up","parameters":{"type":"object","properties":{}}}]},
            "environment":{"type":"none"},"input":"call-lookup"})).await?;
        let id = session["id"].as_str().context("session id")?.to_owned();
        let url = format!("{sessions}/{id}");
        let pending = loop {
            let session = request(&client, reqwest::Method::GET, &url, Value::Null).await?;
            if session["status"] == "requires_action" {
                break session;
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await;
        };
        let action = &pending["required_actions"][0];
        let send = |events: Value| client.post(format!("{url}/events")).bearer_auth(TOKEN).json(&json!({"events":events})).send();
        assert_eq!(send(json!([{"type":"agent.session.input.tool_result","turn_id":action["turn_id"],"call_id":action["call_id"],"success":true,"output":"found-it"}])).await?.status(), reqwest::StatusCode::ACCEPTED);
        idle(&client, &url, /*expected_turns*/ 1).await?;
        assert_eq!(send(json!([{"type":"agent.session.input.message","input":"second-turn"}])).await?.status(), reqwest::StatusCode::ACCEPTED);
        idle(&client, &url, /*expected_turns*/ 2).await?;
        let turns = request(&client, reqwest::Method::GET, &format!("{url}/turns?order=asc"), Value::Null).await?;
        // IDs carry the official prefixes, and records carry the fields the
        // official service reports.
        assert_eq!(json!({"session":&id[..5],"agent":session["agent"]["id"].as_str().map(|agent| &agent[..6]),"turn":turns["data"][0]["id"].as_str().map(|turn| &turn[..5]),
            "review":session["review"],"compaction":session["agent"]["compaction"],"experimental":session["agent"]["experimental"],
            "cache":turns["data"][0].as_object().map(|turn| turn.contains_key("prompt_cache_options"))}),
            json!({"session":"sess_","agent":"agent_","turn":"turn_","review":"none","compaction":{"async":false},
                "experimental":{"time_awareness":false},"cache":true}));
        let first_turn = turns["data"][0]["id"].as_str().context("first turn")?.to_owned();
        let second_turn = turns["data"][1]["id"].as_str().context("second turn")?.to_owned();

        // A running turn has no trace yet.
        assert_eq!(send(json!([{"type":"agent.session.input.message","input":"stall-turn"}])).await?.status(), reqwest::StatusCode::ACCEPTED);
        let traces_url = format!("{url}/traces");
        let running = loop {
            let turns = request(&client, reqwest::Method::GET, &format!("{url}/turns"), Value::Null).await?;
            if turns["data"].as_array().is_some_and(|turns| turns.len() == 3) {
                break request(&client, reqwest::Method::GET, &format!("{traces_url}?order=asc"), Value::Null).await?;
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await;
        };
        let ids = |page: &Value| page["data"].as_array().into_iter().flatten().map(|trace| trace["id"].clone()).collect::<Vec<_>>();
        let (first, second) = (first_turn.clone(), second_turn.clone());
        assert_eq!((ids(&running), running["has_more"].clone()), (vec![json!(first), json!(second)], json!(false)));

        // The function-calling turn: the root agent with its two model
        // responses and the tool call, which carries the submitted result.
        let trace = running["data"][0].clone();
        // Turn IDs carry a prefix; the trace ID is the turn's UUID. Like the
        // official service, a trace is named by its turn.
        let trace_id = uuid::Uuid::parse_str(first_turn.strip_prefix("turn_").context("turn prefix")?)?.simple().to_string();
        assert_eq!(json!({"id":trace["id"],"object":trace["object"],"session_id":trace["session_id"],"turn_id":trace["turn_id"],
            "resource":trace["otlp"]["resourceSpans"][0]["resource"]["attributes"]}),
            json!({"id":first,"object":"agent.session.trace","session_id":id,"turn_id":null,"resource":[
                {"key":"service.name","value":{"stringValue":"codex-agents-api"}}]}));
        let recorded = spans(&trace);
        let root = recorded.iter().find(|span| span["parentSpanId"] == "").context("root span")?;
        // Spans follow the OpenTelemetry GenAI conventions as the official
        // service applies them; a span that ended well leaves its status unset.
        let parsed = |span: &Value, key: &str| -> anyhow::Result<Value> {
            Ok(serde_json::from_str(attribute(span, key).as_str().with_context(|| format!("{key} missing"))?)?)
        };
        assert_eq!(json!({"name":root["name"],"status":root["status"],"type":attribute(root, "openai.agents.agent_type"),
            "agent":attribute(root, "gen_ai.agent.id"),"instructions":attribute(root, "gen_ai.system_instructions"),
            "model":attribute(root, "gen_ai.request.model"),"conversation":attribute(root, "gen_ai.conversation.id"),
            "session":attribute(root, "openai.managed_agents.session.id"),"turn":attribute(root, "openai.managed_agents.turn.id"),
            "total":attribute(root, "openai.managed_agents.usage.total.total_tokens"),"completeness":parsed(root, "openai.managed_agents.trace.completeness")?,
            "asked":parsed(root, "gen_ai.input.messages")?,"answered":parsed(root, "gen_ai.output.messages")?}),
            json!({"name":"invoke_agent","status":{"code":0},"type":"root","agent":session["agent"]["id"],"instructions":"Look things up.",
                "model":"mock-model","conversation":id,"session":id,"turn":first_turn,"total":30,
                "completeness":{"status":"complete","missing_outputs":[],"ordering_issues":[]},
                "asked":[{"role":"user","parts":[{"type":"text","content":"call-lookup"}]}],
                "answered":[{"role":"assistant","parts":[{"type":"text","content":"finished"}]}]}));
        let children = recorded.iter().filter(|span| span["parentSpanId"] == root["spanId"]).collect::<Vec<_>>();
        assert_eq!(children.iter().map(|span| json!([span["name"], span["kind"]])).collect::<Vec<_>>(),
            vec![json!(["chat mock-model", 3]), json!(["chat mock-model", 3]), json!(["execute_tool lookup", 1])]);
        let tool = children[2];
        assert_eq!(json!({"status":tool["status"],"call":attribute(tool, "gen_ai.tool.call.id"),"type":attribute(tool, "gen_ai.tool.type"),
            "arguments":parsed(tool, "gen_ai.tool.call.arguments")?,"result":attribute(tool, "gen_ai.tool.call.result")}),
            json!({"status":{"code":0},"call":"trace-call","type":"function","arguments":{},"result":"found-it"}));
        // The first response asked for the call; the second saw its result
        // and answered.
        assert_eq!(parsed(children[0], "gen_ai.output.messages")?,
            json!([{"role":"assistant","parts":[{"type":"tool_call","id":"trace-call","name":"lookup","arguments":{}}]}]));
        assert_eq!(json!([parsed(children[1], "gen_ai.input.messages")?, parsed(children[1], "gen_ai.output.messages")?]),
            json!([[{"role":"tool","parts":[{"type":"tool_call_response","id":"trace-call","response":"found-it"}]}],
                [{"role":"assistant","parts":[{"type":"text","content":"finished"}]}]]));
        // Each response carries its own usage and names the model it came from.
        assert_eq!(children[..2].iter().map(|span| json!([attribute(span, "gen_ai.usage.input_tokens"), attribute(span, "gen_ai.usage.cache_read.input_tokens"),
            attribute(span, "gen_ai.usage.output_tokens"), attribute(span, "gen_ai.response.model")])).collect::<Vec<_>>(),
            vec![json!([12, 0, 3, "mock-model"]), json!([12, 0, 3, "mock-model"])]);
        // Every span belongs to the trace, has a unique ID, and lies within
        // the root agent's span.
        let mut span_ids = recorded.iter().map(|span| span["spanId"].clone()).collect::<Vec<_>>();
        span_ids.dedup();
        assert_eq!(span_ids.len(), recorded.len());
        for span in &recorded {
            assert_eq!(span["traceId"], json!(trace_id));
            assert!(nanos(span, "startTimeUnixNano") <= nanos(span, "endTimeUnixNano"), "{span}");
            assert!(nanos(root, "startTimeUnixNano") <= nanos(span, "startTimeUnixNano") && nanos(span, "endTimeUnixNano") <= nanos(root, "endTimeUnixNano"), "{span}");
        }

        // Pages follow the turns, newest first by default; exports are stable.
        let newest = request(&client, reqwest::Method::GET, &format!("{traces_url}?limit=1"), Value::Null).await?;
        assert_eq!((ids(&newest), newest["has_more"].clone()), (vec![json!(second)], json!(true)));
        let older = request(&client, reqwest::Method::GET, &format!("{traces_url}?limit=1&after={second}"), Value::Null).await?;
        assert_eq!(older["data"][0], trace);
        // Cursors from when traces had their own IDs still work.
        let legacy = request(&client, reqwest::Method::GET, &format!("{traces_url}?limit=1&after=trace_{second}"), Value::Null).await?;
        assert_eq!(legacy["data"][0], trace);
        for (query, message) in [("order=up", "order must be asc or desc"), ("after=trace_missing", "invalid after cursor"), ("cursor=x", "unknown query parameter cursor")] {
            let response = client.get(format!("{traces_url}?{query}")).bearer_auth(TOKEN).send().await?;
            assert_eq!((response.status(), response.json::<Value>().await?["error"]["message"].clone()), (reqwest::StatusCode::BAD_REQUEST, json!(message)));
        }
        let missing = client.get(format!("{sessions}/missing/traces")).bearer_auth(TOKEN).send().await?;
        assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);

        // A cancelled turn is exported once it ends, without an OK status.
        assert_eq!(send(json!([{"type":"agent.session.input.cancel"}])).await?.status(), reqwest::StatusCode::ACCEPTED);
        idle(&client, &url, /*expected_turns*/ 3).await?;
        let latest = request(&client, reqwest::Method::GET, &format!("{traces_url}?limit=1"), Value::Null).await?;
        let cancelled = spans(&latest["data"][0]);
        let root = cancelled.iter().find(|span| span["parentSpanId"] == "").context("cancelled root")?;
        assert_eq!(json!({"status":root["status"],"outcome":attribute(root, "openai.agents.status")}), json!({"status":{"code":0},"outcome":"cancelled"}));

        // Deleting the session removes every record it left, traces included.
        request(&client, reqwest::Method::DELETE, &url, Value::Null).await?;
        server.abort();
        api.shutdown().await?;
        assert_eq!(leftover_rows(data.path(), &id).await?, Vec::<(String, i64)>::new());
        Ok::<_, anyhow::Error>(())
    }).await?
}
