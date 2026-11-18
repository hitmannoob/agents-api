//! Managed environment lifecycle: snapshots, reset, suspension, resumption,
//! and expiry.
//!
//! A loop checks each provisioned managed environment every
//! [`Lifecycle::tick`](crate::managed::Lifecycle):
//! - A connected environment is snapshotted once it connects, after a busy
//!   spell ends, and every `snapshot_interval` while it stays busy. Turns
//!   starting and ending, and files written through the API, record activity
//!   and wake the loop, so the snapshot follows the work at once rather than
//!   at the next tick.
//! - One whose executor stayed disconnected for `death_grace` is checked with
//!   the provider; if its sandbox is gone (or never reconnects), a new sandbox
//!   is provisioned from the latest snapshot (or, before the first one, from
//!   its original setup) under the same environment ID and
//!   `agent.session.environment.reset` reports the new `reset_count`.
//!   Conversation history survives; work since the snapshot does not.
//! - One idle for `idle_suspend` is snapshotted and its sandbox stopped
//!   (`suspended`); the session's next input restores it from the snapshot.
//! - One suspended longer than `snapshot_ttl` has lost its snapshot
//!   (`expired`), and later input is refused.
//!
//! Restoring needs the environment's confidential variables, which are kept
//! encrypted in the secrets store when the operator configured a vault
//! passphrase.
use crate::State;
use crate::registry::ExecutorState;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::Ordering;

/// Start the lifecycle loop once.
pub(crate) fn spawn(state: &Arc<State>) {
    if state.lifecycle_started.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(run(Arc::downgrade(state)));
}

async fn run(state: Weak<State>) {
    loop {
        let Some(strong) = state.upgrade() else {
            return;
        };
        let tick = crate::lock(&strong.lifecycle).tick;
        let wake = Arc::clone(&strong.lifecycle_wake);
        if let Err(error) = check(&strong).await {
            tracing::warn!(
                error = format!("{error:#}"),
                "managed environment lifecycle check failed"
            );
        }
        drop(strong);
        // A wake during the check is kept, so it runs again at once.
        tokio::select! {
            () = tokio::time::sleep(tick) => {}
            () = wake.notified() => {}
        }
    }
}

type Row = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    bool,
);

async fn check(state: &Arc<State>) -> anyhow::Result<()> {
    let Some(provider) = state.provider() else {
        return Ok(());
    };
    let config = *crate::lock(&state.lifecycle);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, session_id, sandbox_id, snapshot_id, snapshot_ms, activity_ms, disconnected_ms, suspended_ms, waiting \
         FROM environments WHERE provider IS NOT NULL AND provisioned = 1 AND failed = 0 AND expired = 0",
    )
    .fetch_all(&state.store.0)
    .await?;
    for (
        environment_id,
        session_id,
        sandbox_id,
        snapshot_id,
        snapshot_ms,
        activity_ms,
        disconnected_ms,
        suspended_ms,
        waiting,
    ) in rows
    {
        let now = crate::records::now_ms();
        if crate::lock(&state.reprovisioning).contains(&environment_id) {
            continue;
        }
        if let Some(suspended_ms) = suspended_ms {
            if now - suspended_ms >= config.snapshot_ttl.as_millis() as i64 {
                expire(state, &session_id, &environment_id).await?;
            }
            continue;
        }
        let status: Option<String> = sqlx::query_scalar(
            "SELECT json_extract(data, '$.status') FROM public_sessions WHERE id = ?",
        )
        .bind(&session_id)
        .fetch_optional(&state.store.0)
        .await?
        .flatten();
        let busy = waiting || matches!(status.as_deref(), Some("in_progress" | "requires_action"));
        let activity_ms = if busy {
            sqlx::query("UPDATE environments SET activity_ms = ? WHERE id = ?")
                .bind(now)
                .bind(&environment_id)
                .execute(&state.store.0)
                .await?;
            now
        } else {
            activity_ms.unwrap_or(now)
        };
        let Some(sandbox_id) = sandbox_id else {
            continue;
        };
        let connected = state.registry.state(&environment_id) == Some(ExecutorState::Connected);
        if connected {
            let snapshot_ms = snapshot_ms.unwrap_or(0);
            let due = snapshot_id.is_none()
                || (activity_ms > snapshot_ms
                    && (!busy || now - snapshot_ms >= config.snapshot_interval.as_millis() as i64));
            let mut fresh = snapshot_id.is_some() && !due;
            if due {
                match provider
                    .snapshot(sandbox_id.clone(), config.snapshot_ttl)
                    .await
                {
                    Ok(snapshot) => {
                        sqlx::query(
                            "UPDATE environments SET snapshot_id = ?, snapshot_ms = ? WHERE id = ?",
                        )
                        .bind(snapshot)
                        .bind(now)
                        .bind(&environment_id)
                        .execute(&state.store.0)
                        .await?;
                        fresh = true;
                    }
                    Err(error) => tracing::warn!(
                        environment_id,
                        error = format!("{error:#}"),
                        "environment snapshot failed"
                    ),
                }
            }
            if !busy && fresh && now - activity_ms >= config.idle_suspend.as_millis() as i64 {
                suspend(state, &session_id, &environment_id, sandbox_id, activity_ms).await?;
            }
        } else if let Some(disconnected_ms) = disconnected_ms {
            let grace = config.death_grace.as_millis() as i64;
            if now - disconnected_ms < grace {
                continue;
            }
            // A sandbox that is gone, or one whose executor never came back,
            // is replaced.
            let gone = match provider.running(sandbox_id.clone()).await {
                Ok(running) => !running,
                Err(error) => {
                    tracing::warn!(
                        environment_id,
                        error = format!("{error:#}"),
                        "sandbox status unknown"
                    );
                    false
                }
            };
            if gone || now - disconnected_ms >= grace * 4 {
                if !gone {
                    let _ = provider.destroy(sandbox_id).await;
                }
                reprovision(state, session_id, environment_id, /*reset*/ true);
            }
        }
    }
    Ok(())
}

/// Stop an idle environment's sandbox. `idle_since` is the activity the
/// decision saw; the snapshot before it takes seconds, so the decision is
/// re-checked under the session's input gate, and input that arrived meanwhile
/// (which records activity under the same gate) keeps the sandbox running.
async fn suspend(
    state: &Arc<State>,
    session_id: &str,
    environment_id: &str,
    sandbox_id: String,
    idle_since: i64,
) -> anyhow::Result<()> {
    let admission = state.input_gates.lock(session_id).await;
    // Recorded first, so the executor dropping as the sandbox stops is not
    // taken for a death.
    let suspended = sqlx::query(
        "UPDATE environments SET suspended_ms = ?, sandbox_id = NULL WHERE id = ? AND sandbox_id = ? \
         AND NOT waiting AND COALESCE(activity_ms, 0) <= ? AND (SELECT json_extract(data, '$.status') FROM public_sessions \
         WHERE id = environments.session_id) NOT IN ('in_progress', 'requires_action')",
    )
    .bind(crate::records::now_ms())
    .bind(environment_id)
    .bind(&sandbox_id)
    .bind(idle_since)
    .execute(&state.store.0)
    .await?
    .rows_affected()
        > 0;
    drop(admission);
    if !suspended {
        return Ok(());
    }
    if let Some(provider) = state.provider()
        && let Err(error) = provider.destroy(sandbox_id).await
    {
        tracing::warn!(
            environment_id,
            error = format!("{error:#}"),
            "suspended sandbox not destroyed"
        );
    }
    crate::environments::emit(
        state,
        session_id,
        environment_id,
        "openai_hosted",
        "suspended",
        None,
    );
    Ok(())
}

async fn expire(state: &Arc<State>, session_id: &str, environment_id: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE environments SET expired = 1 WHERE id = ?")
        .bind(environment_id)
        .execute(&state.store.0)
        .await?;
    state
        .secrets
        .discard(vec![
            crate::managed::env_secret(environment_id),
            crate::managed::setup_secret(environment_id),
        ])
        .await;
    crate::environments::emit(
        state,
        session_id,
        environment_id,
        "openai_hosted",
        "expired",
        None,
    );
    Ok(())
}

/// Bring a suspended environment back for the input that arrived for it.
pub(crate) fn resume(state: &Arc<State>, session_id: String, environment_id: String) {
    reprovision(state, session_id, environment_id, /*reset*/ false);
}

/// Provision a new sandbox for an environment from its latest snapshot, in
/// the background; at most one runs per environment.
fn reprovision(state: &Arc<State>, session_id: String, environment_id: String, reset: bool) {
    if !crate::lock(&state.reprovisioning).insert(environment_id.clone()) {
        return;
    }
    let state = Arc::clone(state);
    tokio::spawn(async move {
        let result = restore(&state, &session_id, &environment_id, reset).await;
        crate::lock(&state.reprovisioning).remove(&environment_id);
        if let Err(error) = result {
            let (code, verb) = if reset {
                (
                    "environment_reset_failed",
                    "replacing the environment's sandbox",
                )
            } else {
                ("environment_resume_failed", "resuming the environment")
            };
            let message = format!("{verb} failed: {error:#}");
            tracing::warn!(environment_id, error = message, "environment not restored");
            if let Err(error) =
                crate::environments::fail(&state, &session_id, &environment_id, code, &message)
                    .await
            {
                tracing::warn!(error = format!("{error:#}"), "restore failure not recorded");
            }
        }
    });
}

async fn restore(
    state: &Arc<State>,
    session_id: &str,
    environment_id: &str,
    reset: bool,
) -> anyhow::Result<()> {
    let provider = state
        .provider()
        .ok_or_else(|| anyhow::anyhow!("no environment provider is configured"))?;
    let (data, snapshot_id, registry_url, env_count, setup_count): (
        String,
        Option<String>,
        Option<String>,
        i64,
        i64,
    ) = sqlx::query_as(
        "SELECT data, snapshot_id, registry_url, env_count, setup_count FROM environments WHERE id = ?",
    )
    .bind(environment_id)
    .fetch_one(&state.store.0)
    .await?;
    // A sandbox lost before its first snapshot is rebuilt from scratch: the
    // packages, files, and setup commands it was created with run again.
    let setup = if snapshot_id.is_some() || setup_count == 0 {
        crate::managed::Setup {
            files: Vec::new(),
            setup_commands: Vec::new(),
        }
    } else {
        let not_kept = || {
            anyhow::anyhow!(
                "the environment had no snapshot yet, and its files and setup commands were not kept (the operator has no vault passphrase)"
            )
        };
        if !state.secrets.configured() {
            return Err(not_kept());
        }
        let stored = state
            .secrets
            .get(crate::managed::setup_secret(environment_id))
            .await
            .map_err(|error| anyhow::anyhow!(error.1))?
            .ok_or_else(not_kept)?;
        serde_json::from_value(stored)?
    };
    let registry_url =
        registry_url.ok_or_else(|| anyhow::anyhow!("the environment's registry URL is unknown"))?;
    let env: BTreeMap<String, String> = if env_count > 0 {
        let stored = state
            .secrets
            .get(crate::managed::env_secret(environment_id))
            .await
            .map_err(|error| anyhow::anyhow!(error.1))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "the environment's variables were not kept (the operator has no vault passphrase)"
                )
            })?;
        serde_json::from_value(stored)?
    } else {
        BTreeMap::new()
    };
    let public: Value = serde_json::from_str(&data)?;
    let token = crate::managed::new_token();
    sqlx::query("UPDATE environments SET token_sha256 = ?, executor_key = NULL WHERE id = ?")
        .bind(crate::managed::digest(&token))
        .bind(environment_id)
        .execute(&state.store.0)
        .await?;
    let request = crate::managed::ProvisionRequest {
        environment_id: environment_id.to_owned(),
        registry_url,
        token,
        container_size: public["container_size"]
            .as_str()
            .unwrap_or("medium")
            .to_owned(),
        packages: serde_json::from_value(public["packages"].clone()).unwrap_or_default(),
        env,
        files: setup.files,
        setup_commands: setup.setup_commands,
        network: serde_json::from_value(public["network"].clone())?,
        snapshot_id,
    };
    let sandbox_id = provider.provision(request).await?;
    let updated = sqlx::query(
        "UPDATE environments SET sandbox_id = ?, suspended_ms = NULL, disconnected_ms = NULL, activity_ms = ?, \
         reset_count = reset_count + ? WHERE id = ? RETURNING reset_count",
    )
    .bind(&sandbox_id)
    .bind(crate::records::now_ms())
    .bind(i64::from(reset))
    .bind(environment_id)
    .fetch_optional(&state.store.0)
    .await?;
    let Some(row) = updated else {
        // The session was deleted while its sandbox came up.
        let _ = provider.destroy(sandbox_id).await;
        return Ok(());
    };
    let reset_count: i64 = sqlx::Row::try_get(&row, 0)?;
    crate::lock(&state.stale_mcp).insert(session_id.to_owned());
    if reset {
        crate::records::emit(
            state,
            json!({"type": "agent.session.environment.reset", "session_id": session_id, "turn_id": null,
                "environment_id": environment_id, "reset_count": reset_count}),
        );
    }
    crate::environments::emit(
        state,
        session_id,
        environment_id,
        "openai_hosted",
        "ready",
        None,
    );
    Ok(())
}
