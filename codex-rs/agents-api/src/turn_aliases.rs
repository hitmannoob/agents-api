//! Public IDs for turns that were queued before Codex started them.
//!
//! A turn whose input waits for a self-hosted executor is reported as
//! `queued` under an ID the service picks, but Codex picks its own ID when it
//! starts the turn. The first turn Codex starts on the session's thread after
//! the wait takes the queued ID: every notification and tool call request is
//! translated as it arrives, so records, streams, webhooks, traces, and usage
//! only ever see public IDs, and requests that name a turn to Codex are
//! translated back. Every other turn keeps the ID Codex gave it.
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub(crate) struct TurnAliases {
    /// Codex turn ID to public turn ID.
    public: Mutex<HashMap<String, String>>,
    /// Public turn ID to Codex turn ID.
    codex: Mutex<HashMap<String, String>>,
    /// The queued turn the next turn started on a thread becomes, as (session
    /// ID, public turn ID), by thread ID.
    expected: Mutex<HashMap<String, (String, String)>>,
}

impl TurnAliases {
    pub(crate) async fn load(pool: &sqlx::SqlitePool) -> anyhow::Result<Self> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT codex_turn_id, public_turn_id FROM turn_aliases")
                .fetch_all(pool)
                .await?;
        let aliases = Self::default();
        for (codex, public) in rows {
            crate::lock(&aliases.codex).insert(public.clone(), codex.clone());
            crate::lock(&aliases.public).insert(codex, public);
        }
        Ok(aliases)
    }

    /// Give the next turn Codex starts on `thread_id` the queued turn's ID.
    pub(crate) fn expect(&self, thread_id: &str, session_id: &str, public_id: &str) {
        crate::lock(&self.expected).insert(
            thread_id.to_owned(),
            (session_id.to_owned(), public_id.to_owned()),
        );
    }

    /// Stop expecting a turn on `thread_id`, when starting it failed.
    pub(crate) fn abandon(&self, thread_id: &str) {
        crate::lock(&self.expected).remove(thread_id);
    }

    /// Record that Codex started `codex_id` on `thread_id`, binding it to the
    /// queued turn expected there. Both the `turn/start` response and the
    /// `turn/started` notification report the start; the first one binds.
    pub(crate) async fn started(
        &self,
        pool: &sqlx::SqlitePool,
        thread_id: &str,
        codex_id: &str,
    ) -> anyhow::Result<()> {
        if crate::lock(&self.public).contains_key(codex_id) {
            return Ok(());
        }
        let Some((session_id, public_id)) = crate::lock(&self.expected).remove(thread_id) else {
            return Ok(());
        };
        sqlx::query("INSERT OR IGNORE INTO turn_aliases (codex_turn_id, session_id, public_turn_id) VALUES (?, ?, ?)")
            .bind(codex_id)
            .bind(&session_id)
            .bind(&public_id)
            .execute(pool)
            .await?;
        crate::lock(&self.codex).insert(public_id.clone(), codex_id.to_owned());
        crate::lock(&self.public).insert(codex_id.to_owned(), public_id);
        Ok(())
    }

    /// The public ID of a turn Codex named.
    pub(crate) fn public(&self, codex_id: &str) -> String {
        crate::lock(&self.public)
            .get(codex_id)
            .cloned()
            .unwrap_or_else(|| codex_id.to_owned())
    }

    /// The ID Codex knows a public turn by.
    pub(crate) fn codex(&self, public_id: &str) -> String {
        crate::lock(&self.codex)
            .get(public_id)
            .cloned()
            .unwrap_or_else(|| public_id.to_owned())
    }

    /// Translate the turn IDs in a Codex notification to public ones,
    /// binding a newly started turn first.
    pub(crate) async fn translate(
        &self,
        pool: &sqlx::SqlitePool,
        notification: &mut Value,
    ) -> anyhow::Result<()> {
        if notification["method"] == "turn/started"
            && let (Some(thread_id), Some(codex_id)) = (
                notification["params"]["threadId"].as_str(),
                notification["params"]["turn"]["id"].as_str(),
            )
        {
            self.started(pool, thread_id, codex_id).await?;
        }
        let params = &mut notification["params"];
        for pointer in ["/turnId", "/turn/id"] {
            if let Some(slot) = params.pointer_mut(pointer)
                && let Some(codex_id) = slot.as_str()
            {
                *slot = json!(self.public(codex_id));
            }
        }
        Ok(())
    }
}
