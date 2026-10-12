//! Persisted tool configuration. Execution of non-function tools belongs to G06-G08.
use crate::resources::FunctionTool;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

// The untagged function representation preserves existing saved agents and prototype clients.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum Tool {
    Function(FunctionTool),
    Capability(CapabilityTool),
}

impl Tool {
    pub(crate) fn function(&self) -> Option<&FunctionTool> {
        match self {
            Self::Function(tool) => Some(tool),
            Self::Capability(_) => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CapabilityTool {
    ToolSearch,
    ProgrammaticToolCalling {
        #[serde(default = "enabled")]
        enabled: bool,
    },
    Mcp {
        server_label: String,
        transport: McpTransport,
        allowed_tools: Option<Vec<String>>,
        #[serde(default, deserialize_with = "default_if_null")]
        connection_origin: ConnectionOrigin,
        credential_id: Option<String>,
        #[serde(default)]
        request_metadata: BTreeMap<String, Value>,
        #[serde(default)]
        required: bool,
    },
    WebSearch {
        allowed_domains: Option<Vec<String>>,
        #[serde(default, deserialize_with = "default_if_null")]
        context_size: Detail,
        location: Option<Location>,
        #[serde(default, deserialize_with = "default_if_null")]
        mode: SearchMode,
    },
}

fn enabled() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum McpTransport {
    Http {
        server_url: String,
        #[serde(default, skip_serializing)]
        authorization: Option<String>,
        #[serde(default, deserialize_with = "default_if_null")]
        headers: BTreeMap<String, String>,
    },
    Stdio {
        command: String,
        cwd: String,
        #[serde(default, deserialize_with = "default_if_null")]
        args: Vec<String>,
        #[serde(default, deserialize_with = "default_if_null")]
        env_vars: Vec<String>,
    },
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConnectionOrigin {
    #[default]
    Service,
    Environment,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Detail {
    Low,
    #[default]
    Medium,
    High,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SearchMode {
    Disabled,
    Cached,
    #[default]
    Live,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Location {
    city: Option<String>,
    country: Option<String>,
    region: Option<String>,
    timezone: Option<String>,
}

/// The agent's programmatic tool calling setting, when it has the tool.
pub(crate) fn programmatic_tool_calling(config: &crate::resources::AgentConfig) -> Option<bool> {
    config.tools.iter().find_map(|tool| match tool {
        Tool::Capability(CapabilityTool::ProgrammaticToolCalling { enabled }) => Some(*enabled),
        _ => None,
    })
}

/// The Codex `web_search` mode and `tools.web_search` settings for an agent's
/// enabled web search tool. TOML has no null, so unset settings are omitted.
pub(crate) fn web_search(config: &crate::resources::AgentConfig) -> Option<(SearchMode, Value)> {
    config.tools.iter().find_map(|tool| match tool {
        Tool::Capability(CapabilityTool::WebSearch {
            allowed_domains,
            context_size,
            location,
            mode,
        }) if *mode != SearchMode::Disabled => {
            let mut settings = serde_json::json!({"context_size": context_size});
            if let Some(domains) = allowed_domains {
                settings["allowed_domains"] = serde_json::json!(domains);
            }
            if let Some(location) = location {
                let set: serde_json::Map<String, Value> = serde_json::to_value(location)
                    .ok()?
                    .as_object()?
                    .iter()
                    .filter(|(_, value)| !value.is_null())
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                settings["location"] = Value::Object(set);
            }
            Some((mode.clone(), settings))
        }
        _ => None,
    })
}

fn default_if_null<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}
