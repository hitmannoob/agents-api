use crate::ApiError;
use crate::State;
use crate::resources::AgentConfig;
use crate::resources::Environment;
use axum::http::StatusCode;
use serde_json::Value;
use serde_json::json;
use std::collections::HashSet;

/// Most tools an agent may define. The contract documents no limit; the
/// request body bounds their size.
const MAX_TOOLS: usize = 128;

pub(crate) fn validate(config: &AgentConfig) -> Result<(), ApiError> {
    let invalid = || {
        ApiError(
            StatusCode::BAD_REQUEST,
            "invalid or oversized agent capabilities".into(),
        )
    };
    if config.tools.len() > MAX_TOOLS || config.mcp_servers.len() > 16 {
        return Err(invalid());
    }
    let mut names = HashSet::new();
    for tool in config
        .tools
        .iter()
        .filter_map(crate::agent_tools::Tool::function)
    {
        if !identifier(&tool.name)
            || tool.name == "mcp"
            || tool.name.starts_with("mcp__")
            || !names.insert(&tool.name)
            || tool.parameters.get("type") != Some(&json!("object"))
            || codex_tools::parse_tool_input_schema(&tool.parameters).is_err()
        {
            return Err(invalid());
        }
    }
    names.clear();
    for selection in &config.mcp_servers {
        if !identifier(&selection.server)
            || !names.insert(&selection.server)
            || selection.allowed_tools.len() > 32
            || selection.allowed_tools.iter().any(|name| !identifier(name))
            || selection.allowed_tools.iter().collect::<HashSet<_>>().len()
                != selection.allowed_tools.len()
        {
            return Err(invalid());
        }
    }
    crate::mcp::validate(config)?;
    if let Some(effort) = config.reasoning.as_ref().and_then(|r| r.effort.as_deref())
        && !matches!(
            effort,
            "none"
                | "minimal"
                | "low"
                | "medium"
                | "high"
                | "xhigh"
                | "max"
                | "ultra"
                | "persistent"
        )
    {
        return Err(invalid());
    }
    Ok(())
}

pub(crate) fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// The worker's catalog entry for `slug`, when it lists one.
async fn model(state: &State, slug: &str) -> Result<Option<Value>, ApiError> {
    let mut cursor = Value::Null;
    // Bound catalog traversal even if an external worker returns a broken cursor.
    for _ in 0..100 {
        let page = state
            .rpc(
                "model/list",
                json!({"cursor":cursor,"limit":100,"includeHidden":true}),
            )
            .await?;
        if let Some(model) = page["data"]
            .as_array()
            .and_then(|models| models.iter().find(|model| model["model"] == slug))
        {
            return Ok(Some(model.clone()));
        }
        let next = page["nextCursor"].clone();
        if next.is_null() || next == cursor {
            break;
        }
        cursor = next;
    }
    Ok(None)
}

/// The namespace deferred functions are offered in: Codex defers only
/// namespaced dynamic tools, and reserves `functions`.
pub(crate) const DEFERRED_NAMESPACE: &str = "deferred";

/// The `dynamicTools` for a thread. Deferred functions are offered through
/// tool search when the model supports it. Otherwise Codex would hide them
/// from the model altogether, so they are offered up front like the others:
/// still callable, only without the context saving.
pub(crate) async fn dynamic_tools(state: &State, config: &AgentConfig) -> Result<Value, ApiError> {
    let functions: Vec<_> = config
        .tools
        .iter()
        .filter_map(crate::agent_tools::Tool::function)
        .collect();
    let defer = functions.iter().any(|tool| tool.defer_loading)
        && model(state, &config.model)
            .await?
            .is_some_and(|model| model["supportsSearchTool"] == true);
    let spec = |tool: &crate::resources::FunctionTool| json!({"type": "function", "name": tool.name, "description": tool.description, "inputSchema": tool.parameters});
    let mut tools: Vec<Value> = functions
        .iter()
        .filter(|tool| !(defer && tool.defer_loading))
        .map(|tool| spec(tool))
        .collect();
    if defer {
        tools.push(json!({"type": "namespace", "name": DEFERRED_NAMESPACE,
            "description": "Functions the application provides, found with tool_search.",
            "tools": functions.iter().filter(|tool| tool.defer_loading).map(|tool| {
                let mut function = spec(tool);
                function["deferLoading"] = json!(true);
                function
            }).collect::<Vec<_>>()}));
    }
    Ok(json!(tools))
}

pub(crate) async fn overrides(
    state: &State,
    config: &AgentConfig,
    environment: &Environment,
    mcp_tokens: &std::collections::BTreeMap<String, String>,
    stdio_env: &crate::stdio_env::Values,
) -> Result<Value, ApiError> {
    if matches!(config.service_tier.as_deref(), Some("priority" | "fast")) {
        let supported = model(state, &config.model).await?.is_some_and(|model| {
            model["serviceTiers"]
                .as_array()
                .is_some_and(|tiers| tiers.iter().any(|tier| tier["id"] == "priority"))
        });
        if !supported {
            return Err(crate::contract::invalid(
                "selected model does not advertise priority service tier support",
            ));
        }
    }
    // A model's catalog entry can pin how it calls tools, and Codex follows it
    // over any configuration. Without the tool the model decides; an explicit
    // setting the model cannot honor is refused rather than ignored.
    if let Some(enabled) = crate::agent_tools::programmatic_tool_calling(config) {
        let mode = model(state, &config.model)
            .await?
            .and_then(|model| model["toolMode"].as_str().map(str::to_owned));
        match (enabled, mode.as_deref()) {
            (false, Some("codeMode" | "codeModeOnly")) => {
                return Err(crate::contract::invalid(
                    "selected model calls tools from code; programmatic_tool_calling cannot be disabled",
                ));
            }
            (true, Some("direct")) => {
                return Err(crate::contract::invalid(
                    "selected model does not support programmatic tool calling",
                ));
            }
            _ => {}
        }
    }
    let cwd = match environment {
        // A self-hosted workspace is not on this host.
        Environment::None | Environment::SelfHosted { .. } => None,
        Environment::Local { cwd } => Some(cwd),
    };
    let effective = state.rpc("config/read", json!({"cwd": cwd})).await?;
    let mut servers = effective
        .pointer("/config/mcp_servers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    crate::mcp::check(state, config, &servers).await?;
    for selection in &config.mcp_servers {
        let Some(server) = servers.get(&selection.server) else {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("unknown MCP server: {}", selection.server),
            ));
        };
        if server.get("enabled") == Some(&json!(false))
            || selection.allowed_tools.iter().any(|tool| {
                server
                    .get("enabled_tools")
                    .and_then(Value::as_array)
                    .is_some_and(|allowed| !allowed.contains(&json!(tool)))
                    || server
                        .get("disabled_tools")
                        .and_then(Value::as_array)
                        .is_some_and(|denied| denied.contains(&json!(tool)))
            })
        {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "MCP selection exceeds server-side policy".into(),
            ));
        }
    }
    for (name, server) in &mut servers {
        let selection = config
            .mcp_servers
            .iter()
            .find(|selection| selection.server == *name);
        // Override only policy fields. config/read includes nullable transport
        // defaults which are not valid TOML overrides, and may contain secrets.
        *server = json!({"enabled": selection.is_some()});
        if let Some(selection) = selection {
            server["enabled_tools"] = json!(selection.allowed_tools);
        }
    }
    let environment_id = match environment {
        Environment::SelfHosted { id, .. } => Some(id.as_str()),
        Environment::None | Environment::Local { .. } => None,
    };
    servers.extend(crate::mcp::overrides(
        config,
        mcp_tokens,
        stdio_env,
        environment_id,
    ));
    // Codex's V2 multi-agent runtime counts the root thread toward its cap, so
    // it gets one more thread than the public subagent limit.
    let multi_agent = match &config.multi_agent {
        Some(settings) if settings.enabled => json!({"enabled": true,
            "max_concurrent_threads_per_session": settings.max_concurrent_subagents.unwrap_or(6) + 1}),
        _ => json!(false),
    };
    // Disable other sources of MCP capabilities; never mutate global config.
    let mut overrides = json!({
        "mcp_servers": servers, "features.plugins": false, "features.apps": false,
        "features.enable_mcp_apps": false, "web_search": "disabled",
        "agents.enabled": false, "features.multi_agent_v2": multi_agent,
        // Codex's goal tools are not an advertised capability, and its MCP
        // resource tools would read any resource of a server, beyond the
        // caller's allowed tools.
        "features.goals": false,
        "features.code_mode": false,
        "features.code_mode_only": false,
        "features.mcp_resources": false,
        "tools.experimental_request_user_input.enabled": false,
        "model_reasoning_summary": config.reasoning.as_ref().and_then(|r| r.summary.as_deref()).unwrap_or("none"),
        "model_verbosity": config.text.as_ref().and_then(|t| t.verbosity.as_ref()).map_or(json!("medium"), |v| json!(v)),
        "features.fast_mode": true,
        "features.explicit_default_service_tier": true,
    });
    if let Some(effort) = config.reasoning.as_ref().and_then(|r| r.effort.as_ref()) {
        overrides["model_reasoning_effort"] = json!(effort);
    }
    // Programmatic tool calling is Codex's code mode: the model writes
    // JavaScript, run by the sibling `codex-code-mode-host`, that calls the
    // session's tools. It fails closed rather than silently falling back to
    // direct calls when the host is missing, and interrupts end running cells.
    if crate::agent_tools::programmatic_tool_calling(config) == Some(true) {
        overrides["features.code_mode"] = json!(true);
        overrides["features.code_mode_host"] =
            json!({"enabled": true, "disable_in_process_fallback": true});
        overrides["features.code_mode_interrupt"] = json!(true);
    }
    if let Some((mode, settings)) = crate::agent_tools::web_search(config) {
        overrides["web_search"] = json!(mode);
        overrides["tools.web_search"] = settings;
    }
    // Plugins in the caller's capability directories need Codex plugins on.
    // Plugins the worker's own configuration installs stay off, like its other
    // MCP sources, and so do remote plugins.
    if let Environment::SelfHosted {
        capability_directories,
        ..
    } = environment
        && !capability_directories.is_empty()
    {
        overrides["features.plugins"] = json!(true);
        overrides["features.remote_plugin"] = json!(false);
        overrides["plugins"] = effective
            .pointer("/config/plugins")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(id, _)| (id.clone(), json!({"enabled": false})))
            .collect::<serde_json::Map<_, _>>()
            .into();
    }
    Ok(overrides)
}
