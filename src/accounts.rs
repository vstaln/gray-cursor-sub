//! Multi-account pooling: several `cursor-agent` credential homes behind
//! one provider.
//!
//! cursor-agent partitions credentials by home: on Linux the file store
//! writes `auth.json` under `$XDG_CONFIG_HOME/cursor` (default
//! `~/.config/cursor`), data lives in `CURSOR_DATA_DIR` (default
//! `~/.cursor`); on macOS the default store is the login keychain —
//! `AGENT_CLI_CREDENTIAL_STORE=file` forces `~/.cursor/auth.json`. An
//! "account" here is one staged HOME plus a label, persisted in
//! `<gray home>/cursor-sub/accounts.json`. With no file (or an empty
//! `accounts` list) the plugin serves the ambient login exactly as
//! before — zero-config back-compat.
//!
//! Selection is per `provider/chat` turn: `fill_first` (the default)
//! takes the first non-cooled account in file order; `least_used` takes
//! the non-cooled account with the oldest `last_used_at`. `account use`
//! pins a label: a usable pinned account always serves, else the
//! strategy picks among the rest.
//!
//! Cooldowns: only account-scoped verdicts cool an account — a 401 (a
//! dead login) or a 429 (quota). An explicit `retry-after`/`resets …`
//! hint in the upstream message wins over the computed TTL. Request-
//! scoped failures (other 4xx, dead wires, malformed turns) never cool:
//! with one account they would mask the real error behind "all accounts
//! cooling". A lone configured account caps every cooldown at
//! [`SOLE_CAP`] for the same reason.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 401 TTL: a dead login, retried occasionally in case it healed.
const COOL_401: u64 = 5 * 60;
/// 429 TTL when the upstream message carries no usable reset hint.
const COOL_429: u64 = 60 * 60;
/// A lone configured account never sits out longer than this — a longer
/// bench would mask the real upstream verdict.
const SOLE_CAP: u64 = 60;
/// Sanity bound on a parsed reset hint (usage windows reset on a
/// bounded cycle; a bogus parse must not bench an account forever).
const MAX_RESET: u64 = 8 * 24 * 3600;

/// One credential home in the pool. `home` is the staged `$HOME` the
/// spawned `cursor-agent` children get; health fields are write-through.
#[derive(Serialize, Deserialize)]
pub struct Account {
    pub label: String,
    pub home: PathBuf,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub added_at: u64,
    /// Unix seconds the cooldown ends; 0 = not cooling.
    #[serde(default)]
    pub cooldown_until: u64,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub request_count: u64,
    #[serde(default)]
    pub last_used_at: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct Store {
    #[serde(default)]
    accounts: Vec<Account>,
    /// `account use` pin: a usable pinned account always serves.
    #[serde(default)]
    selected: Option<String>,
    /// `fill_first` (default) or `least_used`.
    #[serde(default)]
    strategy: String,
}

/// The account a turn is served by: label plus the staged home the
/// spawned `cursor-agent` children get.
pub struct Pick {
    pub label: String,
    pub home: PathBuf,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// `<gray home>/cursor-sub/` — the plugin's state directory.
fn state_dir() -> Option<PathBuf> {
    std::env::var_os("GRAY_HOME")
        .filter(|v| !v.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".gray")))
        .map(|h| h.join("cursor-sub"))
}

/// `~` expands at read so hand-edited configs and `--home ~` work.
pub fn expand_home(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if s == "~" {
        if let Some(h) = home_dir() {
            return h;
        }
    } else if let Some(rest) = s.strip_prefix("~/") {
        if let Some(h) = home_dir() {
            return h.join(rest);
        }
    }
    p.to_path_buf()
}

fn file_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("accounts.json"))
}

/// Managed homes live here: `<gray home>/cursor-sub/homes/<label>`.
fn homes_dir() -> Option<PathBuf> {
    state_dir().map(|d| d.join("homes"))
}

fn load() -> Store {
    let Some(path) = file_path() else {
        return Store::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Store::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save(store: &Store) -> Result<(), String> {
    let path = file_path().ok_or("cannot resolve GRAY_HOME")?;
    let text = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    write_atomic(&path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut mkdir = std::fs::DirBuilder::new();
    mkdir.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        mkdir.mode(0o700);
    }
    mkdir.create(dir)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let res = opts
        .open(&tmp)
        .and_then(|mut f| f.write_all(bytes))
        .and_then(|()| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// The env a `cursor-agent` child gets for one credential home. HOME is
/// staged so `~/.cursor` and `~/.config` resolve inside `home`;
/// XDG_CONFIG_HOME and CURSOR_DATA_DIR are pinned too so ambient values
/// can never leak this account's auth into (or out of) another home.
/// macOS additionally forces the file credential store — the default
/// login keychain is global and cannot be partitioned by env.
pub fn home_env(home: &Path) -> Vec<(String, String)> {
    let h = home.to_string_lossy().into_owned();
    let mut env = vec![
        ("HOME".to_string(), h.clone()),
        (
            "XDG_CONFIG_HOME".to_string(),
            Path::new(&h).join(".config").to_string_lossy().into_owned(),
        ),
        (
            "CURSOR_DATA_DIR".to_string(),
            Path::new(&h).join(".cursor").to_string_lossy().into_owned(),
        ),
    ];
    if cfg!(windows) {
        env.push(("USERPROFILE".to_string(), h.clone()));
    }
    if cfg!(target_os = "macos") {
        env.push(("AGENT_CLI_CREDENTIAL_STORE".to_string(), "file".to_string()));
    }
    env
}

/// The env the serving account's probes should run under: its staged
/// home when a pool is configured, else nothing (ambient).
pub fn serving_env() -> Vec<(String, String)> {
    serving()
        .ok()
        .flatten()
        .map(|p| home_env(&p.home))
        .unwrap_or_default()
}

fn cooled(a: &Account, now: u64) -> bool {
    a.cooldown_until > now
}

fn pick_of(a: &Account) -> Pick {
    Pick {
        label: a.label.clone(),
        home: expand_home(&a.home),
    }
}

fn pick_from(store: &Store) -> Result<Option<Pick>, String> {
    if store.accounts.is_empty() {
        return Ok(None);
    }
    let now = now_secs();
    // A usable pinned account always serves.
    if let Some(sel) = store.selected.as_deref() {
        if let Some(a) = store
            .accounts
            .iter()
            .find(|a| a.label == sel && !cooled(a, now))
        {
            return Ok(Some(pick_of(a)));
        }
    }
    let next = if store.strategy == "least_used" {
        store
            .accounts
            .iter()
            .filter(|a| !cooled(a, now))
            .min_by_key(|a| a.last_used_at)
    } else {
        // fill_first: file order is the priority order.
        store.accounts.iter().find(|a| !cooled(a, now))
    };
    if let Some(a) = next {
        return Ok(Some(pick_of(a)));
    }
    let soon = store
        .accounts
        .iter()
        .min_by_key(|a| a.cooldown_until)
        .expect("nonempty");
    Err(format!(
        "all {} cursor accounts are cooling down; {} next at {}",
        store.accounts.len(),
        soon.label,
        iso_timestamp(UNIX_EPOCH + Duration::from_secs(soon.cooldown_until)),
    ))
}

/// The account a turn would use — read-only (no use recorded). Probes
/// and `provider/auth/*` use this; only `provider/chat` calls [`pick`].
pub fn serving() -> Result<Option<Pick>, String> {
    pick_from(&load())
}

/// Per-turn selection: resolve the serving account and record the use.
pub fn pick() -> Result<Option<Pick>, String> {
    let mut store = load();
    let Some(p) = pick_from(&store)? else {
        return Ok(None);
    };
    if let Some(a) = store.accounts.iter_mut().find(|a| a.label == p.label) {
        a.request_count += 1;
        a.last_used_at = now_secs();
        let _ = save(&store);
    }
    Ok(Some(p))
}

/// The turn this account served succeeded: clear any recorded failure.
pub fn note_success(label: &str) {
    let mut store = load();
    let Some(a) = store.accounts.iter_mut().find(|a| a.label == label) else {
        return;
    };
    if a.cooldown_until != 0 || a.last_error.is_some() {
        a.cooldown_until = 0;
        a.last_error = None;
        let _ = save(&store);
    }
}

/// The turn this account served failed with `detail`. Only account-
/// scoped upstream verdicts cool it — and those are the only failures
/// worth a same-request retry on a sibling. Returns `true` when the
/// account was benched; `false` for request-scoped failures, which
/// leave the pool untouched and must surface to the caller as-is.
pub fn note_failure(label: &str, detail: &str) -> bool {
    let Some((code, msg)) = api_error(detail) else {
        return false;
    };
    let ttl = match code {
        401 => COOL_401,
        429 => parse_retry_after(msg)
            .map(|d| d.as_secs().min(MAX_RESET))
            .unwrap_or(COOL_429),
        // Request-scoped verdicts never cool an account.
        _ => return false,
    };
    cool(label, ttl, detail);
    true
}

fn cool(label: &str, ttl: u64, detail: &str) {
    let mut store = load();
    let ttl = if store.accounts.len() == 1 {
        ttl.min(SOLE_CAP)
    } else {
        ttl.min(MAX_RESET)
    };
    let Some(a) = store.accounts.iter_mut().find(|a| a.label == label) else {
        return;
    };
    a.cooldown_until = now_secs() + ttl.max(1);
    a.last_error = Some(detail.chars().take(200).collect());
    let _ = save(&store);
}

// ---- upstream-verdict classification --------------------------------------

/// `needle` appears in `hay` bounded by non-alphanumerics (so `429` in a
/// timestamp or byte count does not count).
fn word_hit(hay: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(i) = hay[start..].find(needle) {
        let i = start + i;
        let before_ok = hay[..i]
            .chars()
            .last()
            .map(|c| !c.is_ascii_alphanumeric())
            .unwrap_or(true);
        let after_ok = hay[i + needle.len()..]
            .chars()
            .next()
            .map(|c| !c.is_ascii_alphanumeric())
            .unwrap_or(true);
        if before_ok && after_ok {
            return true;
        }
        start = i + needle.len();
    }
    false
}

/// Split a turn-failure detail into `(status, detail)` when it names an
/// account-scoped upstream verdict. The ACP bridge reports failures as
/// plain strings (`session/prompt: …`, `agent exited mid-turn<stderr>`),
/// so we recognize an HTTP status token or the usual wording, and treat
/// anything else as request-scoped (never cooled).
fn api_error(detail: &str) -> Option<(u16, &str)> {
    let low = detail.to_lowercase();
    for code in [401u16, 429] {
        if word_hit(&low, &code.to_string()) {
            return Some((code, detail));
        }
    }
    const WORDS: &[(&[&str], u16)] = &[
        (
            &[
                "rate limit",
                "rate_limit",
                "ratelimit",
                "too many requests",
                "usage limit",
                "quota",
                "retry after",
            ],
            429,
        ),
        (
            &[
                "unauthorized",
                "unauthenticated",
                "authentication failed",
                "invalid api key",
                "invalid token",
                "expired token",
                "login required",
                "not logged in",
                "sign in",
            ],
            401,
        ),
    ];
    for (words, code) in WORDS {
        if words.iter().any(|w| low.contains(w)) {
            return Some((*code, detail));
        }
    }
    None
}

// ---- retry-after / reset-hint parsing ------------------------------------

/// `unit` as seconds; `None` when the word isn't a time unit.
fn unit_secs(unit: &str) -> Option<u64> {
    match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => Some(1),
        "m" | "min" | "mins" | "minute" | "minutes" => Some(60),
        "h" | "hr" | "hrs" | "hour" | "hours" => Some(3600),
        "d" | "day" | "days" => Some(86400),
        _ => None,
    }
}

/// Split text into digit/letter runs: `"2h 15m"` → `["2","h","15","m"]`.
fn atoms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tok in text.split_whitespace().take(10) {
        let mut cur = String::new();
        let mut digit = None;
        for c in tok.chars() {
            if !c.is_ascii_alphanumeric() {
                continue;
            }
            let d = c.is_ascii_digit();
            if digit.is_some_and(|x| x != d) {
                out.push(std::mem::take(&mut cur));
            }
            digit = Some(d);
            cur.push(c);
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

/// Parse a leading duration like `2h 15m`, `90 seconds`, `an hour`, or a
/// bare `90` (seconds). `None` when nothing duration-like leads.
fn dur_prefix(text: &str) -> Option<Duration> {
    let a = atoms(text);
    let (mut total, mut i, mut any) = (0u64, 0usize, false);
    while i < a.len() {
        let n: u64 = if a[i] == "a" || a[i] == "an" {
            1
        } else {
            match a[i].parse() {
                Ok(n) => n,
                Err(_) => break,
            }
        };
        i += 1;
        match a.get(i).and_then(|u| unit_secs(u)) {
            Some(u) => {
                total += n * u;
                i += 1;
            }
            // A bare number is seconds ("retry after 90").
            None => total += n,
        }
        any = true;
    }
    (any && total > 0).then(|| Duration::from_secs(total.min(MAX_RESET)))
}

/// Howard Hinnant's `days_from_civil`: days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(146097);
    let yoe = y - era * 400;
    let doy = (153 * if m > 2 { m - 3 } else { m + 9 } + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy as i64;
    era * 146097 + doe - 719468
}

/// A leading ISO-8601-ish datetime (`YYYY-MM-DD[ T]HH:MM[:SS][Z|±HH:MM]`)
/// as unix seconds. Missing time/zone reads as midnight UTC — a wrong
/// hint costs a longer bench, never a wrong answer.
fn iso_epoch(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    for i in 0..b.len().saturating_sub(9) {
        if !(digit(i) && digit(i + 1) && digit(i + 2) && digit(i + 3))
            || b.get(i + 4) != Some(&b'-')
            || b.get(i + 7) != Some(&b'-')
        {
            continue;
        }
        let (Ok(y), Ok(mo), Ok(da)) = (
            text[i..i + 4].parse::<i64>(),
            text[i + 5..i + 7].parse::<u32>(),
            text[i + 8..i + 10].parse::<u32>(),
        ) else {
            continue;
        };
        if !(1..=12).contains(&mo) || !(1..=31).contains(&da) {
            continue;
        }
        let (mut h, mut mi, mut se, mut off) = (0i64, 0i64, 0i64, 0i64);
        let j = i + 10;
        // Optional time after 'T' or a space.
        if matches!(b.get(j), Some(b'T') | Some(b't') | Some(b' ')) {
            let r = &text[j + 1..];
            let rb = r.as_bytes();
            if rb.len() >= 5
                && rb[0].is_ascii_digit()
                && rb[1].is_ascii_digit()
                && rb[2] == b':'
                && rb[3].is_ascii_digit()
                && rb[4].is_ascii_digit()
            {
                h = r[0..2].parse().ok()?;
                mi = r[3..5].parse().ok()?;
                let mut k = 5;
                if rb.get(k) == Some(&b':')
                    && rb.len() >= k + 3
                    && rb[k + 1].is_ascii_digit()
                    && rb[k + 2].is_ascii_digit()
                {
                    se = r[k + 1..k + 3].parse().unwrap_or(0);
                    k += 3;
                }
                // Optional zone: Z or ±HH[:MM].
                match rb.get(k) {
                    Some(b'Z') | Some(b'z') | None => {}
                    Some(&sign @ (b'+' | b'-')) => {
                        let z = &r[k + 1..];
                        let zh_len = z.bytes().take_while(|c| c.is_ascii_digit()).count();
                        if let Ok(zh) = z[..zh_len].parse::<i64>() {
                            let rest = &z[zh_len..];
                            let rest = rest.strip_prefix(':').unwrap_or(rest);
                            let zd: String =
                                rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                            let zm: i64 = zd.parse().unwrap_or(0);
                            let zi = zh * 3600 + zm * 60;
                            off = if sign == b'-' { -zi } else { zi };
                        }
                    }
                    _ => {}
                }
            }
        }
        return Some(days_from_civil(y, mo, da) * 86400 + h * 3600 + mi * 60 + se - off);
    }
    None
}

/// Honor an upstream-provided reset hint: `Retry-After`/retry-in
/// seconds, `resets in <duration>`, or `resets at <datetime>`.
fn parse_retry_after(msg: &str) -> Option<Duration> {
    let low = msg.to_lowercase();
    for key in ["retry-after", "retry after"] {
        if let Some(i) = low.find(key) {
            if let Some(d) = dur_prefix(low[i + key.len()..].trim_start_matches([':', '='])) {
                return Some(d);
            }
        }
    }
    for pat in ["resets in ", "reset in ", "try again in ", "retry in "] {
        if let Some(i) = low.find(pat) {
            if let Some(d) = dur_prefix(&low[i + pat.len()..]) {
                return Some(d);
            }
        }
    }
    for pat in ["resets at ", "reset at "] {
        if let Some(i) = low.find(pat) {
            if let Some(t) = iso_epoch(&msg[i + pat.len()..]) {
                let now = now_secs() as i64;
                if t > now {
                    return Some(Duration::from_secs((t - now) as u64));
                }
            }
        }
    }
    None
}

/// `t` as `YYYY-MM-DD HH:MM:SS UTC`, for human-facing cooldown displays.
fn iso_timestamp(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

// ---- `account` command ----------------------------------------------------

const USAGE: &str = "\
cursor-sub account — multiple Cursor logins behind one provider.
  cursor-sub account add [--home <dir>] <label>   run `cursor-agent login` into a
                                                  new credential home (or adopt
                                                  an existing staged home)
  cursor-sub account list                          accounts + health
  cursor-sub account use <label>                   pin an account (use --clear to unpin)
  cursor-sub account remove [--keep] <label>       drop an account (+its managed home)
  cursor-sub account status                        which account serves now
The file is <gray home>/cursor-sub/accounts.json; absent or empty
`accounts` keeps the single ambient `cursor-agent` login.";

/// `cursor-sub account …` (interactive: a real terminal) and `/cursor
/// account …` (inside the sidecar — `add` can't drive a terminal login
/// there, though `add --home` adoption still works).
pub fn command(args: &[String], interactive: bool) -> Result<String, String> {
    match args.first().map(String::as_str) {
        Some("add") => cmd_add(&args[1..], interactive),
        Some("use") => cmd_use(&args[1..]),
        Some("remove") | Some("rm") => cmd_remove(&args[1..]),
        Some("status") => cmd_status(),
        Some("list") | None => cmd_list(),
        Some(other) => Err(format!("unknown account verb {other:?}\n\n{USAGE}")),
    }
}

fn valid_label(label: &str) -> Result<(), String> {
    let ok = !label.is_empty()
        && label.len() <= 40
        && label
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then_some(())
        .ok_or_else(|| format!("bad label {label:?}: 1-40 chars of [A-Za-z0-9_-]"))
}

/// The login a credential home currently holds: `(email, tier)` from
/// `cursor-agent about` run under [`home_env`]. `userEmail` null means
/// the home has no completed login.
fn probe_login(home: &Path) -> Option<(String, String)> {
    let about = crate::setup::about_env(&home_env(home))?;
    let email = about
        .get("userEmail")
        .and_then(Value::as_str)
        .filter(|e| !e.is_empty())?
        .to_string();
    let tier = about
        .get("subscriptionTier")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some((email, tier))
}

fn cmd_add(args: &[String], interactive: bool) -> Result<String, String> {
    let mut home: Option<PathBuf> = None;
    let mut label: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--home" {
            let Some(h) = it.next() else {
                return Err("--home needs a directory".into());
            };
            home = Some(expand_home(Path::new(h)));
        } else if a.starts_with('-') {
            return Err(format!("unknown flag {a:?}\n\n{USAGE}"));
        } else if label.is_none() {
            label = Some(a.clone());
        } else {
            return Err(USAGE.into());
        }
    }
    let label = label.ok_or_else(|| format!("account add needs a label\n\n{USAGE}"))?;
    valid_label(&label)?;
    let mut store = load();
    if store.accounts.iter().any(|a| a.label == label) {
        return Err(format!("account {label:?} already exists"));
    }
    let binary = crate::setup::resolve_command().ok_or(crate::setup::INSTALL_HINT)?;
    let managed = home.is_none();
    let home = match home {
        Some(h) => h,
        None => homes_dir().ok_or("cannot resolve GRAY_HOME")?.join(&label),
    };
    if managed {
        if !interactive {
            return Err(
                "account add runs `cursor-agent login`; use `gray cursor-sub account add <label>` in a terminal"
                    .into(),
            );
        }
        std::fs::create_dir_all(&home).map_err(|e| format!("create {}: {e}", home.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700));
        }
        eprintln!("logging {label:?} into {} …", home.display());
        let status = std::process::Command::new(&binary)
            .arg("login")
            .envs(home_env(&home))
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| format!("spawn cursor-agent login: {e}"))?;
        if !status.success() {
            return Err(format!("`cursor-agent login` exited {status}"));
        }
    }
    let Some((email, tier)) = probe_login(&home) else {
        return Err(format!(
            "no login in {} — {}",
            home.display(),
            if managed {
                "`cursor-agent login` did not complete"
            } else {
                "adopt a directory that holds a real login"
            },
        ));
    };
    // Dedupe on the verified upstream identity only — a null email never
    // proves two homes are the same account.
    if let Some(dup) = store
        .accounts
        .iter()
        .find(|a| a.email.as_deref() == Some(email.as_str()))
    {
        return Err(format!(
            "that login is already registered as {:?} ({email})",
            dup.label
        ));
    }
    store.accounts.push(Account {
        label: label.clone(),
        home: home.clone(),
        email: Some(email.clone()),
        tier: if tier.is_empty() { None } else { Some(tier) },
        added_at: now_secs(),
        cooldown_until: 0,
        last_error: None,
        request_count: 0,
        last_used_at: 0,
    });
    if store.selected.is_none() {
        store.selected = Some(label.clone());
    }
    save(&store)?;
    Ok(format!(
        "account {label:?} added ({email}); home {}",
        home.display()
    ))
}

fn cmd_use(args: &[String]) -> Result<String, String> {
    let mut store = load();
    match args.first().map(String::as_str) {
        None => {
            let sel = store.selected.as_deref().unwrap_or("(none)");
            Ok(format!("pinned account: {sel}"))
        }
        Some("--clear") | Some("none") | Some("auto") => {
            store.selected = None;
            save(&store)?;
            Ok("pin cleared — selection follows the strategy".to_string())
        }
        Some(label) => {
            if !store.accounts.iter().any(|a| a.label == label) {
                return Err(format!("no account {label:?}"));
            }
            store.selected = Some(label.to_string());
            save(&store)?;
            Ok(format!("serving account: {label}"))
        }
    }
}

fn cmd_remove(args: &[String]) -> Result<String, String> {
    let mut keep = false;
    let mut label: Option<String> = None;
    for a in args {
        if a == "--keep" {
            keep = true;
        } else if label.is_none() {
            label = Some(a.clone());
        } else {
            return Err(USAGE.into());
        }
    }
    let label = label.ok_or_else(|| format!("account remove needs a label\n\n{USAGE}"))?;
    let mut store = load();
    let Some(i) = store.accounts.iter().position(|a| a.label == label) else {
        return Err(format!("no account {label:?}"));
    };
    let acc = store.accounts.remove(i);
    if store.selected.as_deref() == Some(label.as_str()) {
        store.selected = None;
    }
    save(&store)?;
    // Only homes we created are ours to delete; adopted `--home` dirs
    // belong to the user.
    let mut msg = format!("account {label:?} removed");
    if !keep {
        if let Some(dir) = homes_dir() {
            if acc.home.starts_with(&dir) && acc.home.is_dir() {
                std::fs::remove_dir_all(&acc.home).map_err(|e| {
                    format!("removed {label:?} but not {}: {e}", acc.home.display())
                })?;
                msg.push_str(&format!(" and its credential home {}", acc.home.display()));
            }
        }
    }
    Ok(msg)
}

fn account_line(a: &Account, now: u64, selected: &Option<String>) -> String {
    let mark = if selected.as_deref() == Some(a.label.as_str()) {
        '*'
    } else {
        ' '
    };
    let state = if cooled(a, now) {
        format!(
            "cooled until {}",
            iso_timestamp(UNIX_EPOCH + Duration::from_secs(a.cooldown_until))
        )
    } else {
        "ok".to_string()
    };
    let who = a.email.clone().unwrap_or_else(|| "-".to_string());
    format!(
        "{mark} {:<16} {:<12} {:<30} {}",
        a.label,
        state,
        who,
        a.home.display()
    )
}

fn cmd_list() -> Result<String, String> {
    let store = load();
    if store.accounts.is_empty() {
        return Ok(
            "no accounts configured — turns use the ambient `cursor-agent` login".to_string(),
        );
    }
    let now = now_secs();
    let mut out = format!("{} account(s):\n", store.accounts.len());
    for a in &store.accounts {
        out.push_str(&account_line(a, now, &store.selected));
        out.push('\n');
    }
    out.push_str("`use` pins *; fill_first order is file order.");
    Ok(out)
}

fn cmd_status() -> Result<String, String> {
    let store = load();
    if store.accounts.is_empty() {
        return Ok("no accounts configured — serving the ambient `cursor-agent` login".to_string());
    }
    let mut out = match pick_from(&store) {
        Ok(Some(p)) => format!("serving account: {} (home {})\n", p.label, p.home.display()),
        Ok(None) => "serving account: (none)\n".to_string(),
        Err(e) => format!("serving account: {e}\n"),
    };
    out.push_str(&format!(
        "strategy: {}\n",
        if store.strategy.is_empty() {
            "fill_first"
        } else {
            &store.strategy
        }
    ));
    let now = now_secs();
    for a in &store.accounts {
        out.push_str(&account_line(a, now, &store.selected));
        if let Some(e) = &a.last_error {
            out.push_str(&format!("  last error: {e}"));
        }
        out.push('\n');
    }
    Ok(out)
}

/// Short "serving account" suffix for `/cursor status`; empty when no
/// pool is configured.
pub fn status_suffix() -> String {
    match serving() {
        Ok(Some(p)) => format!("serving account: {} ({})", p.label, p.home.display()),
        Ok(None) => String::new(),
        Err(e) => e,
    }
}
