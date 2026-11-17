//! Session trace export. Each finished root turn is one OTLP trace, shaped as
//! the tracing guide describes: an agent span for the root agent and for each
//! subagent that ran during the turn, with generation spans for model
//! responses and tool spans for calls beneath the agent that made them. Traces
//! are built from saved records on request, so span IDs are stable across
//! exports. Attribute names follow the OpenTelemetry GenAI conventions where
//! the guide leaves them open.
use crate::ApiError;
use crate::State;
use crate::contract::invalid;
use crate::otlp::attribute;
use crate::otlp::hex;
use crate::otlp::span;
use crate::otlp::span_id;
use crate::projects::Principal;
use crate::projects::Resource;
use axum::Extension;
use axum::Json;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State as Extract;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

const SERVICE: &str = "codex-agents-api";

/// A saved turn or item with the times the service observed it.
struct Record {
    seq: i64,
    id: String,
    turn_id: String,
    subagent_id: Option<String>,
    data: Value,
    started_ms: Option<i64>,
    completed_ms: Option<i64>,
}

type Row = (
    i64,
    String,
    String,
    Option<String>,
    String,
    Option<i64>,
    Option<i64>,
);

impl Record {
    fn from_row(row: Row) -> anyhow::Result<Self> {
        let (seq, id, turn_id, subagent_id, data, started_ms, completed_ms) = row;
        Ok(Self {
            seq,
            id,
            turn_id,
            subagent_id,
            data: serde_json::from_str(&data)?,
            started_ms,
            completed_ms,
        })
    }

    /// When a turn started and ended. Turns saved before timing was recorded
    /// fall back to their second-resolution timestamps.
    fn span(&self) -> (i64, i64) {
        let seconds = |field: &str| self.data[field].as_i64().map(|at| at * 1000);
        let start = self
            .started_ms
            .or_else(|| seconds("started_at"))
            .or_else(|| seconds("created_at"))
            .unwrap_or_default();
        let end = self
            .completed_ms
            .or_else(|| seconds("completed_at"))
            .unwrap_or(start);
        (start, end.max(start))
    }

    fn kind(&self) -> &str {
        self.data["type"].as_str().unwrap_or_default()
    }

    /// Inputs to the model: user messages and function results.
    fn is_input(&self) -> bool {
        // A message from another agent is input to the agent receiving it.
        self.data["role"] == "user"
            || matches!(self.kind(), "function_call_output" | "agent_message")
    }

    /// Calls the model made, as opposed to its messages and reasoning.
    fn is_tool(&self) -> bool {
        !self.is_input() && !matches!(self.kind(), "message" | "reasoning")
    }
}

/// A model response's reported usage and, for responses recorded with
/// timing, when the service saw it finish.
struct Generation {
    turn_id: String,
    input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
    model: Option<String>,
    cached_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    completed_ms: Option<i64>,
}

type GenerationRow = (
    String,
    i64,
    i64,
    i64,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

pub(crate) async fn list(
    Extract(state): Extract<Arc<State>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Query(query): Query<Vec<(String, String)>>,
) -> Result<Json<Value>, ApiError> {
    crate::projects::require(&state, &principal.project_id, &id, Resource::PublicSession).await?;
    let mut after = None;
    let mut limit = 20;
    let mut ascending = false;
    for (key, value) in &query {
        match key.as_str() {
            "after" => after = Some(value.clone()),
            "limit" => {
                limit = value
                    .parse::<i64>()
                    .map_err(|_| invalid("limit must be an integer"))?
                    .clamp(1, 100);
            }
            "order" => {
                ascending = match value.as_str() {
                    "asc" => true,
                    "desc" => false,
                    _ => return Err(invalid("order must be asc or desc")),
                };
            }
            _ => return Err(invalid(format!("unknown query parameter {key}"))),
        }
    }
    let session = crate::records::session(&state, &id).await?;
    let pool = &state.store.0;
    let start = match after {
        Some(after) => sqlx::query_scalar("SELECT seq FROM public_records WHERE session_id = ? AND kind = 'turn' AND subagent_id IS NULL AND id = ?")
            .bind(&id)
            // Traces are named by their turn; `trace_` cursors predate that.
            .bind(after.strip_prefix("trace_").unwrap_or(&after))
            .fetch_optional(pool)
            .await
            .map_err(anyhow::Error::from)?
            .ok_or_else(|| invalid("invalid after cursor"))?,
        None if ascending => 0,
        None => i64::MAX,
    };
    // Traces are built once a turn ends.
    let rows: Vec<Row> = sqlx::query_as(if ascending {
        "SELECT seq, id, turn_id, subagent_id, data, started_ms, completed_ms FROM public_records WHERE session_id = ? AND kind = 'turn' AND subagent_id IS NULL AND json_extract(data, '$.status') IN ('completed', 'failed', 'cancelled') AND seq > ? ORDER BY seq ASC LIMIT ?"
    } else {
        "SELECT seq, id, turn_id, subagent_id, data, started_ms, completed_ms FROM public_records WHERE session_id = ? AND kind = 'turn' AND subagent_id IS NULL AND json_extract(data, '$.status') IN ('completed', 'failed', 'cancelled') AND seq < ? ORDER BY seq DESC LIMIT ?"
    })
    .bind(&id)
    .bind(start)
    .bind(limit + 1)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let has_more = rows.len() as i64 > limit;
    let mut data = Vec::new();
    for row in rows.into_iter().take(limit as usize) {
        data.push(trace(&state, &session, Record::from_row(row)?).await?);
    }
    Ok(Json(
        json!({"object":"list","first_id":data.first().map(|v| &v["id"]),
        "last_id":data.last().map(|v| &v["id"]),"data":data,"has_more":has_more}),
    ))
}

/// What every span in one trace shares.
struct Context<'a> {
    trace_id: &'a str,
    session_id: &'a str,
    /// The root turn the trace exports.
    turn_id: &'a str,
    /// The response that issued each tool call, by (turn ID, call ID).
    issued: &'a HashMap<(String, String), i64>,
}

impl Context<'_> {
    /// The attributes that tie a span to its session and turn.
    fn ids(&self, turn_id: &str) -> [Value; 3] {
        [
            attribute("gen_ai.conversation.id", &json!(self.session_id)),
            attribute("openai.managed_agents.session.id", &json!(self.session_id)),
            attribute("openai.managed_agents.turn.id", &json!(turn_id)),
        ]
    }
}

/// Whether the trace shows everything its turn did: tool calls that never
/// reported an outcome, and responses that could not be told apart.
#[derive(Default)]
struct Completeness {
    missing_outputs: Vec<String>,
    ordering_issues: Vec<String>,
}

impl Completeness {
    fn json(&self) -> Value {
        let status = if self.missing_outputs.is_empty() && self.ordering_issues.is_empty() {
            "complete"
        } else {
            "incomplete"
        };
        json!({"status":status,"missing_outputs":self.missing_outputs,"ordering_issues":self.ordering_issues})
    }
}

/// One root turn's trace, with the subagent turns that started while it was
/// the session's latest turn.
async fn trace(state: &State, session: &Value, root: Record) -> anyhow::Result<Value> {
    let pool = &state.store.0;
    let session_id = session["id"].as_str().unwrap_or_default();
    let next: Option<i64> = sqlx::query_scalar("SELECT min(seq) FROM public_records WHERE session_id = ? AND kind = 'turn' AND subagent_id IS NULL AND seq > ?")
        .bind(session_id)
        .bind(root.seq)
        .fetch_one(pool)
        .await?;
    let subagent_turns = sqlx::query_as::<_, Row>("SELECT seq, id, turn_id, subagent_id, data, started_ms, completed_ms FROM public_records WHERE session_id = ? AND kind = 'turn' AND subagent_id IS NOT NULL AND seq > ? AND seq < ? ORDER BY seq")
        .bind(session_id)
        .bind(root.seq)
        .bind(next.unwrap_or(i64::MAX))
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(Record::from_row)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let turn_ids = json!(
        std::iter::once(&root.id)
            .chain(subagent_turns.iter().map(|turn| &turn.id))
            .collect::<Vec<_>>()
    )
    .to_string();
    let items = sqlx::query_as::<_, Row>("SELECT seq, id, turn_id, subagent_id, data, started_ms, completed_ms FROM public_records WHERE session_id = ? AND kind = 'item' AND turn_id IN (SELECT value FROM json_each(?)) ORDER BY seq")
        .bind(session_id)
        .bind(&turn_ids)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(Record::from_row)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let generations = sqlx::query_as::<_, GenerationRow>("SELECT turn_id, input_tokens, output_tokens, total_tokens, model, cached_tokens, reasoning_tokens, completed_ms FROM generations WHERE session_id = ? AND turn_id IN (SELECT value FROM json_each(?)) ORDER BY seq")
        .bind(session_id)
        .bind(&turn_ids)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(
            |(
                turn_id,
                input_tokens,
                output_tokens,
                total_tokens,
                model,
                cached_tokens,
                reasoning_tokens,
                completed_ms,
            )| Generation {
                turn_id,
                input_tokens,
                output_tokens,
                total_tokens,
                model,
                cached_tokens,
                reasoning_tokens,
                completed_ms,
            },
        )
        .collect::<Vec<_>>();
    let issued: HashMap<(String, String), i64> = sqlx::query_as::<_, (String, String, i64)>("SELECT turn_id, call_id, response FROM response_calls WHERE session_id = ? AND turn_id IN (SELECT value FROM json_each(?))")
        .bind(session_id)
        .bind(&turn_ids)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|(turn_id, call_id, response)| ((turn_id, call_id), response))
        .collect();
    // The model the root turn asked for, or the one the provider rerouted it to.
    let requested: Option<String> =
        sqlx::query_scalar("SELECT model FROM turn_models WHERE session_id = ? AND turn_id = ?")
            .bind(session_id)
            .bind(&root.id)
            .fetch_optional(pool)
            .await?;
    let trace_id = Uuid::parse_str(root.id.strip_prefix("turn_").unwrap_or(&root.id))
        .map(|id| id.simple().to_string())
        .unwrap_or_else(|_| hex(&Sha256::digest(root.id.as_bytes())[..16]));
    let context = Context {
        trace_id: &trace_id,
        session_id,
        turn_id: &root.id,
        issued: &issued,
    };
    let mut completeness = Completeness::default();
    let agent = &session["agent"];
    let root_agent = Agent {
        key: "root".into(),
        id: agent["id"].as_str().unwrap_or_default().to_owned(),
        name: agent["name"].as_str().map(str::to_owned),
        kind: "root",
        instructions: agent["instructions"].as_str().map(str::to_owned),
        model: requested.or_else(|| agent["model"].as_str().map(str::to_owned)),
    };
    let root_span = span_id(&trace_id, &root_agent.key);
    let mut spans = agent_spans(
        &context,
        /*parent*/ None,
        &root_agent,
        &[&root],
        &items,
        &generations,
        &mut completeness,
    );
    let mut subagents: Vec<(&str, Vec<&Record>)> = Vec::new();
    for turn in &subagent_turns {
        let subagent = turn.subagent_id.as_deref().unwrap_or_default();
        match subagents.iter_mut().find(|(id, _)| *id == subagent) {
            Some((_, turns)) => turns.push(turn),
            None => subagents.push((subagent, vec![turn])),
        }
    }
    for (subagent, turns) in &subagents {
        let name: Option<String> = sqlx::query_scalar(
            "SELECT json_extract(data, '$.name') FROM subagents WHERE session_id = ? AND id = ?",
        )
        .bind(session_id)
        .bind(*subagent)
        .fetch_optional(pool)
        .await?
        .flatten();
        let agent = Agent {
            key: format!("subagent:{subagent}"),
            id: (*subagent).to_owned(),
            name,
            kind: "subagent",
            instructions: None,
            model: None,
        };
        spans.extend(agent_spans(
            &context,
            Some(&root_span),
            &agent,
            turns,
            &items,
            &generations,
            &mut completeness,
        ));
    }
    // The root agent's span carries the whole turn's usage, subagents
    // included, and whether the trace is complete.
    let all_turns: Vec<&Record> = std::iter::once(&root).chain(&subagent_turns).collect();
    let mut summary = Vec::new();
    if let Some([input, cached, output, reasoning, total]) = usage(&all_turns) {
        for (key, value) in [
            ("input_tokens", input),
            ("cache_read.input_tokens", cached),
            ("output_tokens", output),
            ("reasoning.output_tokens", reasoning),
            ("total_tokens", total),
        ] {
            summary.push(attribute(
                &format!("openai.managed_agents.usage.total.{key}"),
                &json!(value),
            ));
        }
    }
    summary.push(attribute(
        "openai.managed_agents.trace.completeness",
        &completeness.json(),
    ));
    if let Some(attributes) = spans
        .first_mut()
        .and_then(|span| span["attributes"].as_array_mut())
    {
        attributes.extend(summary);
    }
    let resource = [attribute("service.name", &json!(SERVICE))];
    Ok(
        json!({"id":root.id,"object":"agent.session.trace","session_id":session_id,
        "created_at":root.span().1 / 1000,"otlp":{"resourceSpans":[{"resource":{"attributes":resource},
        "scopeSpans":[{"scope":{"name":SERVICE,"version":env!("CARGO_PKG_VERSION")},"spans":spans}]}]}}),
    )
}

struct Agent {
    /// Distinguishes the agent's span within its trace.
    key: String,
    id: String,
    name: Option<String>,
    kind: &'static str,
    instructions: Option<String>,
    /// The model the agent's turns asked for, when known.
    model: Option<String>,
}

/// Input, cached input, output, reasoning output, and total tokens summed over
/// `turns`, or `None` when none reported usage.
fn usage(turns: &[&Record]) -> Option<[i64; 5]> {
    let reported: Vec<&Value> = turns
        .iter()
        .map(|turn| &turn.data["usage"])
        .filter(|usage| usage.is_object())
        .collect();
    (!reported.is_empty()).then(|| {
        [
            "/input_tokens",
            "/input_tokens_details/cached_tokens",
            "/output_tokens",
            "/output_tokens_details/reasoning_tokens",
            "/total_tokens",
        ]
        .map(|pointer| {
            reported
                .iter()
                .filter_map(|usage| usage.pointer(pointer).and_then(Value::as_i64))
                .sum()
        })
    })
}

/// When an item started and ended, within its turn's bounds.
fn times(item: &Record, (turn_start, turn_end): (i64, i64)) -> (i64, i64) {
    let begin = item.started_ms.or(item.completed_ms).unwrap_or(turn_start);
    (begin, item.completed_ms.unwrap_or(turn_end).max(begin))
}

/// One model response: the inputs it was the first to see, the outputs it
/// produced, its reported usage, and when it ran.
struct Response<'a> {
    inputs: Vec<&'a Record>,
    outputs: Vec<&'a Record>,
    generation: Option<&'a Generation>,
    span: (i64, i64),
}

/// A turn's responses from their recorded usage reports. Codex reports a
/// response's usage once the tool calls it made have finished, just before it
/// sends the next request, so report `k` marks when response `k + 1` began.
/// A tool call belongs to the response that issued it, as recorded when the
/// model's call arrived. A message or reasoning belongs to the response in
/// progress when it started: the one after every response reported by then.
/// An input belongs to that response if it arrived before the response
/// produced anything, and otherwise to the next one, which sees it; a tool's
/// result is such an input. A response ends at its last output: a message
/// finishing, or a tool call being issued. Items after the last report belong
/// to a response that never finished.
fn timed<'a>(
    context: &Context,
    turn: &Record,
    owned: &[&'a Record],
    usage: &[&'a Generation],
) -> Vec<Response<'a>> {
    let bounds = turn.span();
    let reports: Vec<i64> = usage
        .iter()
        .map(|generation| generation.completed_ms.unwrap_or(bounds.1))
        .collect();
    // The response in progress at `at`.
    let current = |at: i64| reports.iter().filter(|report| **report < at).count();
    let mut buckets: Vec<(Vec<&'a Record>, Vec<&'a Record>)> =
        vec![Default::default(); usage.len() + 1];
    for item in owned.iter().copied() {
        let at = times(item, bounds).0;
        let codex_id = item
            .id
            .strip_prefix(&format!("item_{}_", item.turn_id))
            .unwrap_or(&item.id);
        let index = if item.is_input() {
            let index = current(at);
            let answered = owned.iter().any(|output| {
                !output.is_input()
                    && current(times(output, bounds).0) == index
                    && times(output, bounds).0 < at
            });
            index + usize::from(answered)
        } else if let Some(response) = item
            .is_tool()
            .then(|| {
                context
                    .issued
                    .get(&(item.turn_id.clone(), codex_id.to_owned()))
            })
            .flatten()
        {
            usize::try_from(*response).unwrap_or(usize::MAX)
        } else {
            current(at)
        }
        .min(usage.len());
        if item.is_input() {
            buckets[index].0.push(item);
        } else {
            buckets[index].1.push(item);
        }
    }
    let mut responses = Vec::new();
    for (index, (inputs, outputs)) in buckets.into_iter().enumerate() {
        let generation = usage.get(index).copied();
        if generation.is_none() && outputs.is_empty() {
            continue;
        }
        let begin = index
            .checked_sub(1)
            .map_or(bounds.0, |prior| reports[prior])
            .max(
                inputs
                    .iter()
                    .map(|item| times(item, bounds).0)
                    .max()
                    .unwrap_or(bounds.0),
            );
        let end = outputs
            .iter()
            .map(|item| {
                if item.is_tool() {
                    times(item, bounds).0
                } else {
                    times(item, bounds).1
                }
            })
            .fold(begin, i64::max);
        responses.push(Response {
            inputs,
            outputs,
            generation,
            span: (begin, end),
        });
    }
    responses
}

/// A turn's responses inferred from its items, for turns recorded before
/// responses had timing. A response's outputs run until the model has to
/// wait: for an input such as a function result, or for a tool call it made
/// to return, which a message or reasoning after that call shows. Usage is
/// attached only when every response has its report.
fn inferred<'a>(
    turn: &Record,
    owned: &[&'a Record],
    usage: &[&'a Generation],
) -> (Vec<Response<'a>>, bool) {
    let bounds = turn.span();
    let mut groups: Vec<(Vec<&'a Record>, Vec<&'a Record>)> = Vec::new();
    let mut current: (Vec<&'a Record>, Vec<&'a Record>) = (Vec::new(), Vec::new());
    for item in owned.iter().copied() {
        let waited =
            item.is_input() || (!item.is_tool() && current.1.iter().any(|output| output.is_tool()));
        if waited && !current.1.is_empty() {
            groups.push(std::mem::take(&mut current));
        }
        if item.is_input() {
            current.0.push(item);
        } else {
            current.1.push(item);
        }
    }
    if !current.1.is_empty() {
        groups.push(current);
    }
    let matched = usage.len() == groups.len();
    let mut responses: Vec<Response> = Vec::new();
    let mut previous = bounds.0;
    for (index, (inputs, outputs)) in groups.into_iter().enumerate() {
        let returned = responses.last().map_or_else(Vec::new, |prior| {
            prior
                .outputs
                .iter()
                .filter(|item| item.is_tool())
                .map(|item| times(item, bounds).1)
                .collect()
        });
        let begin = inputs
            .iter()
            .map(|item| times(item, bounds).1)
            .chain(returned)
            .fold(previous, i64::max);
        // A response ends when it produced its last output: a tool call's
        // issue, or the end of a message.
        let end = outputs
            .iter()
            .map(|item| {
                if item.is_tool() {
                    times(item, bounds).0
                } else {
                    times(item, bounds).1
                }
            })
            .fold(begin, i64::max);
        responses.push(Response {
            inputs,
            outputs,
            generation: matched.then(|| usage[index]),
            span: (begin, end),
        });
        previous = end;
    }
    (responses, matched)
}

/// A tool call as the trace describes it.
struct Tool {
    name: String,
    /// `function` for the agent's functions; `extension` for tools the
    /// service runs.
    kind: &'static str,
    call_id: String,
    arguments: Value,
    result: Option<Value>,
    /// The failure's message, when the call failed.
    failure: Option<String>,
}

/// Arguments sent as JSON text are exported as the JSON they encode.
fn decoded(arguments: &Value) -> Value {
    arguments
        .as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_else(|| arguments.clone())
}

fn message_text(error: &Value) -> Option<String> {
    match error {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Object(fields) if fields.get("message").is_some_and(Value::is_string) => {
            fields["message"].as_str().map(str::to_owned)
        }
        other => Some(other.to_string()),
    }
}

/// Describe a tool call item, finding a function call's result among `owned`.
fn tool(item: &Record, owned: &[&Record]) -> Tool {
    let call = &item.data;
    let call_id = call["call_id"]
        .as_str()
        .or_else(|| call["id"].as_str())
        .unwrap_or_default()
        .to_owned();
    let failed = call["status"] == "failed";
    match item.kind() {
        "function_call" => {
            let output = owned
                .iter()
                .find(|output| {
                    output.kind() == "function_call_output"
                        && output.data["call_id"] == call["call_id"]
                })
                .map(|output| &output.data);
            let failure = output
                .filter(|output| output["status"] == "failed")
                .map(|output| {
                    message_text(&output["error"]).unwrap_or_else(|| "Function call failed".into())
                });
            Tool {
                name: call["name"].as_str().unwrap_or_default().to_owned(),
                kind: "function",
                call_id,
                arguments: decoded(&call["arguments"]),
                result: output.map(|output| output["output"].clone()),
                failure,
            }
        }
        "mcp_call" => Tool {
            name: format!(
                "{}.{}",
                call["server_label"].as_str().unwrap_or_default(),
                call["name"].as_str().unwrap_or_default()
            ),
            kind: "extension",
            call_id,
            arguments: decoded(&call["arguments"]),
            result: Some(call["output"].clone()),
            failure: message_text(&call["error"])
                .or_else(|| failed.then(|| "MCP call failed".into())),
        },
        "command_execution" => Tool {
            name: "command_execution".into(),
            kind: "extension",
            call_id,
            arguments: json!({"command":call["command"]}),
            result: Some(json!({"output":call["output"],"exit_code":call["exit_code"]})),
            failure: failed.then(|| "Command execution failed".into()),
        },
        kind => {
            let mut arguments = call.clone();
            if let Some(fields) = arguments.as_object_mut() {
                for field in ["id", "type", "status", "turn_id", "error"] {
                    fields.remove(field);
                }
            }
            Tool {
                name: kind.to_owned(),
                kind: "extension",
                call_id,
                arguments,
                result: None,
                failure: message_text(&call["error"])
                    .or_else(|| failed.then(|| format!("{kind} failed"))),
            }
        }
    }
}

/// Items as OpenTelemetry GenAI messages: `{role, parts}`, with consecutive
/// items from one role joined into one message.
fn messages<'a>(items: impl IntoIterator<Item = &'a Record>, results: &[Value]) -> Value {
    let mut messages: Vec<(String, Vec<Value>)> = results
        .iter()
        .map(|part| ("tool".to_owned(), vec![part.clone()]))
        .collect();
    for item in items {
        let data = &item.data;
        let text_parts = || -> Vec<Value> {
            data["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| match part["type"].as_str() {
                    Some("input_text" | "output_text") => {
                        Some(json!({"type":"text","content":part["text"]}))
                    }
                    Some("input_image") => {
                        Some(json!({"type":"uri","modality":"image","uri":part["image_url"]}))
                    }
                    _ => None,
                })
                .collect()
        };
        let (role, parts) = match item.kind() {
            "message" => (
                data["role"].as_str().unwrap_or("assistant").to_owned(),
                text_parts(),
            ),
            "agent_message" => ("user".to_owned(), text_parts()),
            "reasoning" => {
                let summary = data["summary"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if summary.is_empty() {
                    continue;
                }
                (
                    "assistant".to_owned(),
                    vec![json!({"type":"reasoning","content":summary})],
                )
            }
            "function_call_output" => (
                "tool".to_owned(),
                vec![
                    json!({"type":"tool_call_response","id":data["call_id"],"response":data["output"]}),
                ],
            ),
            _ => {
                let call = tool(item, &[]);
                (
                    "assistant".to_owned(),
                    vec![
                        json!({"type":"tool_call","id":call.call_id,"name":call.name,"arguments":call.arguments}),
                    ],
                )
            }
        };
        if parts.is_empty() {
            continue;
        }
        match messages.last_mut() {
            Some((last, existing)) if *last == role => existing.extend(parts),
            _ => messages.push((role, parts)),
        }
    }
    json!(
        messages
            .into_iter()
            .map(|(role, parts)| json!({"role":role,"parts":parts}))
            .collect::<Vec<_>>()
    )
}

/// An agent's span, then the generation and tool spans of its turns beneath it.
fn agent_spans(
    context: &Context,
    parent: Option<&str>,
    agent: &Agent,
    turns: &[&Record],
    items: &[Record],
    generations: &[Generation],
    completeness: &mut Completeness,
) -> Vec<Value> {
    let trace_id = context.trace_id;
    let own = span_id(trace_id, &agent.key);
    let start = turns
        .iter()
        .map(|turn| turn.span().0)
        .min()
        .unwrap_or_default();
    let end = turns
        .iter()
        .map(|turn| turn.span().1)
        .max()
        .unwrap_or(start);
    let last = turns.last().map(|turn| &turn.data).unwrap_or(&Value::Null);
    let status = last["status"].as_str().unwrap_or_default();
    let owned_by_agent: Vec<&Record> = items
        .iter()
        .filter(|item| turns.iter().any(|turn| turn.id == item.turn_id))
        .collect();
    let mut attributes = vec![
        attribute("gen_ai.operation.name", &json!("invoke_agent")),
        attribute("gen_ai.agent.id", &json!(agent.id)),
        attribute(
            "gen_ai.input.messages",
            &messages(
                owned_by_agent
                    .iter()
                    .copied()
                    .filter(|item| item.data["role"] == "user" || item.kind() == "agent_message"),
                &[],
            ),
        ),
        attribute(
            "gen_ai.output.messages",
            &messages(
                owned_by_agent
                    .iter()
                    .copied()
                    .rfind(|item| item.kind() == "message" && item.data["role"] == "assistant"),
                &[],
            ),
        ),
    ];
    attributes.extend(context.ids(context.turn_id));
    attributes.extend([
        attribute("openai.agents.agent_type", &json!(agent.kind)),
        attribute("openai.agents.status", &json!(status)),
    ]);
    for (key, value) in [
        ("gen_ai.agent.name", agent.name.as_ref()),
        ("gen_ai.system_instructions", agent.instructions.as_ref()),
        ("gen_ai.request.model", agent.model.as_ref()),
    ] {
        if let Some(value) = value {
            attributes.push(attribute(key, &json!(value)));
        }
    }
    if let Some([input, cached, output, reasoning, _]) = usage(turns) {
        attributes.extend([
            attribute("gen_ai.usage.input_tokens", &json!(input)),
            attribute("gen_ai.usage.cache_read.input_tokens", &json!(cached)),
            attribute("gen_ai.usage.output_tokens", &json!(output)),
            attribute("gen_ai.usage.reasoning.output_tokens", &json!(reasoning)),
        ]);
    }
    // Spans that ended well leave their status unset.
    let outcome = match status {
        "failed" => (2, last["error"]["message"].as_str().map(str::to_owned)),
        _ => (0, None),
    };
    let name = match &agent.name {
        Some(name) => format!("invoke_agent {name}"),
        None => "invoke_agent".to_owned(),
    };
    let mut spans = vec![span(
        trace_id,
        &own,
        parent,
        &name,
        (start, end),
        outcome,
        attributes,
    )];
    for turn in turns {
        let bounds = turn.span();
        let owned: Vec<&Record> = items
            .iter()
            .filter(|item| item.turn_id == turn.id)
            .collect();
        let usage: Vec<&Generation> = generations
            .iter()
            .filter(|generation| generation.turn_id == turn.id)
            .collect();
        let responses = if !usage.is_empty()
            && usage
                .iter()
                .all(|generation| generation.completed_ms.is_some())
        {
            timed(context, turn, &owned, &usage)
        } else {
            let (responses, matched) = inferred(turn, &owned, &usage);
            if !usage.is_empty() {
                completeness.ordering_issues.push(if matched {
                    format!("responses in turn {} were inferred from its items", turn.id)
                } else {
                    format!(
                        "turn {} reported {} responses but its items show {}; their usage is omitted",
                        turn.id,
                        usage.len(),
                        responses.len()
                    )
                });
            }
            responses
        };
        let mut results: Vec<Value> = Vec::new();
        for (index, response) in responses.iter().enumerate() {
            let model = response
                .generation
                .and_then(|generation| generation.model.as_ref())
                .or(agent.model.as_ref());
            let mut attributes = vec![
                attribute("gen_ai.operation.name", &json!("chat")),
                attribute(
                    "gen_ai.input.messages",
                    &messages(response.inputs.iter().copied(), &results),
                ),
                attribute(
                    "gen_ai.output.messages",
                    &messages(response.outputs.iter().copied(), &[]),
                ),
            ];
            attributes.extend(context.ids(&turn.id));
            if let Some(model) = model {
                attributes.push(attribute("gen_ai.request.model", &json!(model)));
            }
            if let Some(generation) = response.generation {
                attributes.extend([
                    attribute("gen_ai.usage.input_tokens", &json!(generation.input_tokens)),
                    attribute(
                        "gen_ai.usage.output_tokens",
                        &json!(generation.output_tokens),
                    ),
                    attribute(
                        "openai.agents.usage.total_tokens",
                        &json!(generation.total_tokens),
                    ),
                ]);
                for (key, value) in [
                    (
                        "gen_ai.usage.cache_read.input_tokens",
                        generation.cached_tokens,
                    ),
                    (
                        "gen_ai.usage.reasoning.output_tokens",
                        generation.reasoning_tokens,
                    ),
                ] {
                    if let Some(value) = value {
                        attributes.push(attribute(key, &json!(value)));
                    }
                }
                if let Some(model) = &generation.model {
                    attributes.push(attribute("gen_ai.response.model", &json!(model)));
                }
            }
            let mut chat = span(
                trace_id,
                &span_id(
                    trace_id,
                    &format!("{}:generation:{}:{index}", agent.key, turn.id),
                ),
                Some(&own),
                &model.map_or_else(|| "chat".to_owned(), |model| format!("chat {model}")),
                response.span,
                (0, None),
                attributes,
            );
            // A model request is a call out of the service.
            chat["kind"] = json!(3);
            spans.push(chat);
            // The next response sees the results of this one's tool calls;
            // function results arrive as inputs of their own.
            results = response
                .outputs
                .iter()
                .filter(|item| item.is_tool() && item.kind() != "function_call")
                .map(|item| {
                    let call = tool(item, &owned);
                    json!({"type":"tool_call_response","id":call.call_id,"response":call.result})
                })
                .collect();
        }
        for item in owned.iter().filter(|item| item.is_tool()) {
            let call = tool(item, &owned);
            let status = item.data["status"].as_str().unwrap_or_default();
            let mut attributes = vec![
                attribute("gen_ai.operation.name", &json!("execute_tool")),
                attribute("gen_ai.tool.name", &json!(call.name)),
                attribute("gen_ai.tool.type", &json!(call.kind)),
                attribute("gen_ai.tool.call.id", &json!(call.call_id)),
                attribute("gen_ai.tool.call.arguments", &call.arguments),
            ];
            attributes.extend(context.ids(&turn.id));
            attributes.push(attribute("openai.agents.status", &json!(status)));
            let outcome = match (&call.failure, &call.result) {
                (Some(message), result) => {
                    attributes.push(attribute("error.type", &json!("_OTHER")));
                    if let Some(result) = result {
                        attributes.push(attribute("openai.managed_agents.error.data", result));
                    }
                    (2, Some(message.clone()))
                }
                (None, Some(result)) => {
                    attributes.push(attribute("gen_ai.tool.call.result", result));
                    (0, None)
                }
                (None, None) => (0, None),
            };
            if status == "in_progress"
                || (item.kind() == "function_call"
                    && call.result.is_none()
                    && turn.data["status"] != "in_progress")
            {
                completeness.missing_outputs.push(call.call_id.clone());
            }
            spans.push(span(
                trace_id,
                &span_id(trace_id, &format!("{}:tool:{}", agent.key, item.id)),
                Some(&own),
                &format!("execute_tool {}", call.name),
                times(item, bounds),
                outcome,
                attributes,
            ));
        }
    }
    spans
}
