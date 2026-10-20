//! Model catalog: probe a throwaway cursor-agent ACP child for
//! `cursor/list_available_models` (needs no prompt — extension method).

use acp_bridge::acp::{AcpClient, SpawnSpec};
use acp_bridge::session::Driver;
use serde_json::{json, Value};
use std::time::Duration;

use crate::driver::CursorDriver;

/// Returns raw `[{value,name,configOptions?}]` or None if the probe fails.
/// `env` stages the serving account's credential home (empty = ambient).
pub fn list_available(env: &[(String, String)]) -> Option<Vec<Value>> {
    let bin = crate::setup::resolve_command()?;
    let spec = SpawnSpec {
        command: bin,
        args: crate::setup::acp_argv(),
        cwd: std::env::temp_dir(),
        env: env.to_vec(),
    };
    let driver = CursorDriver { env: env.to_vec() };
    let mut client = AcpClient::spawn(&spec).ok()?;
    let _ = client
        .initialize(driver.client_info(), driver.client_capabilities())
        .ok()?;
    if let Some(method_id) = driver.auth_method_id() {
        let _ = client.authenticate(method_id);
    }
    let sess = client
        .session_new(&std::env::temp_dir().to_string_lossy(), json!([]))
        .ok()?;
    let res = client
        .ext_call(
            "cursor/list_available_models",
            json!({}),
            Duration::from_secs(15),
        )
        .or_else(|_| -> Result<Value, String> {
            // Some builds expose the list only via session/new.models.
            Ok(sess.get("models").cloned().unwrap_or_else(|| json!({})))
        })
        .ok()?;
    let models = res
        .get("models")
        .or_else(|| res.get("availableModels"))?
        .as_array()?
        .clone();
    Some(models)
}

/// catalog rows for provider/models: `{id, name}`.
pub fn catalog(env: &[(String, String)]) -> Vec<Value> {
    list_available(env)
        .unwrap_or_default()
        .iter()
        .filter_map(|m| {
            let id = m
                .get("value")
                .or_else(|| m.get("modelId"))
                .and_then(Value::as_str)?;
            Some(json!({
                "id": id,
                "name": m.get("name").and_then(Value::as_str).unwrap_or(id),
            }))
        })
        .collect()
}
