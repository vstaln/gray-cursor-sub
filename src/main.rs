use gray_acp::relay::{self, Intents, TurnFn};
use gray_acp::session::{self, Pool};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use cursor_sub::{accounts, driver::CursorDriver, models, sdk, setup, usage};

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
            "name": "Cursor",
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
        "command_descriptions": {"/cursor": "switch to Cursor models"},
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

/// The login that would serve turns: the pooled account's staged home
/// when configured, else the ambient `cursor-agent` login.
fn login_info() -> Option<Value> {
    setup::logged_in_env(&accounts::serving_env())
}

fn auth_start(ops: &Ops, params: &Value) -> Result<Value, RpcError> {
    let method = params
        .get("auth_method")
        .and_then(Value::as_str)
        .unwrap_or_default();
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
        return Err(unavailable(
            "set CURSOR_API_KEY first, or /connect with a pasted key",
        ));
    }
    let bin = setup::resolve_command().ok_or_else(|| unavailable(setup::INSTALL_HINT))?;
    if login_info().is_some() {
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
    let op_id = params
        .get("operation_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // An immediate-complete op (empty id) just re-probes the CLI login.
    if op_id.is_empty() {
        return Ok(match login_info() {
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
    if let Some(info) = login_info() {
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
    let op_id = params
        .get("operation_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(mut op) = ops.lock().unwrap().remove(op_id) {
        if let Some(mut c) = op.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
    Ok(json!({}))
}

fn auth_refresh(params: &Value) -> Result<Value, RpcError> {
    let method = params
        .get("auth_method")
        .and_then(Value::as_str)
        .unwrap_or_default();
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
    match login_info() {
        Some(info) => Ok(json!({"credential": login_material(&info)})),
        None => Err(RpcError::Rpc {
            code: "invalid_grant",
            message: setup::LOGIN_HINT.into(),
        }),
    }
}

fn auth_revoke(params: &Value) -> Result<Value, RpcError> {
    let method = params
        .get("auth_method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if method == AUTH_CLI {
        if let Some(bin) = setup::resolve_command() {
            let _ = Command::new(bin)
                .arg("logout")
                .envs(accounts::serving_env())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    } else {
        setup::remove_api_key_cache();
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

enum ApiTransport {
    Sdk(&'static std::path::Path),
    Cli,
}

fn api_transport() -> Result<ApiTransport, RpcError> {
    if setup::endpoint().is_some() {
        return setup::resolve_command()
            .map(|_| ApiTransport::Cli)
            .ok_or_else(|| unavailable("CURSOR_API_ENDPOINT requires the Cursor CLI"));
    }
    match sdk::runtime() {
        Ok(root) => Ok(ApiTransport::Sdk(root)),
        Err(_)
            if std::env::var_os("GRAY_CURSOR_SUB_SDK_ROOT").is_none()
                && setup::resolve_command().is_some() =>
        {
            Ok(ApiTransport::Cli)
        }
        Err(error) => Err(unavailable(format!(
            "{error}; install the Cursor SDK runtime or Cursor CLI"
        ))),
    }
}

fn gate_cli_auth(account: Option<&accounts::Pick>) -> Result<(), RpcError> {
    if setup::resolve_command().is_none() {
        return Err(unavailable(setup::INSTALL_HINT));
    }
    match account {
        Some(p) if setup::logged_in_env(&accounts::home_env(&p.home)).is_none() => {
            Err(unavailable(format!(
                "cursor account {:?} has no login in {}",
                p.label,
                p.home.display()
            )))
        }
        None if setup::logged_in().is_none() => Err(unavailable(setup::LOGIN_HINT)),
        _ => Ok(()),
    }
}

fn provider_models(params: &Value) -> Result<Value, RpcError> {
    let method = params
        .get("auth_method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let rows = if method == AUTH_KEY {
        let pasted = params
            .pointer("/credential/material/secrets/api_key")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|key| !key.is_empty());
        let key = pasted
            .map(str::to_owned)
            .or_else(setup::api_key)
            .ok_or_else(|| unavailable("set CURSOR_API_KEY for this auth method"))?;
        let rows = match api_transport()? {
            ApiTransport::Sdk(root) => sdk::catalog(root, &key).map_err(unavailable)?,
            ApiTransport::Cli => models::catalog(&setup::cli_env(Vec::new(), Some(&key))),
        };
        if pasted.is_some() && !rows.is_empty() {
            setup::write_api_key_cache(&key)
                .map_err(|e| unavailable(format!("Cursor API key cache: {e}")))?;
        }
        rows
    } else {
        let account = accounts::serving().map_err(unavailable)?;
        gate_cli_auth(account.as_ref())?;
        let env = account
            .map(|p| accounts::home_env(&p.home))
            .unwrap_or_default();
        models::catalog(&setup::cli_env(env, None))
    };
    if rows.is_empty() {
        return Err(unavailable("Cursor model discovery returned no models"));
    }
    Ok(json!({
        "provider": PROVIDER_ID,
        "models": rows,
        "default_model": rows.first().and_then(|m| m.get("id")).cloned().unwrap_or(Value::Null),
    }))
}

/// One pooled ACP session-pool per credential home (key `""` = ambient).
fn pool_for(key: &str, env: Vec<(String, String)>) -> Arc<Pool> {
    static POOLS: OnceLock<Mutex<HashMap<String, Arc<Pool>>>> = OnceLock::new();
    let pools = POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    pools
        .lock()
        .unwrap()
        .entry(key.to_string())
        .or_insert_with(|| Pool::new(Arc::new(CursorDriver { env })))
        .clone()
}

fn cli_key_pool_for(key: &str) -> Arc<Pool> {
    static POOL: OnceLock<Mutex<(String, Arc<Pool>)>> = OnceLock::new();
    let new_pool = || {
        Pool::new(Arc::new(CursorDriver {
            env: setup::cli_env(Vec::new(), Some(key)),
        }))
    };
    let mut state = POOL
        .get_or_init(|| Mutex::new((key.to_owned(), new_pool())))
        .lock()
        .unwrap();
    if state.0 != key {
        *state = (key.to_owned(), new_pool());
    }
    Arc::clone(&state.1)
}

fn provider_chat(
    sdk_pool: &Arc<sdk::Pool>,
    intents: &Intents,
    params: &Value,
) -> Result<Value, RpcError> {
    let provider = params
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if provider != PROVIDER_ID {
        return Err(RpcError::Protocol("unknown provider".into()));
    }
    let model = params
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let turn: TurnFn = if params.get("auth_method").and_then(Value::as_str) == Some(AUTH_KEY) {
        let key = setup::api_key()
            .ok_or_else(|| unavailable("set CURSOR_API_KEY for this auth method"))?;
        match api_transport()? {
            ApiTransport::Sdk(root) => {
                let pool = Arc::clone(sdk_pool);
                let model = model.to_owned();
                Arc::new(move |raw, cancel| pool.run(root, &key, &model, raw, cancel))
            }
            ApiTransport::Cli => {
                let pool = cli_key_pool_for(&key);
                Arc::new(move |raw, cancel| session::run_turn(&pool, raw, cancel))
            }
        }
    } else {
        gate_cli_auth(accounts::serving().map_err(unavailable)?.as_ref())?;
        Arc::new(cli_subscription_turn)
    };
    let (relay_url, relay_token) =
        relay::mint_turn(intents, model, turn, TOKEN_PREFIX).map_err(unavailable)?;
    Ok(json!({
        "relay_url": relay_url,
        "relay_token": relay_token,
    }))
}

fn cli_subscription_turn(raw: &[u8], cancel: &AtomicBool) -> Result<Vec<u8>, String> {
    let mut tried = std::collections::BTreeSet::new();
    let mut last_err = String::new();
    loop {
        let account = match accounts::pick() {
            Ok(Some(p)) => p,
            Ok(None) => {
                let pool = pool_for("", setup::cli_env(Vec::new(), None));
                return session::run_turn(&pool, raw, cancel);
            }
            Err(e) => {
                return Err(if last_err.is_empty() {
                    e
                } else {
                    format!("{e} · last error: {last_err}")
                });
            }
        };
        if !tried.insert(account.label.clone()) {
            // pick() never hands back a cooled label, so a repeat
            // means the bench could not persist — bail rather than
            // spin on one bad account.
            return Err(format!(
                "cursor account {:?} failed again: {last_err}",
                account.label
            ));
        }
        let key = account.home.to_string_lossy().into_owned();
        let pool = pool_for(
            &key,
            setup::cli_env(accounts::home_env(&account.home), None),
        );
        match session::run_turn(&pool, raw, cancel) {
            Ok(sse) => {
                // Account health rides the verdict: a served answer
                // clears the slate.
                accounts::note_success(&account.label);
                return Ok(sse);
            }
            Err(e) => {
                if !accounts::note_failure(&account.label, &e) {
                    return Err(e);
                }
                last_err = e;
            }
        }
    }
}

fn command_run(params: &Value) -> Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if name != "/cursor" {
        return Err(RpcError::Protocol(format!("unknown command {name}")));
    }
    let argv: Vec<String> = params
        .get("argv")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    match argv.first().map(String::as_str).unwrap_or("") {
        "" => Ok(json!({})), // bare /cursor → provider-login shortcut
        "status" => {
            let mut text = match login_info() {
                Some(info) => format!(
                    "Cursor CLI: logged in as {} ({}, cli {})",
                    info.get("email").and_then(Value::as_str).unwrap_or("?"),
                    info.get("subscriptionTier")
                        .and_then(Value::as_str)
                        .unwrap_or("?"),
                    info.get("cliVersion")
                        .and_then(Value::as_str)
                        .unwrap_or("?"),
                ),
                None => match setup::about() {
                    Some(_) => {
                        "Cursor CLI present but not logged in — run `cursor-agent login`".into()
                    }
                    None => setup::INSTALL_HINT.into(),
                },
            };
            if setup::api_key().is_some() {
                let transport: &str = match api_transport() {
                    Ok(ApiTransport::Sdk(_)) => "Cursor SDK ready (API-key auth configured)",
                    Ok(ApiTransport::Cli) => {
                        "Cursor API-key auth using CLI compatibility transport"
                    }
                    Err(_) => "Cursor API key configured; install the SDK runtime or Cursor CLI",
                };
                text.push_str(&format!("\n{transport}"));
            }
            let suffix = accounts::status_suffix();
            if !suffix.is_empty() {
                text.push_str(&format!("\n{suffix}"));
            }
            Ok(json!({"text": text}))
        }
        "account" => Ok(json!({
            "text": accounts::command(&argv[1..], false).unwrap_or_else(|e| e),
        })),
        _ => Err(RpcError::Protocol(
            "usage: /cursor [status|account …]".into(),
        )),
    }
}

// ---------- sidecar loop ----------

fn dispatch(
    method: &str,
    params: &Value,
    ops: &Ops,
    intents: &Intents,
    sdk_pool: &Arc<sdk::Pool>,
) -> Result<Value, RpcError> {
    match method {
        "plugin/manifest" => Ok(manifest()),
        "provider/auth/start" => auth_start(ops, params),
        "provider/auth/poll" => auth_poll(ops, params),
        "provider/auth/cancel" => auth_cancel(ops, params),
        "provider/auth/refresh" => auth_refresh(params),
        "provider/auth/revoke" => auth_revoke(params),
        "provider/usage" => usage::handle(params).map_err(unavailable),
        "provider/models" => provider_models(params),
        "provider/chat" => provider_chat(sdk_pool, intents, params),
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
    match std::env::args().nth(1).as_deref() {
        Some("manifest") => {
            println!("{}", manifest());
            return;
        }
        // `gray cursor-sub account …`: the multi-account pool — `add`
        // drives the real `cursor-agent login` into a fresh credential
        // home, so it needs this terminal, not the sidecar's NDJSON pipe.
        Some("account") => {
            let args: Vec<String> = std::env::args_os()
                .skip(2)
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            match accounts::command(&args, std::io::IsTerminal::is_terminal(&std::io::stdin())) {
                Ok(text) => {
                    println!("{text}");
                    return;
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(2);
                }
            }
        }
        _ => {}
    }
    let ops: Ops = Arc::new(Mutex::new(HashMap::new()));
    let intents: Intents = Default::default();
    let sdk_pool = Arc::new(sdk::Pool::default());
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
        let method = req
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
        let result = dispatch(&method, &params, &ops, &intents, &sdk_pool);
        if !id.is_null() && !write_response(&mut out, id, result) {
            break;
        }
        if method == "plugin/shutdown" {
            break;
        }
    }
    sdk_pool.close();
}
