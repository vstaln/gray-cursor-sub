//! cursor-agent ACP driver: wire-level quirks for the shared bridge.

use acp_bridge::acp::{AcpClient, SpawnSpec};
use acp_bridge::session::{Driver, PermissionDecision};
use serde_json::{json, Value};

/// `env` stages the account's credential home (HOME/XDG/CURSOR_DATA_DIR
/// pins); empty means the ambient login.
pub struct CursorDriver {
    pub env: Vec<(String, String)>,
}

impl Driver for CursorDriver {
    fn spawn_spec(&self, stage_dir: &std::path::Path) -> SpawnSpec {
        SpawnSpec {
            command: crate::setup::resolve_command().unwrap_or_else(|| "cursor-agent".to_string()),
            args: crate::setup::acp_argv(),
            cwd: stage_dir.to_path_buf(),
            env: self.env.clone(),
        }
    }

    fn client_info(&self) -> (&'static str, &'static str) {
        ("gray-cursor-sub", env!("CARGO_PKG_VERSION"))
    }

    fn client_capabilities(&self) -> Value {
        json!({
            "fs": {"readTextFile": false, "writeTextFile": false},
            "terminal": false,
            "_meta": {"parameterizedModelPicker": true},
        })
    }

    fn auth_method_id(&self) -> Option<&'static str> {
        Some(crate::setup::AUTH_METHOD_ID)
    }

    /// Cursor's "ask" mode = read-only Q&A; "agent" = default. Model role
    /// wants the lightest mode; GRAY_CURSOR_SUB_MODE overrides.
    fn session_mode(&self) -> Option<&'static str> {
        let mode = std::env::var("GRAY_CURSOR_SUB_MODE").ok()?;
        // Leak once per process: sidecar mode is fixed for its lifetime.
        Some(match mode.trim() {
            "ask" => "ask",
            "plan" => "plan",
            "agent" => "agent",
            other => Box::leak(other.to_string().into_boxed_str()),
        })
    }

    /// The host's user-question surface doesn't exist for a pure model
    /// provider: auto-answer cursor's ask-question with an empty map,
    /// accept plans silently.
    fn ext_request(&self, method: &str, _params: &Value) -> Option<Value> {
        match method {
            "cursor/ask_question" => Some(json!({"answers": {}})),
            "cursor/create_plan" => Some(json!({"accepted": true})),
            _ => None,
        }
    }

    /// Only the host's own tools (mcp__gray__*) may run. Cursor's native
    /// tools are denied — the sidecar is a model, not an agent runtime.
    fn permission_option(&self, params: &Value) -> PermissionDecision {
        let title = params
            .get("toolCall")
            .and_then(|t| t.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let allow = title.contains("mcp__gray__")
            || title.contains("mcp_gray__")
            || title.contains("gray/")
            || title.starts_with("gray ");
        if !allow {
            return PermissionDecision::Deny;
        }
        let id = params
            .get("options")
            .and_then(Value::as_array)
            .and_then(|o| {
                o.iter()
                    .find(|opt| opt.get("kind").and_then(Value::as_str) == Some("allow_once"))
                    .or_else(|| {
                        o.iter().find(|opt| {
                            opt.get("kind").and_then(Value::as_str) == Some("allow_always")
                        })
                    })
            })
            .and_then(|opt| opt.get("optionId").and_then(Value::as_str))
            .unwrap_or("allow_once")
            .to_string();
        PermissionDecision::Allow(id)
    }

    fn apply_model(
        &self,
        client: &mut AcpClient,
        session_id: &str,
        session_new_result: &Value,
        model: &str,
        effort: Option<&str>,
        overrides: &[(String, String)],
    ) -> Result<(), String> {
        if let Err(e) = client.set_model(session_id, model, Value::Null) {
            if e.contains("-32601") || e.contains("not found") || e.contains("Invalid") {
                return Err(format!("unknown cursor model `{model}`"));
            }
            return Err(format!("cursor set_model: {e}"));
        }
        // config option ids by key: context, effort/reasoning, fast, thinking.
        for (key, value) in overrides {
            let Some(option_id) = find_config_option(session_new_result, key) else {
                continue;
            };
            let _ = client.set_config_option(session_id, &option_id, json!(value));
        }
        if let Some(e) = effort {
            if !overrides.iter().any(|(k, _)| k == "effort") {
                if let Some(option_id) = find_config_option(session_new_result, "effort") {
                    let _ = client.set_config_option(session_id, &option_id, json!(e));
                }
            }
        }
        Ok(())
    }
}

/// Match a bracket override key ("context"/"effort"/"fast"/"thinking")
/// against the session/new configOptions by id-or-name.
fn find_config_option(new_result: &Value, key: &str) -> Option<String> {
    let opts = new_result.get("configOptions")?.as_array()?;
    let want = key.to_lowercase();
    opts.iter()
        .find(|o| {
            let id = o.get("id").and_then(Value::as_str).unwrap_or("");
            let name = o.get("name").and_then(Value::as_str).unwrap_or("");
            let id_l = id.to_lowercase();
            let name_l = name.to_lowercase();
            match want.as_str() {
                "context" => id_l == "context" || name_l.contains("context"),
                "effort" => {
                    ["effort", "reasoning_effort", "reasoning", "thought_level"]
                        .contains(&id_l.as_str())
                        || name_l.contains("effort")
                        || name_l.contains("reasoning")
                }
                "fast" => id_l == "fast" || name_l == "fast" || name_l.contains("fast mode"),
                "thinking" => id_l == "thinking" || name_l.contains("thinking"),
                other => id_l == other,
            }
        })
        .and_then(|o| o.get("id").and_then(Value::as_str).map(str::to_string))
}
