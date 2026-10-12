//! The Skills API (`/v1/skills`): skills uploaded as a ZIP or a directory, each
//! with immutable numbered versions. A skill's `default_version` is the one
//! used when a reference names none; `latest_version` is the newest that
//! remains. The default version cannot be deleted while others remain, and
//! deleting the last version deletes the skill. Contents are canonical ZIPs
//! under `DATA_DIRECTORY/skills`, checked against their digest before they
//! are served.
use crate::ApiError;
use crate::State;
use crate::contract::invalid;
use crate::projects::Principal;
use crate::skill_bundle::Bundle;
use axum::Extension;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State as Extract;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

/// Where skill version contents are stored.
pub(crate) struct Skills(pub(crate) PathBuf);

impl Skills {
    pub(crate) fn contents(&self, version_id: &str) -> PathBuf {
        self.0.join(format!("{version_id}.zip"))
    }
}

pub(crate) fn router() -> Router<Arc<State>> {
    Router::new()
        .route("/v1/skills", post(create).get(list))
        .route("/v1/skills/{id}", get(retrieve).post(update).delete(delete))
        .route("/v1/skills/{id}/content", get(content))
        .route(
            "/v1/skills/{id}/versions",
            post(create_version).get(list_versions),
        )
        .route(
            "/v1/skills/{id}/versions/{version}",
            get(retrieve_version).delete(delete_version),
        )
        .route(
            "/v1/skills/{id}/versions/{version}/content",
            get(version_content),
        )
}

/// A `skill_versions` row: ID, skill ID, number, name, description, and
/// creation time.
type VersionRow = (String, String, i64, String, String, i64);
const VERSION_COLUMNS: &str = "id, skill_id, version, name, description, created_at";

fn public_version((id, skill_id, version, name, description, created_at): VersionRow) -> Value {
    json!({"id": id, "object": "skill.version", "skill_id": skill_id, "version": version.to_string(),
        "name": name, "description": description, "created_at": created_at})
}

fn skill_not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "skill not found".into())
}

fn version_not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "skill version not found".into())
}

/// A skill in this project, as its public object.
async fn skill(state: &State, project_id: &str, id: &str) -> Result<Value, ApiError> {
    let row: Option<(i64, i64, String, String, i64)> = sqlx::query_as(
        "SELECT s.created_at, s.default_version, d.name, d.description, \
             (SELECT MAX(version) FROM skill_versions WHERE skill_id = s.id) \
         FROM skills s JOIN skill_versions d ON d.skill_id = s.id AND d.version = s.default_version \
         WHERE s.id = ? AND s.project_id = ?",
    )
    .bind(id)
    .bind(project_id)
    .fetch_optional(&state.store.0)
    .await
    .map_err(anyhow::Error::from)?;
    let (created_at, default_version, name, description, latest_version) =
        row.ok_or_else(skill_not_found)?;
    Ok(
        json!({"id": id, "object": "skill", "name": name, "description": description,
        "default_version": default_version.to_string(), "latest_version": latest_version.to_string(),
        "created_at": created_at}),
    )
}

/// A positive version number from a path or update.
fn number(version: &str) -> Option<i64> {
    version
        .parse::<i64>()
        .ok()
        .filter(|number| *number > 0 && !version.starts_with('+'))
}

/// One version of a skill in this project.
async fn version(
    state: &State,
    project_id: &str,
    skill_id: &str,
    version: &str,
) -> Result<VersionRow, ApiError> {
    skill(state, project_id, skill_id).await?;
    let number = number(version).ok_or_else(version_not_found)?;
    let row: Option<VersionRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {VERSION_COLUMNS} FROM skill_versions WHERE skill_id = ? AND version = ?"
    )))
    .bind(skill_id)
    .bind(number)
    .fetch_optional(&state.store.0)
    .await
    .map_err(anyhow::Error::from)?;
    row.ok_or_else(version_not_found)
}

/// Receive a skill upload and store it as a new version's contents, returning
/// the version's ID, its bundle, and the upload's form fields.
async fn receive(
    state: &State,
    headers: &HeaderMap,
    body: Body,
    allowed_fields: &[&str],
) -> Result<(String, Bundle, std::collections::BTreeMap<String, String>), ApiError> {
    let boundary = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(crate::upload::boundary)
        .ok_or_else(|| invalid("files is required; upload the skill as multipart/form-data"))?;
    tokio::fs::create_dir_all(&state.skills.0)
        .await
        .map_err(anyhow::Error::from)?;
    let id = format!("skillver_{}", Uuid::new_v4().simple());
    // Parts and the archive are written under temporary names, so an
    // interrupted upload never leaves contents under a version's ID; the
    // startup sweep removes them.
    let parts = state.skills.0.join(format!("{id}.upload"));
    let partial = state.skills.0.join(format!("{id}.partial"));
    let received = async {
        tokio::fs::create_dir(&parts)
            .await
            .map_err(anyhow::Error::from)?;
        let upload = crate::upload::receive_many(
            body,
            &boundary,
            &["files[]", "files"],
            &parts,
            crate::skill_bundle::MAX_FILES,
            crate::skill_bundle::MAX_UPLOAD_BYTES,
        )
        .await?;
        if let Some(unknown) = upload
            .fields
            .keys()
            .find(|field| !allowed_fields.contains(&field.as_str()))
        {
            return Err(invalid(format!("unknown field in skill upload: {unknown}")));
        }
        let output = partial.clone();
        let bundle = tokio::task::spawn_blocking(move || {
            crate::skill_bundle::package(&upload.files, &output)
        })
        .await
        .map_err(anyhow::Error::from)??;
        tokio::fs::rename(&partial, state.skills.contents(&id))
            .await
            .map_err(anyhow::Error::from)?;
        Ok((bundle, upload.fields))
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&parts).await;
    match received {
        Ok((bundle, fields)) => Ok((id, bundle, fields)),
        Err(error) => {
            let _ = tokio::fs::remove_file(&partial).await;
            Err(error)
        }
    }
}

async fn insert_version(
    tx: &mut sqlx::SqliteConnection,
    id: &str,
    skill_id: &str,
    version: i64,
    bundle: &Bundle,
    created_at: i64,
) -> Result<(), ApiError> {
    sqlx::query("INSERT INTO skill_versions (id, skill_id, version, name, description, bytes, sha256, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(id).bind(skill_id).bind(version).bind(&bundle.name).bind(&bundle.description)
        .bind(bundle.bytes as i64).bind(&bundle.sha256).bind(created_at)
        .execute(tx).await.map_err(anyhow::Error::from)?;
    Ok(())
}

async fn create(
    Extract(state): Extract<Arc<State>>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<Value>, ApiError> {
    let (version_id, bundle, _) = receive(&state, &headers, body, &[]).await?;
    let id = format!("skill_{}", Uuid::new_v4().simple());
    let now = crate::contract::now() as i64;
    let stored = async {
        let mut tx = state
            .store
            .0
            .begin()
            .await
            .map_err(anyhow::Error::from)?;
        sqlx::query("INSERT INTO skills (id, project_id, created_at, default_version, next_version) VALUES (?, ?, ?, 1, 2)")
            .bind(&id).bind(&principal.project_id).bind(now)
            .execute(&mut *tx).await.map_err(anyhow::Error::from)?;
        insert_version(&mut tx, &version_id, &id, 1, &bundle, now).await?;
        tx.commit().await.map_err(anyhow::Error::from)?;
        Ok::<_, ApiError>(())
    }
    .await;
    if let Err(error) = stored {
        let _ = tokio::fs::remove_file(state.skills.contents(&version_id)).await;
        return Err(error);
    }
    Ok(Json(skill(&state, &principal.project_id, &id).await?))
}

async fn retrieve(
    Extract(state): Extract<Arc<State>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(skill(&state, &principal.project_id, &id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateParams {
    default_version: Value,
}

async fn update(
    Extract(state): Extract<Arc<State>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    // The skill is looked up first, so a missing one reads as not found
    // whatever the body holds.
    skill(&state, &principal.project_id, &id).await?;
    let params: UpdateParams = serde_json::from_slice(&body)
        .map_err(|error| invalid(format!("invalid skill update: {error}")))?;
    let requested = match &params.default_version {
        Value::String(version) => number(version),
        Value::Number(version) => version.as_i64().filter(|number| *number > 0),
        _ => None,
    }
    .ok_or_else(|| invalid("default_version must be a positive version number"))?;
    let updated = sqlx::query(
        "UPDATE skills SET default_version = ? WHERE id = ? AND EXISTS \
         (SELECT 1 FROM skill_versions WHERE skill_id = ? AND version = ?)",
    )
    .bind(requested)
    .bind(&id)
    .bind(&id)
    .bind(requested)
    .execute(&state.store.0)
    .await
    .map_err(anyhow::Error::from)?
    .rows_affected();
    if updated == 0 {
        return Err(invalid(format!(
            "default_version {requested} is not a version of this skill"
        )));
    }
    Ok(Json(skill(&state, &principal.project_id, &id).await?))
}

async fn delete(
    Extract(state): Extract<Arc<State>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let mut tx = state.store.0.begin().await.map_err(anyhow::Error::from)?;
    let deleted = sqlx::query("DELETE FROM skills WHERE id = ? AND project_id = ?")
        .bind(&id)
        .bind(&principal.project_id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?
        .rows_affected();
    if deleted == 0 {
        return Err(skill_not_found());
    }
    let versions: Vec<String> =
        sqlx::query_scalar("DELETE FROM skill_versions WHERE skill_id = ? RETURNING id")
            .bind(&id)
            .fetch_all(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    // A failure here leaves contents for the startup sweep.
    for version in versions {
        let _ = tokio::fs::remove_file(state.skills.contents(&version)).await;
    }
    Ok(Json(
        json!({"id": id, "object": "skill.deleted", "deleted": true}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListParams {
    after: Option<String>,
    limit: Option<i64>,
    order: Option<String>,
}

impl ListParams {
    /// The page size and the SQL direction and comparison for `order`.
    fn page(&self) -> Result<(i64, &'static str, &'static str), ApiError> {
        let limit = self.limit.unwrap_or(20);
        if !(1..=100).contains(&limit) {
            return Err(invalid("limit must be between 1 and 100"));
        }
        match self.order.as_deref() {
            None | Some("desc") => Ok((limit, "DESC", "<")),
            Some("asc") => Ok((limit, "ASC", ">")),
            Some(_) => Err(invalid("order must be asc or desc")),
        }
    }
}

fn page(data: Vec<Value>, has_more: bool) -> Json<Value> {
    Json(
        json!({"object": "list", "first_id": data.first().map(|item| &item["id"]),
        "last_id": data.last().map(|item| &item["id"]), "has_more": has_more, "data": data}),
    )
}

async fn list(
    Extract(state): Extract<Arc<State>>,
    Extension(principal): Extension<Principal>,
    Query(params): Query<ListParams>,
) -> Result<Json<Value>, ApiError> {
    let (limit, direction, comparison) = params.page()?;
    let after: Option<i64> = match &params.after {
        Some(after) => Some(
            sqlx::query_scalar("SELECT seq FROM skills WHERE id = ? AND project_id = ?")
                .bind(after)
                .bind(&principal.project_id)
                .fetch_optional(&state.store.0)
                .await
                .map_err(anyhow::Error::from)?
                .ok_or_else(|| invalid("after must name an existing skill"))?,
        ),
        None => None,
    };
    // Fetch one extra row to learn whether more follow.
    let ids: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT id FROM skills WHERE project_id = ? AND (?2 IS NULL OR seq {comparison} ?2) ORDER BY seq {direction} LIMIT ?3"
    )))
    .bind(&principal.project_id)
    .bind(after)
    .bind(limit + 1)
    .fetch_all(&state.store.0)
    .await
    .map_err(anyhow::Error::from)?;
    let has_more = ids.len() as i64 > limit;
    let mut data = Vec::new();
    for id in ids.iter().take(limit as usize) {
        data.push(skill(&state, &principal.project_id, id).await?);
    }
    Ok(page(data, has_more))
}

/// Serve a version's contents after checking them against their digest.
async fn serve(state: &State, skill_id: &str, version: i64) -> Result<Response, ApiError> {
    let (id, name, sha256): (String, String, String) = sqlx::query_as(
        "SELECT id, name, sha256 FROM skill_versions WHERE skill_id = ? AND version = ?",
    )
    .bind(skill_id)
    .bind(version)
    .fetch_optional(&state.store.0)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(version_not_found)?;
    let path = state.skills.contents(&id);
    crate::files::verify(&path, &sha256).await?;
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| version_not_found())?;
    let length = file.metadata().await.map_err(anyhow::Error::from)?.len();
    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_owned()),
            (header::CONTENT_LENGTH, length.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}.zip\""),
            ),
        ],
        Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response())
}

async fn content(
    Extract(state): Extract<Arc<State>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let skill = skill(&state, &principal.project_id, &id).await?;
    let default = skill["default_version"]
        .as_str()
        .and_then(number)
        .ok_or_else(|| anyhow::anyhow!("stored default version is not a number"))?;
    serve(&state, &id, default).await
}

async fn create_version(
    Extract(state): Extract<Arc<State>>,
    Extension(principal): Extension<Principal>,
    Path(skill_id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<Value>, ApiError> {
    skill(&state, &principal.project_id, &skill_id).await?;
    let (id, bundle, fields) = receive(&state, &headers, body, &["default"]).await?;
    let stored = async {
        let make_default = match fields.get("default").map(String::as_str) {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err(invalid("default must be true or false")),
        };
        let now = crate::contract::now() as i64;
        let mut tx = state
            .store
            .0
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(anyhow::Error::from)?;
        // The skill may have been deleted while the upload arrived.
        let number: i64 = sqlx::query_scalar(
            "UPDATE skills SET next_version = next_version + 1 WHERE id = ? AND project_id = ? RETURNING next_version - 1",
        )
        .bind(&skill_id)
        .bind(&principal.project_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or_else(skill_not_found)?;
        insert_version(&mut tx, &id, &skill_id, number, &bundle, now).await?;
        if make_default {
            sqlx::query("UPDATE skills SET default_version = ? WHERE id = ?")
                .bind(number)
                .bind(&skill_id)
                .execute(&mut *tx)
                .await
                .map_err(anyhow::Error::from)?;
        }
        tx.commit().await.map_err(anyhow::Error::from)?;
        Ok(number)
    }
    .await;
    let number = match stored {
        Ok(number) => number,
        Err(error) => {
            let _ = tokio::fs::remove_file(state.skills.contents(&id)).await;
            return Err(error);
        }
    };
    let row = version(
        &state,
        &principal.project_id,
        &skill_id,
        &number.to_string(),
    )
    .await?;
    Ok(Json(public_version(row)))
}

async fn retrieve_version(
    Extract(state): Extract<Arc<State>>,
    Extension(principal): Extension<Principal>,
    Path((skill_id, number)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let row = version(&state, &principal.project_id, &skill_id, &number).await?;
    Ok(Json(public_version(row)))
}

async fn version_content(
    Extract(state): Extract<Arc<State>>,
    Extension(principal): Extension<Principal>,
    Path((skill_id, number)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let (_, _, number, ..) = version(&state, &principal.project_id, &skill_id, &number).await?;
    serve(&state, &skill_id, number).await
}

async fn delete_version(
    Extract(state): Extract<Arc<State>>,
    Extension(principal): Extension<Principal>,
    Path((skill_id, number)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let (id, _, number, ..) = version(&state, &principal.project_id, &skill_id, &number).await?;
    let mut tx = state
        .store
        .0
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(anyhow::Error::from)?;
    let (default, count): (i64, i64) = sqlx::query_as(
        "SELECT default_version, (SELECT COUNT(*) FROM skill_versions WHERE skill_id = s.id) FROM skills s WHERE id = ?",
    )
    .bind(&skill_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(skill_not_found)?;
    if number == default && count > 1 {
        return Err(invalid(
            "the default version cannot be deleted; set another default version first",
        ));
    }
    let deleted = sqlx::query("DELETE FROM skill_versions WHERE skill_id = ? AND version = ?")
        .bind(&skill_id)
        .bind(number)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?
        .rows_affected();
    if deleted == 0 {
        return Err(version_not_found());
    }
    // Deleting the last version deletes the skill.
    if count == 1 {
        sqlx::query("DELETE FROM skills WHERE id = ?")
            .bind(&skill_id)
            .execute(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
    }
    tx.commit().await.map_err(anyhow::Error::from)?;
    // A failure here leaves contents for the startup sweep.
    let _ = tokio::fs::remove_file(state.skills.contents(&id)).await;
    Ok(Json(
        json!({"id": id, "object": "skill.version.deleted", "deleted": true, "version": number.to_string()}),
    ))
}

async fn list_versions(
    Extract(state): Extract<Arc<State>>,
    Extension(principal): Extension<Principal>,
    Path(skill_id): Path<String>,
    Query(params): Query<ListParams>,
) -> Result<Json<Value>, ApiError> {
    let (limit, direction, comparison) = params.page()?;
    skill(&state, &principal.project_id, &skill_id).await?;
    let after: Option<i64> = match &params.after {
        Some(after) => Some(
            sqlx::query_scalar("SELECT version FROM skill_versions WHERE id = ? AND skill_id = ?")
                .bind(after)
                .bind(&skill_id)
                .fetch_optional(&state.store.0)
                .await
                .map_err(anyhow::Error::from)?
                .ok_or_else(|| invalid("after must name an existing version of this skill"))?,
        ),
        None => None,
    };
    let rows: Vec<VersionRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {VERSION_COLUMNS} FROM skill_versions WHERE skill_id = ?1 AND (?2 IS NULL OR version {comparison} ?2) ORDER BY version {direction} LIMIT ?3"
    )))
    .bind(&skill_id)
    .bind(after)
    .bind(limit + 1)
    .fetch_all(&state.store.0)
    .await
    .map_err(anyhow::Error::from)?;
    let has_more = rows.len() as i64 > limit;
    let data = rows
        .into_iter()
        .take(limit as usize)
        .map(public_version)
        .collect();
    Ok(page(data, has_more))
}

/// At startup, remove contents left by an interrupted upload or delete.
pub(crate) async fn sweep(state: &State) -> anyhow::Result<()> {
    let Ok(mut entries) = tokio::fs::read_dir(&state.skills.0).await else {
        return Ok(());
    };
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let known = match name.strip_suffix(".zip") {
            Some(id) => {
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM skill_versions WHERE id = ?)")
                    .bind(id)
                    .fetch_one(&state.store.0)
                    .await?
            }
            None => false,
        };
        if !known {
            if entry.file_type().await?.is_dir() {
                let _ = tokio::fs::remove_dir_all(entry.path()).await;
            } else {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
    }
    Ok(())
}
