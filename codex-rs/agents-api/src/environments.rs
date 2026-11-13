//! Self-hosted environments: the executor a session runs its commands on,
//! reached through the service's environment registry.
//!
//! The caller owns the compute. A session records its environment, reports
//! the executor's connection as `agent.session.environment.*` events, and
//! attaches the environment to the worker before the session's first turn.
//! Input that arrives while the executor is offline waits up to five minutes
//! behind an `environment_connection` required action, then starts in order.
//! Input that times out is dropped, never replayed by a late connection.
use crate::ApiError;
use crate::State;
use crate::contract::invalid;
use crate::projects::Principal;
use crate::projects::Resource;
use crate::registry::ExecutorState;
use crate::resources::Environment;
use axum::Extension;
use axum::Json;
use axum::extract::Path;
use axum::extract::State as Extract;
use axum::http::StatusCode;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use uuid::Uuid;

/// How long input waits for a disconnected executor unless the operator
/// chooses otherwise: the contract's five minutes.
const CONNECTION_WAIT: Duration = Duration::from_secs(5 * 60);
const MAX_PATH_BYTES: usize = 4096;
const MAX_CAPABILITY_DIRECTORIES: usize = 32;
const TIMED_OUT: &str =
    "the environment did not connect in time; the input waiting for it was dropped";
const RESTARTED: &str =
    "the service restarted while input waited for the environment; that input was dropped";

/// Input batches one session may hold while its executor is offline.
const MAX_WAITING_INPUTS: usize = 32;
/// Serialized input bytes one session may hold while its executor is offline.
const MAX_WAITING_BYTES: usize = 8 * 1024 * 1024;
/// Serialized input bytes all sessions together may hold.
const MAX_TOTAL_WAITING_BYTES: usize = 256 * 1024 * 1024;

/// Input held while a session's executor is offline.
pub(crate) struct Waits {
    /// Waiting input by session ID.
    inputs: Mutex<HashMap<String, Wait>>,
    /// How long input waits for the executor.
    pub(crate) limit: Mutex<Duration>,
    /// Bytes of every queued input, released as each one leaves the queue.
    total_bytes: Arc<AtomicUsize>,
}

impl Default for Waits {
    fn default() -> Self {
        Self {
            inputs: Mutex::default(),
            limit: Mutex::new(CONNECTION_WAIT),
            total_bytes: Arc::default(),
        }
    }
}

struct Wait {
    inputs: VecDeque<Queued>,
    /// Tells this wait apart from a later one for the same session.
    generation: u64,
}

/// One queued input batch. Dropping it, whether it started, was cancelled,
/// timed out, or its session was deleted, returns its bytes to the budget.
struct Queued {
    items: Vec<Value>,
    bytes: usize,
    total: Arc<AtomicUsize>,
}

impl Drop for Queued {
    fn drop(&mut self) {
        self.total.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

impl Waits {
    /// Count `items` against the queue limits, or refuse them before they are
    /// held. `waiting` is the session's queue so far, if any.
    fn admit(&self, waiting: Option<&Wait>, items: Vec<Value>) -> Result<Queued, ApiError> {
        let bytes = serde_json::to_vec(&items).map_or(usize::MAX, |encoded| encoded.len());
        let (count, held) = waiting.map_or((0, 0), |wait| {
            (
                wait.inputs.len(),
                wait.inputs.iter().map(|queued| queued.bytes).sum(),
            )
        });
        let too_much = || {
            ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "too much input is waiting for this session's environment to connect".into(),
            )
        };
        if count >= MAX_WAITING_INPUTS || held + bytes > MAX_WAITING_BYTES {
            return Err(too_much());
        }
        let previous = self.total_bytes.fetch_add(bytes, Ordering::Relaxed);
        let queued = Queued {
            items,
            bytes,
            total: Arc::clone(&self.total_bytes),
        };
        if previous + bytes > MAX_TOTAL_WAITING_BYTES {
            // Dropping `queued` releases the reservation.
            return Err(too_much());
        }
        Ok(queued)
    }
}

/// A new environment from a `self_hosted` environment parameter.
pub(crate) fn parse(params: &Value) -> Result<Environment, ApiError> {
    let fields = params
        .as_object()
        .ok_or_else(|| invalid("environment must be an object"))?;
    if let Some(unknown) = fields.keys().find(|key| {
        !matches!(
            key.as_str(),
            "type" | "workspace_directory" | "capability_directories" | "skills"
        )
    }) {
        return Err(invalid(format!(
            "unknown field in self_hosted environment: {unknown}"
        )));
    }
    let capability_directories: Vec<String> = match &params["capability_directories"] {
        Value::Null => Vec::new(),
        directories => serde_json::from_value(directories.clone())
            .map_err(|_| invalid("capability_directories must be a list of paths"))?,
    };
    if capability_directories.len() > MAX_CAPABILITY_DIRECTORIES
        || capability_directories
            .iter()
            .any(|directory| !absolute(directory) || directory.len() > MAX_PATH_BYTES)
        || capability_directories.iter().collect::<HashSet<_>>().len()
            != capability_directories.len()
    {
        return Err(invalid(format!(
            "capability_directories must list at most {MAX_CAPABILITY_DIRECTORIES} distinct absolute paths"
        )));
    }
    let directory = params["workspace_directory"]
        .as_str()
        .ok_or_else(|| invalid("workspace_directory is required for self_hosted environments"))?;
    if !absolute(directory) || directory.len() > MAX_PATH_BYTES {
        return Err(invalid(format!(
            "workspace_directory must be an absolute path of at most {MAX_PATH_BYTES} bytes"
        )));
    }
    Ok(Environment::SelfHosted {
        id: format!("env_{}", Uuid::new_v4().simple()),
        cwd: directory.to_owned(),
        capability_directories,
    })
}

/// Whether `path` is absolute on the executor. It may run another OS, so either
/// form is accepted and resolution is left to it.
pub(crate) fn absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    !path.contains('\0')
        && (path.starts_with('/')
            || path.starts_with("\\\\")
            || (bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && matches!(bytes[2], b'\\' | b'/')))
}

/// Record a session's environment and return its public object.
pub(crate) async fn create(
    state: &State,
    session_id: &str,
    environment_id: &str,
    workspace_directory: &str,
    capability_directories: &[String],
    remote_url: String,
) -> anyhow::Result<Value> {
    let public = json!({"id": environment_id, "type": "self_hosted", "workspace_directory": workspace_directory,
        "capability_directories": capability_directories, "remote_url": remote_url});
    sqlx::query("INSERT INTO environments (id, session_id, data) VALUES (?, ?, ?)")
        .bind(environment_id)
        .bind(session_id)
        .bind(public.to_string())
        .execute(&state.store.0)
        .await?;
    Ok(public)
}

/// An environment's safe metadata and connection status. Installed files and
/// plugins describe hosted environments, so a self-hosted one has none; its
/// skills are the Skills API versions pinned to its session.
pub(crate) async fn retrieve(
    Extract(state): Extract<Arc<State>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    crate::projects::require(&state, &principal.project_id, &id, Resource::Environment).await?;
    let row: Option<(String, bool, bool)> =
        sqlx::query_as("SELECT session_id, connected_once, failed FROM environments WHERE id = ?")
            .bind(&id)
            .fetch_optional(&state.store.0)
            .await
            .map_err(anyhow::Error::from)?;
    let (session_id, connected_once, failed) =
        row.ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "environment not found".into()))?;
    // A connected executor wins; input that timed out waiting for one leaves
    // the environment failed until one connects. Self-hosted environments have
    // no lifetime of their own, so they never expire.
    let status = match state.registry.status(&id) {
        "connected" => "connected",
        _ if failed => "failed",
        "disconnected" => "disconnected",
        _ if connected_once => "disconnected",
        _ => "pending",
    };
    let skills = crate::session_skills::installed(&state, &session_id).await?;
    Ok(Json(
        json!({"id": id, "object": "agent.environment", "type": "self_hosted",
            "status": status, "files": [], "plugins": [], "skills": skills}),
    ))
}

/// Broadcast an environment state change on the session's stream.
pub(crate) fn emit(
    state: &State,
    session_id: &str,
    environment_id: &str,
    status: &str,
    error: Option<Value>,
) {
    crate::records::emit(
        state,
        json!({"type": format!("agent.session.environment.{status}"), "session_id": session_id, "turn_id": null,
            "environment": {"id": environment_id, "type": "self_hosted", "status": status, "error": error}}),
    );
}

/// Report the executor connecting and disconnecting until the environment is
/// deleted.
pub(crate) fn watch(state: &Arc<State>, session_id: String, environment_id: String) {
    let mut executor = state.registry.watch(&environment_id);
    let state = Arc::downgrade(state);
    tokio::spawn(async move {
        let mut connected = *executor.borrow_and_update() == ExecutorState::Connected;
        // Deleting the environment drops its slot, which ends this loop.
        while executor.changed().await.is_ok() {
            let now = *executor.borrow_and_update() == ExecutorState::Connected;
            let Some(state) = state.upgrade() else {
                return;
            };
            if now != connected {
                connected = now;
                if now {
                    let recorded = sqlx::query(
                        "UPDATE environments SET connected_once = 1, failed = 0 WHERE id = ?",
                    )
                    .bind(&environment_id)
                    .execute(&state.store.0)
                    .await;
                    if let Err(error) = recorded {
                        tracing::warn!(error = %error, "environment connection not recorded");
                    }
                }
                let status = if now { "connected" } else { "disconnected" };
                emit(
                    &state,
                    &session_id,
                    &environment_id,
                    status,
                    /*error*/ None,
                );
            }
        }
    });
}

/// Watch every stored environment again, and fail input that waited for an
/// executor when the service stopped: it was never dispatched.
pub(crate) async fn recover(state: &Arc<State>) -> anyhow::Result<()> {
    let rows: Vec<(String, String, bool)> =
        sqlx::query_as("SELECT id, session_id, waiting FROM environments")
            .fetch_all(&state.store.0)
            .await?;
    for (environment_id, session_id, waiting) in rows {
        watch(state, session_id.clone(), environment_id);
        if waiting {
            settle(state, &session_id, Some(("failed", Some(RESTARTED)))).await?;
        }
    }
    Ok(())
}

/// Add a session's environment to the current worker connection, once per
/// connection.
pub(crate) async fn attach(
    state: &State,
    project_id: &str,
    environment_id: &str,
) -> Result<(), ApiError> {
    // Check before even the already-attached fast path; never delegate the
    // service's harness authority to a foreign or missing environment.
    crate::projects::require(state, project_id, environment_id, Resource::Environment).await?;
    let attached = state.attached_environments()?;
    if crate::lock(&attached).contains(environment_id) {
        return Ok(());
    }
    let url = state.registry.harness_url().ok_or_else(|| {
        ApiError(
            StatusCode::NOT_IMPLEMENTED,
            "self-hosted environments are disabled; the operator must configure an environment key"
                .into(),
        )
    })?;
    state
        .rpc(
            "environment/add",
            json!({"environmentId": environment_id, "noiseRegistry": {
                "url": url, "environmentId": environment_id, "authToken": state.registry.harness_token()}}),
        )
        .await?;
    crate::lock(&attached).insert(environment_id.to_owned());
    Ok(())
}

/// Start a turn with this input, or hold it until the session's executor
/// connects.
pub(crate) async fn submit(
    state: &Arc<State>,
    session_id: &str,
    items: Vec<Value>,
) -> Result<(), ApiError> {
    let environment_id: Option<String> =
        sqlx::query_scalar("SELECT id FROM environments WHERE session_id = ?")
            .bind(session_id)
            .fetch_optional(&state.store.0)
            .await
            .map_err(anyhow::Error::from)?;
    let Some(environment_id) = environment_id else {
        crate::routes::start_turn(state, session_id, items).await?;
        return Ok(());
    };
    let admission = state.input_gates.lock(session_id).await;
    let connected = *state.registry.watch(&environment_id).borrow() == ExecutorState::Connected;
    let generation = state.registry.next_connection();
    let ready = {
        let mut waits = crate::lock(&state.waits.inputs);
        if let Some(wait) = waits.get_mut(session_id) {
            // Earlier input is still waiting or starting; keep the order.
            let queued = state.waits.admit(Some(wait), items)?;
            wait.inputs.push_back(queued);
            return Ok(());
        }
        if connected {
            Some(items)
        } else {
            let inputs = VecDeque::from([state.waits.admit(/*waiting*/ None, items)?]);
            waits.insert(session_id.to_owned(), Wait { inputs, generation });
            None
        }
    };
    if let Some(items) = ready {
        drop(admission);
        crate::routes::start_turn(state, session_id, items).await?;
        return Ok(());
    }
    let mut tx = state
        .store
        .0
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE environments SET waiting = 1 WHERE id = ?")
        .bind(&environment_id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    let mut events = crate::turns::queue(&mut tx, session_id).await?;
    events.extend(
        crate::records::transition(&mut tx, session_id, "requires_action", /*error*/ None).await?,
    );
    tx.commit().await.map_err(anyhow::Error::from)?;
    drop(admission);
    for event in events {
        crate::records::emit(state, event);
    }
    tokio::spawn(wait(
        Arc::downgrade(state),
        session_id.to_owned(),
        environment_id,
        generation,
    ));
    Ok(())
}

/// Wait for the executor, then start the held input in order, or drop it when
/// the wait times out.
async fn wait(state: Weak<State>, session_id: String, environment_id: String, generation: u64) {
    let Some((mut executor, limit)) = state.upgrade().map(|state| {
        (
            state.registry.watch(&environment_id),
            *crate::lock(&state.waits.limit),
        )
    }) else {
        return;
    };
    let connected = tokio::time::timeout(
        limit,
        executor.wait_for(|executor| *executor == ExecutorState::Connected),
    )
    .await
    .is_ok_and(|result| result.is_ok());
    drop(executor);
    let Some(state) = state.upgrade() else {
        return;
    };
    let current = |waits: &HashMap<String, Wait>| {
        waits
            .get(&session_id)
            .is_some_and(|wait| wait.generation == generation)
    };
    let admission = state.input_gates.lock(&session_id).await;
    {
        let mut waits = crate::lock(&state.waits.inputs);
        // A cancel already dropped this input.
        if !current(&waits) {
            return;
        }
        if !connected {
            waits.remove(&session_id);
        }
    }
    let dropped = if connected {
        None
    } else {
        Some(("failed", Some(TIMED_OUT)))
    };
    if let Err(error) = settle(&state, &session_id, dropped).await {
        tracing::warn!(
            error = format!("{error:#}"),
            "environment wait failed to settle"
        );
    }
    drop(admission);
    if !connected {
        let recorded = sqlx::query("UPDATE environments SET failed = 1 WHERE id = ?")
            .bind(&environment_id)
            .execute(&state.store.0)
            .await;
        if let Err(error) = recorded {
            tracing::warn!(error = %error, "environment failure not recorded");
        }
        let error = json!({"type": "environment_error", "code": "environment_connection_timeout", "message": TIMED_OUT});
        emit(&state, &session_id, &environment_id, "failed", Some(error));
        return;
    }
    // Input arriving meanwhile joins the queue, so it starts after this.
    loop {
        let items = {
            let mut waits = crate::lock(&state.waits.inputs);
            if !current(&waits) {
                return;
            }
            match waits
                .get_mut(&session_id)
                .and_then(|wait| wait.inputs.pop_front())
            {
                // Its bytes return to the budget as it leaves the queue.
                Some(mut queued) => std::mem::take(&mut queued.items),
                None => {
                    waits.remove(&session_id);
                    return;
                }
            }
        };
        if let Err(error) = crate::routes::start_turn(&state, &session_id, items).await {
            crate::lock(&state.waits.inputs).remove(&session_id);
            let failure = json!({"code": "internal_error", "message": error.1});
            if let Err(error) = end_queued(&state, &session_id, Some(failure)).await {
                tracing::warn!(error = format!("{error:#}"), "queued turn not ended");
            }
            let _ =
                crate::records::session_status(&state, &session_id, "failed", Some(&error.1)).await;
            return;
        }
    }
}

/// End a session's wait for its executor. Input that was dropped moves the
/// session to `dropped` (status and error); input about to start leaves the
/// status to the turn.
async fn settle(
    state: &State,
    session_id: &str,
    dropped: Option<(&str, Option<&str>)>,
) -> anyhow::Result<()> {
    let mut tx = state.store.0.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE environments SET waiting = 0 WHERE session_id = ?")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    let mut events = Vec::new();
    if let Some((status, error)) = dropped {
        // The turn the dropped input queued never starts: cancelled with the
        // input, or failed in the environment (a timeout) or the service (a
        // restart).
        let failure = error.map(|message| {
            let code = if message == RESTARTED {
                "internal_error"
            } else {
                "sandbox_error"
            };
            json!({"code": code, "message": message})
        });
        events.extend(crate::turns::end_queued(&mut tx, session_id, failure).await?);
        events.extend(crate::records::transition(&mut tx, session_id, status, error).await?);
    }
    tx.commit().await?;
    for event in events {
        crate::records::emit(state, event);
    }
    Ok(())
}

/// End the session's queued turn on its own transaction.
async fn end_queued(state: &State, session_id: &str, error: Option<Value>) -> anyhow::Result<()> {
    let mut tx = state.store.0.begin_with("BEGIN IMMEDIATE").await?;
    let events = crate::turns::end_queued(&mut tx, session_id, error).await?;
    tx.commit().await?;
    for event in events {
        crate::records::emit(state, event);
    }
    Ok(())
}

/// Drop input still waiting for the executor, returning the session to idle.
/// Returns whether any input was waiting.
pub(crate) async fn cancel(state: &State, session_id: &str) -> Result<bool, ApiError> {
    let _admission = state.input_gates.lock(session_id).await;
    let waiting = crate::lock(&state.waits.inputs)
        .remove(session_id)
        .is_some();
    if waiting {
        settle(state, session_id, Some(("idle", /*error*/ None))).await?;
    }
    Ok(waiting)
}

/// Forget a deleted session's environment: its executor sockets close and the
/// worker stops using it. The caller's compute keeps running.
pub(crate) async fn forget(state: &State, environment_id: &str) {
    state.registry.remove(environment_id);
    let _ = state.executors.remove_environment(environment_id);
    let attached = match state.attached_environments() {
        Ok(attached) => attached,
        Err(_) => return,
    };
    if crate::lock(&attached).remove(environment_id)
        && let Err(error) = state
            .rpc(
                "environment/remove",
                json!({"environmentId": environment_id}),
            )
            .await
    {
        tracing::warn!(
            environment_id,
            error = error.1,
            "worker kept a deleted environment"
        );
    }
}

#[cfg(test)]
#[path = "environments_tests.rs"]
mod tests;
