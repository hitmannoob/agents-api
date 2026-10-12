//! Subagents: Codex child threads spawned by a session's agents. A subagent is
//! registered when its parent reports the spawn; its turns and items are
//! public records tagged with its ID, kept out of the session's root history,
//! and read through the subagent endpoints.
use crate::ApiError;
use crate::State;
use crate::contract::invalid;
use crate::records::Page;
use axum::Json;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State as Extract;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;

/// Who a notification belongs to: its session, the agent that ran it, and the
/// subagent when the thread is one.
pub(crate) struct Owner {
    pub session_id: String,
    pub agent_id: String,
    pub subagent_id: Option<String>,
}

pub(crate) async fn owner(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    thread_id: &str,
) -> anyhow::Result<Option<Owner>> {
    let root: Option<(String, String)> = sqlx::query_as(
        "SELECT p.id, json_extract(p.data, '$.agent.id') FROM sessions s JOIN public_sessions p ON p.id = s.id WHERE s.thread_id = ?",
    )
    .bind(thread_id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((session_id, agent_id)) = root {
        return Ok(Some(Owner {
            session_id,
            agent_id,
            subagent_id: None,
        }));
    }
    let child: Option<(String, String)> = sqlx::query_as(
        "SELECT a.session_id, a.id FROM subagents a JOIN public_sessions p ON p.id = a.session_id WHERE a.thread_id = ?",
    )
    .bind(thread_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(child.map(|(session_id, id)| Owner {
        session_id,
        agent_id: id.clone(),
        subagent_id: Some(id),
    }))
}

/// Register the subagent named by a parent's activity item. Returns its ID and,
/// the first time it is seen, its `subagent.created` event.
pub(crate) async fn register(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    owner: &Owner,
    activity: &Value,
) -> anyhow::Result<Option<(String, Option<Value>)>> {
    let Some(thread_id) = activity["agentThreadId"].as_str() else {
        return Ok(None);
    };
    let id = format!("subagent_{thread_id}");
    let path = activity["agentPath"].as_str();
    let known: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM subagents WHERE thread_id = ?)")
            .bind(thread_id)
            .fetch_one(&mut **tx)
            .await?;
    if known {
        sqlx::query(
            "UPDATE subagents SET agent_path = ? WHERE thread_id = ? AND agent_path IS NULL",
        )
        .bind(path)
        .bind(thread_id)
        .execute(&mut **tx)
        .await?;
        return Ok(Some((id, None)));
    }
    // Codex addresses agents by path, such as `/root/researcher`.
    let name = activity["agentPath"]
        .as_str()
        .and_then(|path| path.rsplit('/').next())
        .filter(|name| !name.is_empty());
    let subagent = json!({"id":id,"object":"agent.session.subagent","session_id":owner.session_id,
        "parent_agent_id":owner.agent_id,"name":name,"instructions":null,
        "opened_at":crate::contract::now(),"closed_at":null,"status":"active"});
    sqlx::query("INSERT INTO subagents (session_id, id, thread_id, data, agent_path) VALUES (?, ?, ?, ?, ?)")
        .bind(&owner.session_id)
        .bind(&id)
        .bind(thread_id)
        .bind(subagent.to_string())
        .bind(path)
        .execute(&mut **tx)
        .await?;
    Ok(Some((
        id,
        Some(json!({"type":"agent.session.subagent.created","subagent":subagent})),
    )))
}

/// The public call item for a subagent activity in a parent turn. Codex does
/// not report the task text with these activities, so the content is empty;
/// a creation carries the model and effort Codex resolved. A subagent's completion has no
/// item of its own: its turn events report it. A creation names the agent that
/// requested it, as the SDK documents; the new subagent's ID is on its
/// `subagent.created` event.
pub(crate) fn activity_item(owner: &Owner, subagent_id: &str, activity: &Value) -> Option<Value> {
    match activity["kind"].as_str()? {
        // Codex resolves the subagent's model and effort when it spawns it.
        "started" => Some(
            json!({"type":"create_subagent_call","agent_id":owner.agent_id,"content":[],
            "model":activity["model"],"reasoning_effort":activity["reasoningEffort"]}),
        ),
        "interacted" => Some(
            json!({"type":"send_subagent_input_call","sender_agent_id":owner.agent_id,
            "recipient_agent_id":subagent_id,"content":[]}),
        ),
        "interrupted" => Some(
            json!({"type":"interrupt_subagent_call","sender_agent_id":owner.agent_id,
            "recipient_agent_id":subagent_id}),
        ),
        _ => None,
    }
}

/// The public `agent_message` for a message Codex delivered between agent
/// threads: a task or message to a subagent, or a subagent's result. Codex
/// wraps plaintext in an envelope (`Message Type: …` through `Payload:`),
/// which is dropped; encrypted content passes through as Codex sent it.
pub(crate) async fn agent_message(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    owner: &Owner,
    item: &Value,
) -> anyhow::Result<Option<Value>> {
    let (Some(author), Some(recipient)) = (item["author"].as_str(), item["recipient"].as_str())
    else {
        return Ok(None);
    };
    let parts = item["content"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let text = parts
        .iter()
        .filter_map(|part| {
            part["text"]
                .as_str()
                .filter(|_| part["type"] == "input_text")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let payload = text
        .split_once("Payload:\n")
        .map_or(text.as_str(), |(_, payload)| payload);
    let mut content: Vec<Value> = Vec::new();
    if !payload.trim().is_empty() {
        content.push(json!({"type":"output_text","text":payload}));
    }
    content.extend(
        parts
            .iter()
            .filter(|part| part["type"] == "encrypted_content")
            .map(|part| json!({"type":"encrypted_content","encrypted_content":part["encrypted_content"]})),
    );
    if content.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        json!({"type":"agent_message","sender_agent_id":agent_id(tx, owner, author).await?,
        "recipient_agent_id":agent_id(tx, owner, recipient).await?,"content":content}),
    ))
}

/// The public ID of the agent Codex addresses by `path`: the session's root
/// agent, or a known subagent; otherwise the path itself, which the SDK
/// accepts as a name.
async fn agent_id(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    owner: &Owner,
    path: &str,
) -> anyhow::Result<String> {
    if path == "/root" {
        let root: Option<String> = sqlx::query_scalar(
            "SELECT json_extract(data, '$.agent.id') FROM public_sessions WHERE id = ?",
        )
        .bind(&owner.session_id)
        .fetch_optional(&mut **tx)
        .await?;
        return Ok(root.unwrap_or_else(|| path.to_owned()));
    }
    let id: Option<String> =
        sqlx::query_scalar("SELECT id FROM subagents WHERE session_id = ? AND agent_path = ?")
            .bind(&owner.session_id)
            .bind(path)
            .fetch_optional(&mut **tx)
            .await?;
    Ok(id.unwrap_or_else(|| path.to_owned()))
}

async fn subagent(state: &State, session_id: &str, id: &str) -> Result<Value, ApiError> {
    crate::records::session(state, session_id).await?;
    let data: Option<String> =
        sqlx::query_scalar("SELECT data FROM subagents WHERE session_id = ? AND id = ?")
            .bind(session_id)
            .bind(id)
            .fetch_optional(&state.store.0)
            .await
            .map_err(anyhow::Error::from)?;
    let data = data.ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "subagent not found".into()))?;
    Ok(serde_json::from_str(&data).map_err(anyhow::Error::from)?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListParams {
    after: Option<String>,
    limit: Option<u32>,
    order: Option<String>,
}

pub(crate) async fn list(
    Extract(state): Extract<Arc<State>>,
    Path(id): Path<String>,
    Query(params): Query<ListParams>,
) -> Result<Json<Value>, ApiError> {
    crate::records::session(&state, &id).await?;
    let limit = params.limit.unwrap_or(/*default*/ 20);
    let ascending = match params.order.as_deref().unwrap_or("desc") {
        "asc" => true,
        "desc" => false,
        _ => return Err(invalid("order must be asc or desc")),
    };
    if !(1..=100).contains(&limit) {
        return Err(invalid("limit must be between 1 and 100"));
    }
    let cursor: Option<i64> = match &params.after {
        Some(after) => Some(
            sqlx::query_scalar("SELECT created_seq FROM subagents WHERE session_id = ? AND id = ?")
                .bind(&id)
                .bind(after)
                .fetch_optional(&state.store.0)
                .await
                .map_err(anyhow::Error::from)?
                .ok_or_else(|| invalid("invalid subagent cursor"))?,
        ),
        None => None,
    };
    let query = if ascending {
        "SELECT data FROM subagents WHERE session_id = ? AND created_seq > ? ORDER BY created_seq ASC LIMIT ?"
    } else {
        "SELECT data FROM subagents WHERE session_id = ? AND created_seq < ? ORDER BY created_seq DESC LIMIT ?"
    };
    let rows: Vec<String> = sqlx::query_scalar(query)
        .bind(&id)
        .bind(cursor.unwrap_or(if ascending { 0 } else { i64::MAX }))
        .bind(i64::from(limit) + 1)
        .fetch_all(&state.store.0)
        .await
        .map_err(anyhow::Error::from)?;
    let has_more = rows.len() > limit as usize;
    let data = rows
        .iter()
        .take(limit as usize)
        .map(|row| serde_json::from_str::<Value>(row))
        .collect::<Result<Vec<_>, _>>()
        .map_err(anyhow::Error::from)?;
    Ok(Json(
        json!({"object":"list","first_id":data.first().map(|s| &s["id"]),
        "last_id":data.last().map(|s| &s["id"]),"data":data,"has_more":has_more}),
    ))
}

pub(crate) async fn retrieve(
    Extract(state): Extract<Arc<State>>,
    Path((id, subagent_id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(subagent(&state, &id, &subagent_id).await?))
}

pub(crate) async fn items(
    Extract(state): Extract<Arc<State>>,
    Path((id, subagent_id)): Path<(String, String)>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    subagent(&state, &id, &subagent_id).await?;
    crate::records::list(&state, &id, "item", page, Some(&subagent_id)).await
}

pub(crate) async fn turns(
    Extract(state): Extract<Arc<State>>,
    Path((id, subagent_id)): Path<(String, String)>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    subagent(&state, &id, &subagent_id).await?;
    crate::records::list(&state, &id, "turn", page, Some(&subagent_id)).await
}

pub(crate) async fn turn(
    Extract(state): Extract<Arc<State>>,
    Path((id, subagent_id, turn_id)): Path<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    subagent(&state, &id, &subagent_id).await?;
    crate::records::turn_record(&state, &id, Some(&subagent_id), &turn_id).await
}

pub(crate) async fn turn_items(
    Extract(state): Extract<Arc<State>>,
    Path((id, subagent_id, turn_id)): Path<(String, String, String)>,
    Query(mut page): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    // The turn must belong to this subagent before its items are listed.
    let _ = crate::records::turn_record(&state, &id, Some(&subagent_id), &turn_id).await?;
    page.turn_id = Some(turn_id);
    crate::records::list(&state, &id, "item", page, Some(&subagent_id)).await
}
