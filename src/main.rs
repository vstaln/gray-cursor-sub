//! cursor-sub — Gray sidecar that relays chat turns to the Cursor CLI's
//! `cursor-agent acp` subprocess (t3code CursorDriver parity, model role).

use acp_bridge::relay::{self, Intents, TurnFn};
use acp_bridge::session::{self, Pool};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use cursor_sub::{driver::CursorDriver, models, usage, setup};

const PROVIDER_ID: &str = "cursor";
const AUTH_CLI: &str = "cursor-login";
const AUTH_KEY: &str = "cursor-api-key";
const TOKEN_PREFIX: &str = "cursor-sub-";

fn manifest() -> Value {
    json!({
        "name": "cursor-sub",
        "display_name": "Cursor",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.2",
        "capabilities": ["provider.credentials", "host.say"],
        "providers": [{
            "id": PROVIDER_ID,
            "name": "Cursor CLI (subscription)",
            "auth_methods": [
                {"id": AUTH_CLI, "kind": "oauth",
                 "name": "Cursor CLI login",
                 "operations": ["login", "refresh", "revoke", "models", "chat"]},
                {"id": AUTH_KEY, "kind": "api_key",
                 "name": "Cursor API key (CURSOR_API_KEY)",
                 "operations": ["models", "chat", "refresh"]},
            ],
            "transport": {
                "kind": "openai-responses",
                "base_url": "https://127.0.0.1:1/",
                "authorization": {"kind": "bearer", "secret_name": "relay_token"},
            },
        }],
        "commands": ["/cursor"],
    })
}

// ---------- errors ----------

#[derive(Debug)]
enum RpcError {
    Protocol(String),
    Unavailable(String),
    Rpc { code: &'static str, message: String },
}

fn unavailable(msg: impl Into<String>) -> RpcError {
    RpcError::Unavailable(msg.into())
}

fn error_value(e: &RpcError) -> Value {
    match e {
        RpcError::Rpc { code, message } => json!({
            "code": code, "message": message, "retryable": false, "terminal": true,
        }),
        RpcError::Unavailable(message) => json!({
            "code": "unavailable", "message": message, "retryable": true, "terminal": false,
        }),
        RpcError::Protocol(message) => json!({
            "code": "protocol", "message": message, "retryable": false, "terminal": true,
        }),
    }
}

// ---------- login ops ----------

struct LoginOp {
    child: Option<Child>,
    url: Arc<Mutex<Option<String>>>,
}
type Ops = Arc<Mutex<HashMap<String, LoginOp>>>;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn auth_start(ops: &Ops, params: &Value) -> Result<Value, RpcError> {
    let method = params.get("auth_method").and_then(Value::as_str).unwrap_or_default();
    if method == AUTH_KEY {
        if setup::api_key().is_some() {
            return Ok(json!({
                "operation_id": "",
                "status": "completed",
                "verification_uri": "",
                "expires_at": now_secs() + 600,
                "retry_after_ms": 0,
            }));
        }
        return Err(unavailable("set CURSOR_API_KEY first, or /connect with a pasted key"));
    }
    let bin = setup::resolve_command().ok_or_else(|| unavailable(setup::INSTALL_HINT))?;
    if setup::logged_in().is_some() {
        return Ok(json!({
            "operation_id": "",
            "status": "completed",
            "verification_uri": "",
            "expires_at": now_secs() + 600,
            "retry_after_ms": 0,
        }));
    }
    // cursor-agent login prints a verification URL; capture it for the host
    // browser flow. NO_OPEN_BROWSER keeps the CLI from racing the host.
    let mut child = Command::new(&bin)
        .arg("login")
        .env("NO_OPEN_BROWSER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| unavailable(format!("cursor-agent login: {e}")))?;
    let url: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    if let Some(stdout) = child.stdout.take() {
        let url2 = url.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(start) = line.find("http") {
                    let end = line[start..]
                        .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                        .map(|e| start + e)
                        .unwrap_or(line.len());
                    if let Ok(mut u) = url2.lock() {
                        if u.is_none() {
                            *u = Some(line[start..end].to_string());
                        }
                    }
                }
            }
        });
    }
    let op_id = uuid::Uuid::new_v4().to_string();
    ops.lock().unwrap().insert(
        op_id.clone(),
        LoginOp {
            child: Some(child),
            url,
        },
    );
    Ok(json!({
        "operation_id": op_id,
        "status": "pending",
        "verification_uri": "",
        "expires_at": now_secs() + 900,
        "retry_after_ms": 2000,
    }))
}

fn auth_poll(ops: &Ops, params: &Value) -> Result<Value, RpcError> {
    let op_id = params.get("operation_id").and_then(Value::as_str).unwrap_or_default();
    // An immediate-complete op (empty id) just re-probes the CLI login.
    if op_id.is_empty() {
        return Ok(match setup::logged_in() {
            Some(info) => json!({"state": "completed", "credential": login_material(&info)}),
            None => json!({"state": "failed", "error": {
                "code": "unavailable", "message": setup::LOGIN_HINT,
                "retryable": false, "terminal": true,
            }}),
        });
    }
    let (child_done, url) = {
        let mut g = ops.lock().unwrap();
        let Some(op) = g.get_mut(op_id) else {
            return Ok(json!({"state": "operation_lost"}));
        };
        let done = match op.child.as_mut().map(|c| c.try_wait()) {
            Some(Ok(Some(status))) => Some(status.success()),
            Some(Err(_)) => Some(false),
            _ => None,
        };
        let url = op.url.lock().ok().and_then(|u| u.clone());
        (done, url)
    };
    if let Some(info) = setup::logged_in() {
        ops.lock().unwrap().remove(op_id);
        return Ok(json!({"state": "completed", "credential": login_material(&info)}));
    }
    match child_done {
        Some(false) => {
            ops.lock().unwrap().remove(op_id);
            Ok(json!({"state": "failed", "error": {
                "code": "login_failed", "message": "cursor-agent login exited without a session",
                "retryable": false, "terminal": true,
            }}))
        }
        _ => Ok(json!({
            "state": "pending",
            "verification_uri": url.unwrap_or_default(),
            "retry_after_ms": 2000,
        })),
    }
}

fn auth_cancel(ops: &Ops, params: &Value) -> Result<Value, RpcError> {
    let op_id = params.get("operation_id").and_then(Value::as_str).unwrap_or_default();
    if let Some(mut op) = ops.lock().unwrap().remove(op_id) {
        if let Some(mut c) = op.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
    Ok(json!({}))
}

fn auth_refresh(params: &Value) -> Result<Value, RpcError> {
    let method = params.get("auth_method").and_then(Value::as_str).unwrap_or_default();
    if method == AUTH_KEY {
        let key = setup::api_key()
            .ok_or_else(|| unavailable("CURSOR_API_KEY not set and no cached key"))?;
        let _ = setup::write_api_key_cache(&key);
        return Ok(json!({"credential": {
            "secrets": {"api_key": key},
            "metadata": {},
            "expires_at": null,
        }}));
    }
    match setup::logged_in() {
        Some(info) => Ok(json!({"credential": login_material(&info)})),
        None => Err(RpcError::Rpc {
            code: "invalid_grant",
            message: setup::LOGIN_HINT.into(),
        }),
    }
}

fn auth_revoke(params: &Value) -> Result<Value, RpcError> {
    let method = params.get("auth_method").and_then(Value::as_str).unwrap_or_default();
    if method == AUTH_CLI {
        if let Some(bin) = setup::resolve_command() {
            let _ = Command::new(bin)
                .arg("logout")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    } else if let Ok(home) = std::env::var("HOME") {
        let _ = std::fs::remove_file(format!("{home}/.cursor-sub.json"));
    }
    Ok(json!({}))
}

fn login_material(info: &Value) -> Value {
    json!({
        "secrets": {"cursor_login": "1"},
        "metadata": {
            "email": info.get("email").and_then(Value::as_str).unwrap_or(""),
            "subscription_tier": info.get("subscriptionTier").and_then(Value::as_str).unwrap_or(""),
            "cli_version": info.get("cliVersion").and_then(Value::as_str).unwrap_or(""),
        },
        "expires_at": null,
    })
}

// ---------- models + chat ----------

fn gate_auth(method: &str) -> Result<(), RpcError> {
    if setup::resolve_command().is_none() {
        return Err(unavailable(setup::INSTALL_HINT));
    }
    match method {
        AUTH_KEY => {
            if setup::api_key().is_none() {
                return Err(unavailable("set CURSOR_API_KEY for this auth method"));
            }
        }
        _ => {
            if setup::logged_in().is_none() {
                return Err(unavailable(setup::LOGIN_HINT));
            }
        }
    }
    Ok(())
}

fn provider_models(params: &Value) -> Result<Value, RpcError> {
    let method = params.get("auth_method").and_then(Value::as_str).unwrap_or_default();
    gate_auth(method)?;
    // Stash a passed api_key so chat turns (credential-free) can reach it.
    if let Some(key) = params
        .get("credential")
        .and_then(|c| c.get("material"))
        .and_then(|m| m.get("secrets"))
        .and_then(|s| s.get("api_key"))
        .and_then(Value::as_str)
    {
        let _ = setup::write_api_key_cache(key);
    }
    let rows = models::catalog();
    if rows.is_empty() {
        return Err(unavailable("cursor model discovery failed (CLI probe)"));
    }
    Ok(json!({
        "provider": PROVIDER_ID,
        "models": rows,
        "default_model": rows.first().and_then(|m| m.get("id")).cloned().unwrap_or(Value::Null),
    }))
}

fn provider_chat(
    intents: &Intents,
    pool: &Arc<Pool>,
    params: &Value,
) -> Result<Value, RpcError> {
    let provider = params.get("provider").and_then(Value::as_str).unwrap_or_default();
    if provider != PROVIDER_ID {
        return Err(RpcError::Protocol("unknown provider".into()));
    }
    let method = params.get("auth_method").and_then(Value::as_str).unwrap_or_default();
    gate_auth(method)?;
    let model = params.get("model").and_then(Value::as_str).unwrap_or_default();
    let pool2 = pool.clone();
    let turn: TurnFn = Arc::new(move |raw: &[u8], cancel: &AtomicBool| {
        session::run_turn(&pool2, raw, cancel)
    });
    let (relay_url, relay_token) = relay::mint_turn(intents, model, turn, TOKEN_PREFIX)
        .map_err(unavailable)?;
    Ok(json!({
        "relay_url": relay_url,
        "relay_token": relay_token,
    }))
}

fn command_run(params: &Value) -> Result<Value, RpcError> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
    if name != "/cursor" {
        return Err(RpcError::Protocol(format!("unknown command {name}")));
    }
    let argv: Vec<String> = params
        .get("argv")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    match argv.first().map(String::as_str).unwrap_or("") {
        "" => Ok(json!({})), // bare /cursor → provider-login shortcut
        "status" => {
            let text = match setup::logged_in() {
                Some(info) => format!(
                    "Cursor CLI: logged in as {} ({}, cli {})",
                    info.get("email").and_then(Value::as_str).unwrap_or("?"),
                    info.get("subscriptionTier").and_then(Value::as_str).unwrap_or("?"),
                    info.get("cliVersion").and_then(Value::as_str).unwrap_or("?"),
                ),
                None => match setup::about() {
                    Some(_) => "Cursor CLI present but not logged in — run `cursor-agent login`".into(),
                    None => setup::INSTALL_HINT.into(),
                },
            };
            Ok(json!({"text": text}))
        }
        _ => Err(RpcError::Protocol("usage: /cursor [status]".into())),
    }
}

// ---------- sidecar loop ----------

fn dispatch(
    method: &str,
    params: &Value,
    ops: &Ops,
    intents: &Intents,
    pool: &Arc<Pool>,
) -> Result<Value, RpcError> {
    match method {
        "plugin/manifest" => Ok(manifest()),
        "provider/auth/start" => auth_start(ops, params),
        "provider/auth/poll" => auth_poll(ops, params),
        "provider/auth/cancel" => auth_cancel(ops, params),
        "provider/auth/refresh" => auth_refresh(params),
        "provider/auth/revoke" => auth_revoke(params),
        "provider/usage" => usage::handle(&params).map_err(unavailable),
        "provider/models" => provider_models(params),
        "provider/chat" => provider_chat(intents, pool, params),
        "command/run" => command_run(params),
        "plugin/tools" => Ok(json!({"tools": []})),
        "event/notify" | "plugin/shutdown" => Ok(json!({})),
        _ => Err(RpcError::Protocol(format!("unknown method {method}"))),
    }
}

fn write_response(out: &mut impl Write, id: Value, outcome: Result<Value, RpcError>) -> bool {
    let response = match outcome {
        Ok(result) => json!({"id": id, "result": result}),
        Err(e) => json!({"id": id, "error": error_value(&e)}),
    };
    writeln!(out, "{response}")
        .and_then(|()| out.flush())
        .is_ok()
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest());
        return;
    }
    let ops: Ops = Arc::new(Mutex::new(HashMap::new()));
    let intents: Intents = Default::default();
    let pool: Arc<Pool> = Pool::new(Arc::new(CursorDriver));
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
        let result = dispatch(&method, &params, &ops, &intents, &pool);
        if !id.is_null() && !write_response(&mut out, id, result) {
            return;
        }
        if method == "plugin/shutdown" {
            break;
        }
    }
}
