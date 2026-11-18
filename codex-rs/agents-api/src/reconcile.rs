//! Correct work that a connection loss resolved provisionally against the
//! app-server's authoritative rollout history, once a backend reconnects. This
//! covers turns failed only by the disconnect and function calls whose delivery
//! became uncertain; both are decided from authoritative turn status, which
//! `thread/turns/list` serves from persisted rollout history (so no resume is
//! needed on a freshly restarted worker).
use crate::State;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Error code that [`crate::records::disconnected`] stamps on turns failed only
/// because the backend connection dropped. Reconciliation revisits exactly
/// these turns; a turn the model genuinely failed carries a different code, so
/// it is never re-examined here.
pub(crate) const CONNECTION_LOST_CODE: &str = "connection_failed";

/// Reconcile every session left with provisional state by a disconnect. A no-op
/// on the common fresh-start case where nothing was interrupted. Best-effort: a
/// session whose thread cannot be listed keeps its provisional state.
pub(crate) async fn run(state: &State) -> anyhow::Result<()> {
    // Sessions whose turns were failed only by the connection loss, plus those
    // holding function calls whose delivery the loss left uncertain.
    let mut sessions: BTreeSet<String> = sqlx::query_scalar(
        "SELECT DISTINCT session_id FROM public_records WHERE kind = 'turn' AND json_extract(data, '$.error.code') = ?",
    )
    .bind(CONNECTION_LOST_CODE)
    .fetch_all(&state.store.0)
    .await?
    .into_iter()
    .collect();
    let calls: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT session_id FROM tool_calls WHERE status IN ('unavailable', 'submitting')",
    )
    .fetch_all(&state.store.0)
    .await?;
    sessions.extend(calls);
    for session_id in sessions {
        if let Err(error) = reconcile_session(state, &session_id).await {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "reconciling session failed"
            );
        }
    }
    Ok(())
}

async fn reconcile_session(state: &State, session_id: &str) -> anyhow::Result<()> {
    let thread_id: Option<String> =
        sqlx::query_scalar("SELECT thread_id FROM sessions WHERE id = ?")
            .bind(session_id)
            .fetch_optional(&state.store.0)
            .await?
            .flatten();
    let Some(thread_id) = thread_id else {
        return Ok(());
    };
    let response = state
        .rpc(
            "thread/turns/list",
            json!({"threadId": thread_id, "limit": 100, "itemsView": "summary"}),
        )
        .await
        .map_err(|error| anyhow::anyhow!(error.1))?;
    // History names turns by Codex's IDs; records by public ones, which are
    // bare for turns recorded before public IDs had a prefix.
    let authoritative: HashMap<String, &Value> = response["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|turn| turn["id"].as_str().map(|id| (id, turn)))
        .flat_map(|(id, turn)| [(state.turn_aliases.public(id), turn), (id.to_owned(), turn)])
        .collect();

    // Correct provisionally-failed turns: a turn the rollout records completed
    // is recovered; one still unfinished keeps its provisional failure, because
    // an interrupted turn's completion cannot be established.
    let provisional: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM public_records WHERE session_id = ? AND kind = 'turn' AND json_extract(data, '$.error.code') = ?",
    )
    .bind(session_id)
    .bind(CONNECTION_LOST_CODE)
    .fetch_all(&state.store.0)
    .await?;
    let mut recovered_turn = false;
    for turn_id in provisional {
        let Some(turn) = authoritative.get(turn_id.as_str()) else {
            continue; // Never persisted authoritatively; keep it failed.
        };
        let (status, error) = match turn["status"].as_str() {
            Some("completed") => ("completed", Value::Null),
            // History reports a turn a worker loss cut short as interrupted
            // too; only one the caller asked to cancel was cancelled.
            Some("interrupted")
                if crate::lock(&state.cancel_requested).remove(turn_id.as_str()) =>
            {
                ("cancelled", Value::Null)
            }
            Some("failed") => ("failed", crate::turns::turn_error(&turn["error"])),
            // inProgress or unknown: completion cannot be established.
            _ => continue,
        };
        let Some(data): Option<String> = sqlx::query_scalar(
            "SELECT data FROM public_records WHERE session_id = ? AND kind = 'turn' AND id = ?",
        )
        .bind(session_id)
        .bind(&turn_id)
        .fetch_optional(&state.store.0)
        .await?
        else {
            continue;
        };
        let mut record: Value = serde_json::from_str(&data)?;
        record["status"] = json!(status);
        record["error"] = error;
        if let Some(completed_at) = turn["completed_at"].as_i64() {
            record["completed_at"] = json!(completed_at);
        }
        sqlx::query(
            "UPDATE public_records SET data = ? WHERE session_id = ? AND kind = 'turn' AND id = ?",
        )
        .bind(record.to_string())
        .bind(session_id)
        .bind(&turn_id)
        .execute(&state.store.0)
        .await?;
        recovered_turn = true;
    }

    // Recover function calls whose delivery acknowledgment was lost. Only a
    // completed turn is conclusive: a turn cannot complete unless every call it
    // contained was resolved, so the result delivered even though the ack never
    // returned, and the stored receipt is restored. Any other status —
    // interrupted, failed, still open, or absent from history — leaves the call
    // unresolved for the caller to reconcile, never claiming a delivery or
    // recreating a waiter from a state that cannot be established.
    let uncertain: Vec<(String, String)> = sqlx::query_as(
        "SELECT turn_id, call_id FROM tool_calls WHERE session_id = ? AND status IN ('unavailable', 'submitting')",
    )
    .bind(session_id)
    .fetch_all(&state.store.0)
    .await?;
    for (turn_id, call_id) in uncertain {
        if authoritative
            .get(turn_id.as_str())
            .is_none_or(|turn| turn["status"] != "completed")
        {
            continue;
        }
        sqlx::query(
            "UPDATE tool_calls SET status = 'submitted' WHERE session_id = ? AND turn_id = ? AND call_id = ?",
        )
        .bind(session_id)
        .bind(&turn_id)
        .bind(&call_id)
        .execute(&state.store.0)
        .await?;
    }

    if recovered_turn {
        // A resolved terminal turn leaves the session ready for input again,
        // mirroring the live path that idles a session after any terminal turn.
        crate::records::session_status(state, session_id, "idle", /*error*/ None)
            .await
            .map_err(|error| anyhow::anyhow!(error.1))?;
    }
    Ok(())
}

/// Input that continues a turn a worker loss interrupted. The model sees the
/// conversation up to the loss, including completed tool calls; the one in
/// flight may or may not have taken effect.
pub(crate) const CONTINUE_INPUT: &str = "The service restarted the agent's worker while your previous turn was running, so that turn was interrupted. The conversation so far is intact, but the last action you started may or may not have finished: check its effects before repeating it. Continue the task from where you left off.";

/// Continue, once a backend serves again, the sessions whose latest turn a
/// worker loss interrupted, unless the operator disabled it. Runs in the
/// background.
pub(crate) fn continue_interrupted(state: &Arc<State>) {
    if state.continuations.load(Ordering::Relaxed) == 0 {
        return;
    }
    let state = Arc::clone(state);
    tokio::spawn(async move {
        if let Err(error) = continue_sessions(&state).await {
            tracing::warn!(
                error = format!("{error:#}"),
                "continuing interrupted turns failed"
            );
        }
    });
}

async fn continue_sessions(state: &Arc<State>) -> anyhow::Result<()> {
    let sessions: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM public_sessions WHERE json_extract(data, '$.status') = 'failed' AND json_extract(data, '$.error') = 'backend connection lost'",
    )
    .fetch_all(&state.store.0)
    .await?;
    for session_id in sessions {
        // Passes may overlap; one session is decided by one pass at a time,
        // and the decision re-reads its state, so it is continued once.
        if !crate::lock(&state.continuing).insert(session_id.clone()) {
            continue;
        }
        let result = continue_session(state, &session_id).await;
        crate::lock(&state.continuing).remove(&session_id);
        if let Err(error) = result {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "interrupted turn not continued"
            );
        }
    }
    Ok(())
}

async fn continue_session(state: &Arc<State>, session_id: &str) -> anyhow::Result<()> {
    let attempts = i64::from(state.continuations.load(Ordering::Relaxed));
    let lost: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM public_sessions WHERE id = ? AND json_extract(data, '$.status') = 'failed' AND json_extract(data, '$.error') = 'backend connection lost')",
    )
    .bind(session_id)
    .fetch_one(&state.store.0)
    .await?;
    if !lost {
        return Ok(());
    }
    // Newest first. Only a turn the loss interrupted is continued, and only
    // while fewer than `attempts` consecutive turns before it were
    // interrupted too, so a turn that keeps killing its worker stops.
    let turns: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT id, json_extract(data, '$.error.message') FROM public_records WHERE session_id = ? AND kind = 'turn' \
         ORDER BY json_extract(data, '$.created_at') DESC, rowid DESC LIMIT ?",
    )
    .bind(session_id)
    .bind(attempts + 1)
    .fetch_all(&state.store.0)
    .await?;
    let interrupted = turns
        .iter()
        .take_while(|(_, message)| message.as_deref() == Some("backend connection lost"))
        .count() as i64;
    if interrupted == 0 || interrupted > attempts {
        return Ok(());
    }
    // Function calls the caller still has to resolve cannot be continued past.
    let latest = &turns[0].0;
    let unresolved: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM tool_calls WHERE session_id = ? AND turn_id IN (?, ?) AND status IN ('pending', 'submitting', 'unavailable'))",
    )
    .bind(session_id)
    .bind(latest)
    .bind(state.turn_aliases.codex(latest))
    .fetch_one(&state.store.0)
    .await?;
    if unresolved {
        return Ok(());
    }
    let input = vec![json!({"type": "text", "text": CONTINUE_INPUT})];
    crate::environments::submit(state, session_id, input)
        .await
        .map_err(|error| anyhow::anyhow!(error.1))?;
    tracing::info!(
        session_id,
        interrupted_turn = latest,
        "continuing interrupted turn"
    );
    Ok(())
}
