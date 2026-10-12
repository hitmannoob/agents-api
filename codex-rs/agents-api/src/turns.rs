//! Turn outcomes the worker cannot report itself: documented failure codes for
//! Codex errors, and turns that fail before the worker starts them.
use crate::ApiError;
use crate::State;
use serde_json::Value;
use serde_json::json;
use uuid::Uuid;

/// Map a Codex turn error to the documented failure categories. A kind without
/// a public counterpart is classified by the provider's HTTP status when the
/// message carries one, and is otherwise `internal_error`.
pub(crate) fn turn_error(error: &Value) -> Value {
    let raw = error["message"].as_str().unwrap_or("turn failed");
    let provider = provider_error(raw);
    let info = &error["codexErrorInfo"];
    let kind = info.as_str().or_else(|| {
        info.as_object()
            .and_then(|info| info.keys().next())
            .map(String::as_str)
    });
    let status = info
        .as_object()
        .and_then(|info| info.values().next())
        .and_then(|details| details["httpStatusCode"].as_u64());
    let code = match kind {
        Some("contextWindowExceeded") => "context_length_exceeded",
        Some("sessionBudgetExceeded") => "session_budget_exceeded",
        Some("usageLimitExceeded") => "usage_limit_exceeded",
        Some("rateLimitExceeded") => "rate_limit_exceeded",
        Some("serverOverloaded") => "server_overloaded",
        Some("cyberPolicy") => "cyber_policy",
        Some(
            "httpConnectionFailed"
            | "responseStreamConnectionFailed"
            | "responseStreamDisconnected",
        ) => "connection_failed",
        Some("responseTooManyFailedAttempts") if status == Some(429) => "rate_limit_exceeded",
        Some("internalServerError" | "responseTooManyFailedAttempts") => "server_error",
        Some("unauthorized") => "authentication_error",
        Some("badRequest") => "invalid_request",
        Some("sandboxError") => "sandbox_error",
        Some("activeTurnNotSteerable") => "active_turn_not_steerable",
        _ => match provider.as_ref().and_then(|provider| provider.status) {
            Some(401 | 403) => "authentication_error",
            Some(429) => "rate_limit_exceeded",
            Some(400..=499) => "invalid_request",
            Some(500..=599) => "server_error",
            _ => "internal_error",
        },
    };
    let message = match provider {
        Some(provider) => {
            // Operators keep the full text; clients get the provider's message.
            tracing::info!(
                error = raw,
                "provider error details withheld from the client"
            );
            provider.message
        }
        None => raw.to_owned(),
    };
    json!({"code":code,"message":message})
}

/// A model provider's error response found inside a Codex error message.
struct ProviderError {
    status: Option<u64>,
    message: String,
}

/// Diagnostics Codex appends to a provider error, in this order. They name the
/// provider endpoint and account, so clients do not see them.
const PROVIDER_SUFFIXES: [&str; 5] = [
    ", url: ",
    ", cf-ray: ",
    ", request id: ",
    ", auth error: ",
    ", auth error code: ",
];
/// The longest provider message passed to clients.
const MAX_PROVIDER_MESSAGE_CHARS: usize = 500;
const PROVIDER_FALLBACK: &str = "the model provider rejected the request";

/// Recognize a provider rejection in a Codex error message and keep only what
/// a client should see. Codex reports one either as its own status text,
/// `unexpected status 400 Bad Request: <message>` followed by diagnostics, or,
/// for a failure inside a response stream, as the provider's raw body. Bodies
/// can carry account details (OpenRouter's names the account's user ID), so
/// only their error message is kept. Anything else is Codex's own message.
fn provider_error(message: &str) -> Option<ProviderError> {
    let (message, status) = match message.strip_prefix("unexpected status ") {
        Some(rest) => (
            rest.split_once(": ").map_or("", |(_, text)| text),
            rest.split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|digits| digits.parse().ok()),
        ),
        None => (message, None),
    };
    // Codex appends the same diagnostics to a provider's friendly message.
    let end = PROVIDER_SUFFIXES
        .iter()
        .filter_map(|suffix| message.find(suffix))
        .min();
    let diagnostics = end.is_some();
    let message = &message[..end.unwrap_or(message.len())];
    let body = message
        .find('{')
        .zip(message.rfind('}'))
        .and_then(|(start, end)| message.get(start..=end))
        .and_then(|body| serde_json::from_str::<Value>(body).ok())
        .filter(Value::is_object);
    if status.is_none() && body.is_none() && !diagnostics {
        return None;
    }
    let (text, status) = match &body {
        Some(body) => {
            let detail = &body["error"];
            let text = detail["message"]
                .as_str()
                .or_else(|| detail.as_str())
                .or_else(|| body["message"].as_str())
                .unwrap_or(PROVIDER_FALLBACK);
            let status = status
                .or_else(|| detail["code"].as_u64())
                .or_else(|| body["status"].as_u64());
            (text, status)
        }
        None => (message, status),
    };
    let text = text.trim();
    // An HTML error page or an empty body says nothing a client can use.
    let readable = !text.is_empty() && !text.starts_with('<');
    let message = if readable {
        let mut message: String = text.chars().take(MAX_PROVIDER_MESSAGE_CHARS).collect();
        if message.len() < text.len() {
            message.push('…');
        }
        message
    } else {
        PROVIDER_FALLBACK.to_owned()
    };
    Some(ProviderError { status, message })
}

/// A turn record that has not started.
fn queued_record(id: &str, agent_id: &str, turn_id: &str) -> Value {
    json!({"id":turn_id,"object":"agent.session.turn","session_id":id,"agent_id":agent_id,"created_at":crate::contract::now(),
        "started_at":null,"completed_at":null,"status":"queued","usage":null,"subagent_id":null,"error":null})
}

/// The session's queued turn: the one its waiting input will start.
pub(crate) async fn queued(
    connection: &mut sqlx::SqliteConnection,
    id: &str,
) -> anyhow::Result<Option<Value>> {
    let data: Option<String> = sqlx::query_scalar("SELECT data FROM public_records WHERE session_id = ? AND kind = 'turn' AND subagent_id IS NULL AND json_extract(data, '$.status') = 'queued' ORDER BY seq LIMIT 1")
        .bind(id)
        .fetch_optional(&mut *connection)
        .await?;
    Ok(data.as_deref().map(serde_json::from_str).transpose()?)
}

/// Queue a turn for input that waits for the session's executor, unless a
/// turn is already running or queued: the input then steers that turn when it
/// starts. Returns the events announcing the turn.
pub(crate) async fn queue(
    connection: &mut sqlx::SqliteConnection,
    id: &str,
) -> anyhow::Result<Vec<Value>> {
    let busy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_records WHERE session_id = ? AND kind = 'turn' AND subagent_id IS NULL AND json_extract(data, '$.status') IN ('queued', 'in_progress', 'waiting'))")
        .bind(id)
        .fetch_one(&mut *connection)
        .await?;
    let agent_id: Option<String> = sqlx::query_scalar(
        "SELECT json_extract(data, '$.agent.id') FROM public_sessions WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *connection)
    .await?;
    let (false, Some(agent_id)) = (busy, agent_id) else {
        return Ok(Vec::new());
    };
    let turn_id = format!("turn_{}", Uuid::new_v4());
    let turn = queued_record(id, &agent_id, &turn_id);
    crate::records::save(
        &mut *connection,
        id,
        "turn",
        &turn,
        &turn_id,
        /*subagent*/ None,
    )
    .await?;
    Ok(vec![
        json!({"type":"agent.session.turn.created","session_id":id,"turn_id":turn_id,"turn":turn}),
    ])
}

/// End the session's queued turn without starting it: cancelled, or failed
/// with `error`. Returns the events reporting it.
pub(crate) async fn end_queued(
    connection: &mut sqlx::SqliteConnection,
    id: &str,
    error: Option<Value>,
) -> anyhow::Result<Vec<Value>> {
    let Some(mut turn) = queued(&mut *connection, id).await? else {
        return Ok(Vec::new());
    };
    let turn_id = turn["id"].as_str().unwrap_or_default().to_owned();
    let status = if error.is_some() {
        "failed"
    } else {
        "cancelled"
    };
    turn["status"] = json!(status);
    turn["completed_at"] = json!(crate::contract::now());
    turn["error"] = error.clone().unwrap_or(Value::Null);
    crate::records::save(
        &mut *connection,
        id,
        "turn",
        &turn,
        &turn_id,
        /*subagent*/ None,
    )
    .await?;
    let mut events = Vec::new();
    if let Some(error) = error {
        events.push(json!({"type":"error","session_id":id,"error":{"type":"error","code":error["code"],"message":error["message"],"param":null}}));
    }
    events.push(json!({"type":format!("agent.session.turn.{status}"),"session_id":id,"turn_id":turn_id,"turn":turn}));
    Ok(events)
}

/// Record a turn that failed before the worker could start it, such as when a
/// required MCP server cannot initialize: the session's queued turn, or else
/// a new one, since the worker never assigned it an ID. The session returns
/// to idle as after any failed turn. Returns `false` for sessions without
/// public records.
pub(crate) async fn fail_unstarted_turn(
    state: &State,
    id: &str,
    error: Value,
) -> Result<bool, ApiError> {
    let mut tx = state
        .store
        .0
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(anyhow::Error::from)?;
    let agent_id: Option<String> = sqlx::query_scalar(
        "SELECT json_extract(data, '$.agent.id') FROM public_sessions WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    let Some(agent_id) = agent_id else {
        return Ok(false);
    };
    let mut events = Vec::new();
    if queued(&mut tx, id).await?.is_none() {
        let turn_id = format!("turn_{}", Uuid::new_v4());
        let turn = queued_record(id, &agent_id, &turn_id);
        crate::records::save(
            &mut *tx, id, "turn", &turn, &turn_id, /*subagent*/ None,
        )
        .await?;
        events.push(json!({"type":"agent.session.turn.created","session_id":id,"turn_id":turn_id,"turn":turn}));
    }
    events.extend(end_queued(&mut tx, id, Some(error)).await?);
    events.extend(crate::records::transition(&mut tx, id, "idle", /*error*/ None).await?);
    tx.commit().await.map_err(anyhow::Error::from)?;
    for event in events {
        crate::records::emit(state, event);
    }
    Ok(true)
}

#[cfg(test)]
#[path = "turns_tests.rs"]
mod tests;
