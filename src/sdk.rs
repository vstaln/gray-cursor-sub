use gray_acp::chat::{self, OutItem, PreparedTurn, TurnResult, Usage};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const CATALOG_TIMEOUT: Duration = Duration::from_secs(20);
const TURN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const PARK_WINDOW: Duration = Duration::from_millis(180);
const MAX_SESSIONS: usize = 8;

pub fn runtime() -> Result<&'static Path, String> {
    static ROOT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = sdk_root()?;
        let worker = Worker::spawn(&root, None)?;
        if worker.reply(PROBE_TIMEOUT)?["type"] != "ready" {
            return Err("unexpected SDK startup response".into());
        }
        Ok(root)
    })
    .as_deref()
    .map_err(Clone::clone)
}

fn sdk_root() -> Result<PathBuf, String> {
    if let Some(root) = std::env::var_os("GRAY_CURSOR_SUB_SDK_ROOT").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(root));
    }
    let exe = std::env::current_exe().map_err(|e| format!("cursor-sub path: {e}"))?;
    exe.ancestors()
        .skip(1)
        .take(3)
        .find(|p| p.join("node_modules/@cursor/sdk/package.json").is_file())
        .map(Path::to_path_buf)
        .ok_or_else(|| "@cursor/sdk is not installed; see gray-cursor-sub setup".to_string())
}

pub fn catalog(root: &Path, key: &str) -> Result<Vec<Value>, String> {
    let mut worker = Worker::spawn(root, Some(key))?;
    if worker.reply(PROBE_TIMEOUT)?["type"] != "ready" {
        return Err("unexpected SDK startup response".into());
    }
    worker.send(&json!({"type": "catalog"}))?;
    let frame = worker.reply(CATALOG_TIMEOUT)?;
    let models = frame["models"]
        .as_array()
        .ok_or("invalid Cursor SDK model catalog")?;
    let mut rows = Vec::new();
    for model in models {
        let Some(id) = model["id"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        let name = model["displayName"].as_str().unwrap_or(id);
        let id = if id == "default" { "auto" } else { id };
        if !rows.iter().any(|row: &Value| row["id"] == id) {
            rows.push(json!({"id": id, "name": name}));
        }
    }
    if let Some(index) = rows.iter().position(|r| r["id"] == "auto") {
        rows[..=index].rotate_right(1);
    }
    Ok(rows)
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Result<Value, String>>,
}

impl Worker {
    fn spawn(root: &Path, key: Option<&str>) -> Result<Self, String> {
        let node = std::env::var_os("GRAY_CURSOR_SUB_NODE")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "node".into());
        let mut command = Command::new(node);
        match key {
            Some(key) => command.env("CURSOR_API_KEY", key),
            None => command.env_remove("CURSOR_API_KEY"),
        };
        let mut child = command
            .current_dir(root)
            .args(["--input-type=module", "-e", include_str!("sdk.mjs")])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("Cursor SDK Node startup: {e}"))?;
        let stdin = child.stdin.take().expect("piped Node stdin");
        let stdout = child.stdout.take().expect("piped Node stdout");
        let (tx, rx) = mpsc::sync_channel(128);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let frame = line
                    .map_err(|_| "could not read Node worker output".into())
                    .and_then(|line| {
                        serde_json::from_str(&line).map_err(|_| "invalid Node worker frame".into())
                    });
                if tx.send(frame).is_err() {
                    return;
                }
            }
        });
        Ok(Self { child, stdin, rx })
    }

    fn send(&mut self, value: &Value) -> Result<(), String> {
        serde_json::to_writer(&mut self.stdin, value)
            .map_err(|e| format!("Cursor SDK wire: {e}"))?;
        self.stdin
            .write_all(b"\n")
            .map_err(|e| format!("Cursor SDK wire: {e}"))
    }

    fn recv(&self, timeout: Duration) -> Result<Option<Value>, String> {
        match self.rx.recv_timeout(timeout) {
            Ok(Ok(frame)) if frame["type"] == "error" => Err(format!(
                "Cursor SDK: {}",
                frame["message"].as_str().unwrap_or("unknown error")
            )),
            Ok(Ok(frame)) => Ok(Some(frame)),
            Ok(Err(e)) => Err(format!("Cursor SDK wire: {e}")),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("Cursor SDK worker exited".into()),
        }
    }

    fn reply(&self, timeout: Duration) -> Result<Value, String> {
        self.recv(timeout)?
            .ok_or_else(|| "Cursor SDK worker timed out".into())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if self.send(&json!({"type": "close"})).is_ok() {
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(2) {
                match self.recv(Duration::from_millis(100)) {
                    Ok(Some(frame)) if frame["type"] == "closed" => break,
                    Err(_) if !matches!(self.child.try_wait(), Ok(None)) => break,
                    _ => {}
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
pub struct Pool(Mutex<Vec<Session>>);

struct Session {
    worker: Worker,
    stage: tempfile::TempDir,
    key: String,
    model: String,
    effort: Option<String>,
    overrides: Vec<(String, String)>,
    system: String,
    absorbed: Vec<Value>,
    reply_call_ids: Vec<String>,
    reply_text: String,
    pending: BTreeMap<String, String>,
    queued: Option<Value>,
    touched: Instant,
}

impl Session {
    fn matches(&self, prep: &PreparedTurn, key: &str) -> bool {
        self.model == prep.model
            && self.overrides == prep.overrides
            && self.effort == prep.effort
            && self.system == prep.system
            && self.key == key
    }

    fn open(root: &Path, key: &str, prep: &PreparedTurn) -> Result<Self, String> {
        let stage = tempfile::tempdir().map_err(|e| format!("Cursor SDK stage: {e}"))?;
        let worker = Worker::spawn(root, Some(key))?;
        Ok(Self {
            worker,
            stage,
            key: key.to_owned(),
            model: prep.model.clone(),
            effort: prep.effort.clone(),
            overrides: prep.overrides.clone(),
            system: prep.system.clone(),
            absorbed: Vec::new(),
            reply_call_ids: Vec::new(),
            reply_text: String::new(),
            pending: BTreeMap::new(),
            queued: None,
            touched: Instant::now(),
        })
    }

    fn prompt(&self, prep: &PreparedTurn, fresh: bool) -> Option<Value> {
        let mut text = Vec::new();
        let mut images = Vec::new();
        if fresh {
            if !prep.system.is_empty() {
                text.push(format!("System instructions:\n{}", prep.system));
            }
            text.push(gray_acp::guard::ONE_TURN_RULE.into());
        }
        for block in &prep.tail_prompt {
            match block["type"].as_str() {
                Some("text") => {
                    if let Some(t) = block["text"].as_str() {
                        text.push(t.to_owned());
                    }
                }
                Some("image") => {
                    images.push(json!({"data": block["data"], "mimeType": block["mimeType"]}))
                }
                _ => {}
            }
        }
        for (id, output) in &prep.outputs {
            text.push(format!("Tool result for {id}: {output}"));
        }
        if text.is_empty() && images.is_empty() {
            return None;
        }
        let text = text.join("\n\n");
        let message = if images.is_empty() {
            json!(text)
        } else {
            json!({"text": text, "images": images})
        };
        Some(json!({
            "type": "turn", "cwd": self.stage.path(), "message": message,
            "model": self.model, "effort": self.effort, "overrides": self.overrides,
            "mode": std::env::var("GRAY_CURSOR_SUB_MODE").unwrap_or_default(),
            "tools": gray_acp::session::to_mcp_tools(&prep.tools),
        }))
    }

    fn drive(
        &mut self,
        prep: &mut PreparedTurn,
        fresh: bool,
        cancel: &AtomicBool,
    ) -> Result<TurnResult, String> {
        let was_parked = !self.pending.is_empty();
        let mut carried = Vec::new();
        for (call, output) in prep.outputs.drain(..) {
            if let Some(id) = self.pending.remove(&call) {
                self.worker
                    .send(&json!({"type": "output", "id": id, "output": output}))?;
            } else {
                carried.push((call, output));
            }
        }
        prep.outputs = carried;
        let prompt = self.prompt(prep, fresh);
        if was_parked {
            if let Some(prompt) = prompt {
                self.queued = Some(prompt);
            }
        } else {
            self.worker.send(&prompt.ok_or("empty Cursor turn")?)?;
        }

        let mut result = TurnResult::default();
        let start = Instant::now();
        let mut activity = start;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("Cursor turn cancelled by client".into());
            }
            if start.elapsed() > TURN_TIMEOUT {
                return Err("Cursor turn timed out".into());
            }
            if !self.pending.is_empty() && activity.elapsed() >= PARK_WINDOW {
                result.suspended = true;
                break;
            }
            let Some(frame) = self.worker.recv(Duration::from_millis(50))? else {
                continue;
            };
            activity = Instant::now();
            match frame["type"].as_str() {
                Some("ready") => {}
                Some("text" | "reasoning") => {
                    let Some(text) = frame["text"].as_str().filter(|s| !s.is_empty()) else {
                        continue;
                    };
                    let reasoning = frame["type"] == "reasoning";
                    match (reasoning, result.items.last_mut()) {
                        (false, Some(OutItem::Message { text: previous }))
                        | (true, Some(OutItem::Reasoning { text: previous })) => {
                            previous.push_str(text)
                        }
                        (false, _) => result.items.push(OutItem::Message { text: text.into() }),
                        (true, _) => result.items.push(OutItem::Reasoning { text: text.into() }),
                    }
                }
                Some("call") => {
                    let id = frame["id"]
                        .as_str()
                        .ok_or("Cursor SDK tool call has no id")?;
                    let name = frame["name"]
                        .as_str()
                        .ok_or("Cursor SDK tool call has no name")?;
                    let call_id = format!("call_{}", chat::rand_hex(24));
                    self.pending.insert(call_id.clone(), id.into());
                    result.items.push(OutItem::FunctionCall {
                        call_id,
                        name: name.into(),
                        arguments: frame["args"].to_string(),
                    });
                }
                Some("done") => {
                    if let Some(usage) = Usage::from_acp(&frame["usage"]) {
                        if let Some(all) = &mut result.usage {
                            all.merge_step(usage);
                        } else {
                            result.usage = Some(usage);
                        }
                    }
                    self.pending.clear();
                    if let Some(prompt) = self.queued.take() {
                        self.worker.send(&prompt)?;
                    } else {
                        result.stop_reason = "end_turn".into();
                        break;
                    }
                }
                _ => return Err("unexpected Cursor SDK turn response".into()),
            }
        }
        self.absorbed = std::mem::take(&mut prep.input);
        (self.reply_call_ids, self.reply_text) = chat::reply_signature(&result);
        self.touched = Instant::now();
        Ok(result)
    }
}

impl Pool {
    pub fn run(
        &self,
        root: &Path,
        key: &str,
        model: &str,
        raw: &[u8],
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>, String> {
        let mut body: Value =
            serde_json::from_slice(raw).map_err(|e| format!("bad request json: {e}"))?;
        if let Some(text) = body["input"].as_str() {
            body["input"] = json!([{"type": "message", "role": "user", "content": text}]);
        }
        let model = body["model"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(model);
        let mut prep = chat::prepare_turn(&body, model)?;
        let mut sessions = self.0.lock().map_err(|_| "Cursor SDK pool lock")?;
        let resumed = sessions.iter().enumerate().find_map(|(index, session)| {
            if !session.matches(&prep, key) {
                return None;
            }
            if !session.pending.is_empty() {
                let delta = prep.input.strip_prefix(session.absorbed.as_slice())?;
                return delta
                    .iter()
                    .any(|v| {
                        v["type"] == "function_call_output"
                            && v["call_id"]
                                .as_str()
                                .is_some_and(|id| session.pending.contains_key(id))
                    })
                    .then(|| {
                        (
                            index,
                            delta
                                .iter()
                                .filter(|v| !chat::assistant_side(v))
                                .cloned()
                                .collect(),
                        )
                    });
            }
            chat::continuation(
                &session.absorbed,
                &session.reply_call_ids,
                &session.reply_text,
                &prep.input,
            )
            .ok()
            .map(|(tail, _)| (index, tail.to_vec()))
        });
        let mut session = resumed
            .as_ref()
            .map(|(index, _)| sessions.swap_remove(*index));
        drop(sessions);
        let fresh = session.is_none();
        let mut session = match session.take() {
            Some(session) => session,
            None => Session::open(root, key, &prep)?,
        };
        prep.tail = resumed
            .map(|(_, tail)| tail)
            .unwrap_or_else(|| prep.input.clone());
        prep.tail
            .retain(|v| !matches!(v["role"].as_str(), Some("system" | "developer")));
        chat::split_tail_with(&mut prep, true);
        let result = session.drive(&mut prep, fresh, cancel)?;
        let mut sessions = self.0.lock().map_err(|_| "Cursor SDK pool lock")?;
        sessions.push(session);
        let evicted = (sessions.len() > MAX_SESSIONS).then(|| {
            let oldest = sessions
                .iter()
                .enumerate()
                .min_by_key(|(_, session)| (!session.pending.is_empty(), session.touched))
                .map(|(index, _)| index)
                .unwrap_or(0);
            sessions.swap_remove(oldest)
        });
        drop(sessions);
        drop(evicted);
        let response_id = format!("resp_{}", chat::rand_hex(24));
        Ok(chat::fold_result(&result, &prep.model, &response_id))
    }

    pub fn close(&self) {
        let sessions = std::mem::take(&mut *self.0.lock().unwrap());
        drop(sessions);
    }
}
