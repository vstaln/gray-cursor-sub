//! Binary resolution + credential probing for cursor-agent.

use serde_json::{json, Value};
use std::process::{Command, Stdio};

pub const INSTALL_HINT: &str = "Cursor CLI not found — install from cursor.com/cli";
pub const LOGIN_HINT: &str =
    "Cursor CLI not logged in — use `/cursor` to connect, or run `cursor-agent login`";
pub const AUTH_METHOD_ID: &str = "cursor_login";

const BINARY_ENV: &str = "GRAY_CURSOR_SUB_COMMAND";
const API_KEY_ENV: &str = "CURSOR_API_KEY";
const ENDPOINT_ENV: &str = "CURSOR_API_ENDPOINT";

pub fn resolve_command() -> Option<String> {
    if let Ok(explicit) = std::env::var(BINARY_ENV) {
        let explicit = explicit.trim().to_string();
        if !explicit.is_empty() {
            return Some(explicit);
        }
    }
    which_on_path("cursor-agent")
}

/// The `about` payload: {cliVersion, userEmail, subscriptionTier, ...}.
/// userEmail == null means unauthenticated. `env` stages a credential
/// home (see [`crate::accounts::home_env`]); empty probes the ambient CLI.
pub fn about_env(env: &[(String, String)]) -> Option<Value> {
    let bin = resolve_command()?;
    let out = Command::new(&bin)
        .args(["about", "--format", "json"])
        .envs(env.iter().cloned())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice::<Value>(&out.stdout).ok()
}

/// Ambient [`about_env`] — no staged home.
pub fn about() -> Option<Value> {
    about_env(&[])
}

/// Some(json) when the given credential home is logged in — metadata
/// fields for the credential.
pub fn logged_in_env(env: &[(String, String)]) -> Option<Value> {
    let about = about_env(env)?;
    let email = about.get("userEmail").and_then(Value::as_str)?;
    if email.is_empty() {
        return None;
    }
    Some(json!({
        "email": email,
        "cliVersion": about.get("cliVersion").and_then(Value::as_str).unwrap_or(""),
        "subscriptionTier": about.get("subscriptionTier").and_then(Value::as_str).unwrap_or(""),
    }))
}

/// Ambient [`logged_in_env`] — no staged home.
pub fn logged_in() -> Option<Value> {
    logged_in_env(&[])
}

/// API key resolution order: explicit flag env > gray-managed cache > env.
pub fn api_key() -> Option<String> {
    if let Ok(k) = std::env::var(API_KEY_ENV) {
        let k = k.trim().to_string();
        if !k.is_empty() {
            return Some(k);
        }
    }
    read_api_key_cache()
}

fn cache_path() -> Option<std::path::PathBuf> {
    std::env::var("GRAY_CURSOR_SUB_CACHE")
        .map(std::path::PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .ok()
                .map(|h| std::path::Path::new(&h).join(".cursor-sub.json"))
        })
}

/// Cache the key the host collected so provider/chat (which gets no
/// credential in its params) can reach it.
pub fn write_api_key_cache(key: &str) -> std::io::Result<()> {
    let Some(path) = cache_path() else {
        return Ok(());
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).truncate(true).write(true).mode(0o600);
        let mut f = opts.open(path)?;
        use std::io::Write;
        f.write_all(format!("{}\n", json!({"api_key": key})).as_bytes())
    }
    #[cfg(not(unix))]
    std::fs::write(path, format!("{}\n", json!({"api_key": key})))
}

pub fn read_api_key_cache() -> Option<String> {
    let path = cache_path()?;
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let k = v.get("api_key").and_then(Value::as_str)?.trim().to_string();
    if k.is_empty() {
        None
    } else {
        Some(k)
    }
}

/// Spawn argv for the ACP child (endpoint + api-key injection).
pub fn acp_argv() -> Vec<String> {
    let mut args = Vec::new();
    if let Ok(endpoint) = std::env::var(ENDPOINT_ENV) {
        let endpoint = endpoint.trim();
        if !endpoint.is_empty() {
            args.push("-e".into());
            args.push(endpoint.into());
        }
    }
    if let Some(key) = api_key() {
        args.push("--api-key".into());
        args.push(key);
    }
    if let Ok(mode) = std::env::var("GRAY_CURSOR_SUB_PERMISSION") {
        match mode.trim() {
            "auto" | "auto-review" => args.push("--auto-review".into()),
            "full-access" | "force" => args.push("--force".into()),
            _ => {}
        }
    }
    args.push("acp".into());
    args
}

/// PATH lookup for an executable name (no new deps).
fn which_on_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for candidate in exe_candidates(&dir, name) {
            if std::fs::metadata(&candidate)
                .map(|m| m.is_file())
                .unwrap_or(false)
            {
                // executable bit check
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(m) = std::fs::metadata(&candidate) {
                        if m.permissions().mode() & 0o111 == 0 {
                            continue;
                        }
                    }
                }
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// On unix the bare name; on Windows the name plus each PATHEXT suffix
/// (cursor-agent may be installed as .exe/.cmd).
#[cfg(unix)]
fn exe_candidates(dir: &std::path::Path, name: &str) -> Vec<std::path::PathBuf> {
    vec![dir.join(name)]
}

#[cfg(windows)]
fn exe_candidates(dir: &std::path::Path, name: &str) -> Vec<std::path::PathBuf> {
    let mut v = vec![dir.join(name)];
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    for ext in pathext.split(';').filter(|e| !e.is_empty()) {
        v.push(dir.join(format!("{name}{ext}")));
    }
    v
}
