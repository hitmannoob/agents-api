//! Literal MCP secrets (`transport.env` and HTTP `transport.authorization`). They
//! are secrets: taken out of an agent configuration before it is stored, kept
//! encrypted in the vault store, and never returned. A saved agent keeps its
//! own copy; a session copies its values when it is created, as it does with
//! vault credentials, so later agent changes do not reach running sessions.
use crate::ApiError;
use crate::State;
use crate::contract::invalid;
use serde_json::Value;
use std::collections::BTreeMap;

/// Values by server label, then variable name or reserved secret key. Keeping
/// this layout and the secret names preserves existing stdio env snapshots.
pub(crate) type Values = BTreeMap<String, BTreeMap<String, String>>;

// This cannot collide with a portable environment variable name.
const AUTHORIZATION: &str = "!authorization";

const MAX_VARIABLES: usize = 64;
const MAX_VALUE_BYTES: usize = 8192;

/// Remove literal secrets from each MCP transport in an agent patch and return the
/// values by server label, or `None` when the patch leaves the tools as they
/// are.
pub(crate) fn take(patch: &mut Value) -> Result<Option<Values>, ApiError> {
    let Some(tools) = patch.get_mut("tools") else {
        return Ok(None);
    };
    let mut values = Values::new();
    for tool in tools.as_array_mut().into_iter().flatten() {
        if tool["type"] != "mcp" {
            continue;
        }
        let label = tool["server_label"].as_str().unwrap_or_default().to_owned();
        let credential = tool.get("credential_id").is_some_and(|id| !id.is_null());
        let Some(transport) = tool.get_mut("transport").and_then(Value::as_object_mut) else {
            continue;
        };
        if transport.get("type") == Some(&Value::from("http")) {
            let authorization = match transport.remove("authorization") {
                None | Some(Value::Null) => continue,
                Some(Value::String(value)) => value,
                Some(_) => return Err(invalid("MCP authorization must be a string or null")),
            };
            crate::mcp::validate_authorization(&authorization)?;
            if credential {
                return Err(invalid(
                    "MCP credential_id cannot be combined with transport.authorization",
                ));
            }
            values.insert(
                label,
                BTreeMap::from([(AUTHORIZATION.to_owned(), authorization)]),
            );
            continue;
        }
        if transport.get("type") != Some(&Value::from("stdio")) {
            continue;
        }
        let env = match transport.remove("env") {
            None | Some(Value::Null) => continue,
            Some(env) => serde_json::from_value::<BTreeMap<String, String>>(env)
                .map_err(|_| invalid("stdio MCP env must map variable names to strings"))?,
        };
        if env.len() > MAX_VARIABLES
            || env.iter().any(|(name, value)| {
                !crate::mcp::variable_name(name) || value.len() > MAX_VALUE_BYTES
            })
        {
            return Err(invalid(format!(
                "stdio MCP env takes at most {MAX_VARIABLES} variables with values of at most {MAX_VALUE_BYTES} bytes"
            )));
        }
        if !env.is_empty() {
            values.insert(label, env);
        }
    }
    Ok(Some(values))
}

/// A server's inline authorization, if it has one.
pub(crate) fn authorization<'a>(values: &'a Values, label: &str) -> Option<&'a str> {
    values.get(label)?.get(AUTHORIZATION).map(String::as_str)
}

pub(crate) fn disabled(values: &Values) -> ApiError {
    let message = if values
        .values()
        .any(|values| values.contains_key(AUTHORIZATION))
    {
        "inline MCP authorization requires the operator to configure a vault passphrase"
    } else {
        "stdio MCP env values require the operator to configure a vault passphrase; use env_vars to inherit them from the executor"
    };
    ApiError(axum::http::StatusCode::NOT_IMPLEMENTED, message.into())
}

fn hex(value: &str) -> String {
    value.bytes().map(|byte| format!("{byte:02X}")).collect()
}

/// The secret holding a saved agent's values.
pub(crate) fn agent_name(agent_id: &str) -> String {
    format!("AGENT_ENV_{}", hex(agent_id))
}

/// The secret holding a session's values.
pub(crate) fn session_name(session_id: &str) -> String {
    format!("SESSION_ENV_{}", hex(session_id))
}

/// Replace the values stored under `name`; empty values remove them. Storing
/// values needs the operator's vault passphrase.
pub(crate) async fn save(state: &State, name: String, values: &Values) -> Result<(), ApiError> {
    if values.is_empty() {
        state.secrets.discard(vec![name]).await;
        return Ok(());
    }
    state
        .secrets
        .set(
            name,
            serde_json::to_value(values).map_err(anyhow::Error::from)?,
        )
        .await
}

/// The values stored under `name`, if any. Without a vault store there are none.
pub(crate) async fn load(state: &State, name: String) -> Result<Values, ApiError> {
    if !state.secrets.configured() {
        return Ok(Values::new());
    }
    Ok(match state.secrets.get(name).await? {
        Some(values) => serde_json::from_value(values).map_err(anyhow::Error::from)?,
        None => Values::new(),
    })
}
