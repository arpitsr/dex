use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::types::ChatMessage;

const SESSION_VERSION: u32 = 1;

/// Serializes tests that redirect XDG_DATA_HOME (it decides where ALL
/// sessions live, including other tests' fixtures).
#[cfg(test)]
pub(crate) static TEST_SESSIONS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct SessionHeader {
    #[serde(rename = "type")]
    entry_type: String,
    version: u32,
    id: String,
    timestamp: String,
    cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

impl SessionHeader {
    pub(crate) fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn cwd(&self) -> &str {
        &self.cwd
    }
    pub(crate) fn timestamp(&self) -> &str {
        &self.timestamp
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct SessionMessageEntry {
    #[serde(rename = "type")]
    entry_type: String,
    id: String,
    timestamp: String,
    #[serde(flatten)]
    message: ChatMessage,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct SessionInfoEntry {
    #[serde(rename = "type")]
    entry_type: String,
    id: String,
    timestamp: String,
    name: String,
}

#[derive(Serialize)]
struct SessionClearEntry {
    #[serde(rename = "type")]
    entry_type: String,
    id: String,
    timestamp: String,
}

#[derive(Serialize)]
struct SessionEventEntry {
    #[serde(rename = "type")]
    entry_type: String,
    id: String,
    timestamp: String,
}

/// Durable record of one side effect: intent (before execution) and outcome
/// (after). Written around every tool execution so a restart can reconcile
/// what happened vs. what completed (P8 journal).
#[derive(Serialize)]
struct SessionEffectEntry {
    #[serde(rename = "type")]
    entry_type: String,
    id: String,
    timestamp: String,
    tool_call_id: String,
    name: String,
    input_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<bool>,
}

/// One entry in the per-session change ledger (`session_state "changes"`).
/// `before`/`after` hold file content (capped) so `/undo` can restore the
/// previous state; hashes always recorded even when content was too big.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct ChangeRecord {
    pub path: String,
    pub tool: String,
    pub before_hash: String,
    pub after_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    pub timestamp: String,
}

/// Cap for content stored in a change record; larger files record hashes
/// only (undo unavailable for them).
const CHANGE_CONTENT_CAP: usize = 64 * 1024;
/// Keep at most this many change records per session (FIFO).
const CHANGE_RECORD_CAP: usize = 50;

#[derive(Serialize)]
struct SessionStateEntry {
    #[serde(rename = "type")]
    entry_type: String,
    id: String,
    timestamp: String,
    key: String,
    value: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Session {
    header: SessionHeader,
    path: Option<PathBuf>,
    counter: u64,
}

impl Session {
    fn session_dir() -> PathBuf {
        if let Some(dir) = env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(dir).join("dex/sessions");
        }
        env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".local/share/dex/sessions"))
            .unwrap_or_else(|| PathBuf::from(".dex/sessions"))
    }

    fn cwd_slug(cwd: &str) -> String {
        let mut hash = 2166136261u64;
        for byte in cwd.as_bytes() {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(16777619);
        }
        format!("{}-{:016x}", cwd.replace(['/', '\\'], "-"), hash)
    }

    pub(crate) fn new(cwd: String, name: Option<String>) -> io::Result<Self> {
        let id = format!("{}_{}", Self::now_ms(), uuid4());
        let dir = Self::session_dir().join(Self::cwd_slug(&cwd));
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.jsonl", id));
        let header = SessionHeader {
            entry_type: "session".to_string(),
            version: SESSION_VERSION,
            id: id.clone(),
            timestamp: Self::now_iso(),
            cwd,
            name,
        };
        let line = serde_json::to_string(&header).map_err(io::Error::other)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", line)?;
        Ok(Self {
            header,
            path: Some(path),
            counter: 0,
        })
    }

    pub(crate) fn from_path(path: &Path) -> io::Result<Self> {
        let file = fs::read_to_string(path)?;
        let mut lines = file.lines();
        let first = lines
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty session file"))?;
        let header: SessionHeader = serde_json::from_str(first).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad session header: {}", e),
            )
        })?;
        if header.entry_type != "session" || header.id.is_empty() || header.cwd.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid session metadata",
            ));
        }
        if header.version != SESSION_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported session version {}", header.version),
            ));
        }
        let counter = lines.count() as u64;
        Ok(Self {
            header,
            path: Some(path.to_path_buf()),
            counter,
        })
    }

    pub(crate) fn in_memory(cwd: String) -> Self {
        Self {
            header: SessionHeader {
                entry_type: "session".to_string(),
                version: SESSION_VERSION,
                id: format!("{}_{}", Self::now_ms(), uuid4()),
                timestamp: Self::now_iso(),
                cwd,
                name: None,
            },
            path: None,
            counter: 0,
        }
    }

    pub(crate) fn open_or_continue(
        cwd: String,
        session_path: Option<&Path>,
        no_session: bool,
    ) -> io::Result<Self> {
        if no_session {
            return Ok(Self::in_memory(cwd));
        }
        if let Some(path) = session_path {
            if path.exists() {
                return Self::from_path(path);
            }
        }
        Self::new(cwd, None)
    }

    pub(crate) fn list(cwd: &str) -> io::Result<Vec<(PathBuf, SessionHeader)>> {
        let dir = Self::session_dir().join(Self::cwd_slug(cwd));
        let mut sessions = Vec::new();
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    if let Ok(text) = fs::read_to_string(&path) {
                        if let Some(first) = text.lines().next() {
                            if let Ok(header) = serde_json::from_str::<SessionHeader>(first) {
                                sessions.push((path, header));
                            }
                        }
                    }
                }
            }
        }
        sessions.sort_by(|a, b| b.1.timestamp.cmp(&a.1.timestamp));
        Ok(sessions)
    }

    /// List every persisted session across all workspaces (registry rebuild
    /// and disk-backed `GET /api/sessions`).
    pub(crate) fn list_all() -> io::Result<Vec<(PathBuf, SessionHeader)>> {
        let base = Self::session_dir();
        let mut sessions = Vec::new();
        if let Ok(entries) = fs::read_dir(&base) {
            for entry in entries.flatten() {
                let dir = entry.path();
                if !dir.is_dir() {
                    continue;
                }
                if let Ok(files) = fs::read_dir(&dir) {
                    for file in files.flatten() {
                        let path = file.path();
                        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                            if let Ok(text) = fs::read_to_string(&path) {
                                if let Some(first) = text.lines().next() {
                                    if let Ok(header) = serde_json::from_str::<SessionHeader>(first)
                                    {
                                        sessions.push((path, header));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        sessions.sort_by(|a, b| b.1.timestamp.cmp(&a.1.timestamp));
        Ok(sessions)
    }

    pub(crate) fn resume(cwd: &str, selector: &str) -> io::Result<Self> {
        let sessions = Self::list(cwd)?;
        let path = if let Ok(index) = selector.parse::<usize>() {
            sessions.get(index).map(|(path, _)| path.clone())
        } else {
            let candidate = PathBuf::from(selector);
            sessions
                .iter()
                .find(|(path, _)| {
                    path == &candidate
                        || path.file_name().and_then(|n| n.to_str()) == Some(selector)
                })
                .map(|(path, _)| path.clone())
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "session not found"))?;
        Self::from_path(&path)
    }

    pub(crate) fn set_name(&mut self, name: String) -> io::Result<()> {
        self.header.name = Some(name.clone());
        let entry = SessionInfoEntry {
            entry_type: "session_info".to_string(),
            id: self.next_id(),
            timestamp: Self::now_iso(),
            name,
        };
        self.append_line(&entry)
    }

    pub(crate) fn append_message(&mut self, message: ChatMessage) -> io::Result<()> {
        let entry = SessionMessageEntry {
            entry_type: "message".to_string(),
            id: self.next_id(),
            timestamp: Self::now_iso(),
            message,
        };
        self.append_line(&entry)
    }

    pub(crate) fn clear_messages(&mut self) -> io::Result<()> {
        let entry = SessionClearEntry {
            entry_type: "clear".into(),
            id: self.next_id(),
            timestamp: Self::now_iso(),
        };
        self.append_line(&entry)
    }

    pub(crate) fn turn_event(&mut self, event: &str) -> io::Result<()> {
        let entry = SessionEventEntry {
            entry_type: event.to_string(),
            id: self.next_id(),
            timestamp: Self::now_iso(),
        };
        self.append_line(&entry)
    }

    /// Durable side-effect intent: recorded BEFORE the tool executes so a
    /// restart can see effects that started but never completed.
    pub(crate) fn effect_start(
        &mut self,
        tool_call_id: &str,
        name: &str,
        input_hash: &str,
    ) -> io::Result<()> {
        let entry = SessionEffectEntry {
            entry_type: "effect_start".into(),
            id: self.next_id(),
            timestamp: Self::now_iso(),
            tool_call_id: tool_call_id.into(),
            name: name.into(),
            input_hash: input_hash.into(),
            ok: None,
        };
        self.append_line(&entry)
    }

    /// Durable side-effect outcome: recorded AFTER the tool executed.
    pub(crate) fn effect_result(&mut self, tool_call_id: &str, ok: bool) -> io::Result<()> {
        let entry = SessionEffectEntry {
            entry_type: "effect_result".into(),
            id: self.next_id(),
            timestamp: Self::now_iso(),
            tool_call_id: tool_call_id.into(),
            name: String::new(),
            input_hash: String::new(),
            ok: Some(ok),
        };
        self.append_line(&entry)
    }

    pub(crate) fn set_state(&mut self, key: &str, value: &str) -> io::Result<()> {
        let entry = SessionStateEntry {
            entry_type: "session_state".into(),
            id: self.next_id(),
            timestamp: Self::now_iso(),
            key: key.into(),
            value: value.into(),
        };
        self.append_line(&entry)
    }

    fn append_line<T: Serialize>(&mut self, entry: &T) -> io::Result<()> {
        if let Some(path) = &self.path {
            let line = serde_json::to_string(entry).map_err(io::Error::other)?;
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(file, "{}", line)?;
            // Durability: flush to disk before reporting success so a crash
            // cannot lose the last record while in-memory state believes it
            // was persisted (P8 journal).
            file.sync_data()?;
        }
        Ok(())
    }

    fn next_id(&mut self) -> String {
        self.counter += 1;
        format!("{:x}", self.counter)
    }
    fn now_iso() -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        chrono::DateTime::from_timestamp(secs as i64, 0)
            .unwrap_or_default()
            .to_rfc3339()
    }
    fn now_ms() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    }
    pub(crate) fn id(&self) -> &str {
        &self.header.id
    }
    pub(crate) fn name(&self) -> Option<&str> {
        self.header.name.as_deref()
    }
    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub(crate) fn count(&self) -> u64 {
        self.counter
    }
    pub(crate) fn display_name(&self) -> String {
        self.name().unwrap_or(self.id()).to_string()
    }

    /// Path of the per-session SSE event journal (`<id>.events.jsonl`).
    pub(crate) fn events_path(&self) -> Option<PathBuf> {
        self.path.as_ref().map(|p| p.with_extension("events.jsonl"))
    }

    /// Append one numbered stream event to the event journal. `seq` is
    /// assigned by the daemon (monotonic per session, seeded from disk on
    /// restart); the journal is what `/api/sessions/{id}/events?since=` replays.
    pub(crate) fn append_event(&mut self, seq: u64, payload: &str) -> io::Result<()> {
        let Some(path) = self.events_path() else {
            return Ok(());
        };
        let payload: Value = serde_json::from_str(payload).unwrap_or(Value::String(payload.into()));
        let line = serde_json::json!({"seq": seq, "ts": Self::now_iso(), "payload": payload});
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{}", line)?;
        file.sync_data()?;
        Ok(())
    }

    /// Replay stream events with `seq > since`, in order. `path` is the
    /// SESSION file; the journal lives at `<session>.events.jsonl`.
    pub(crate) fn load_events(path: &Path, since: u64) -> io::Result<Vec<(u64, String)>> {
        let events_path = path.with_extension("events.jsonl");
        let text = fs::read_to_string(&events_path)?;
        let mut out = Vec::new();
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(seq) = value.get("seq").and_then(Value::as_u64) else {
                continue;
            };
            if seq <= since {
                continue;
            }
            if let Some(payload) = value.get("payload") {
                out.push((seq, payload.to_string()));
            }
        }
        Ok(out)
    }

    /// Highest event seq recorded for a session (0 when none).
    pub(crate) fn max_event_seq(path: &Path) -> u64 {
        let events_path = path.with_extension("events.jsonl");
        let Ok(text) = fs::read_to_string(&events_path) else {
            return 0;
        };
        let mut max = 0;
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let Some(seq) = v.get("seq").and_then(Value::as_u64) {
                    max = max.max(seq);
                }
            }
        }
        max
    }

    /// Terminal state of the most recent turn: "complete", "failed", or
    /// "interrupted" when a `turn_start` has no terminal entry after it.
    pub(crate) fn last_turn_state(path: &Path) -> &'static str {
        let Ok(text) = fs::read_to_string(path) else {
            return "unknown";
        };
        let mut state = "none";
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match value.get("type").and_then(Value::as_str) {
                Some("turn_start") => state = "interrupted",
                Some("turn_complete") => state = "complete",
                Some("turn_failed") => state = "failed",
                _ => {}
            }
        }
        state
    }
}

fn uuid4() -> String {
    let mut bytes = [0u8; 16];
    for b in bytes.iter_mut() {
        *b = rand::random();
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!("{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15])
}

pub(crate) fn load_messages_from_session(path: &Path) -> io::Result<Vec<ChatMessage>> {
    let text = fs::read_to_string(path)?;
    let mut messages = Vec::new();
    for (i, line) in text.lines().enumerate().skip(1) {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "[session] skipping bad line {} in {}: {}",
                    i + 1,
                    path.display(),
                    e
                );
                continue;
            }
        };
        if value.get("type").and_then(Value::as_str) == Some("clear") {
            messages.clear();
        } else if value.get("type").and_then(Value::as_str) == Some("message") {
            match serde_json::from_value::<ChatMessage>(value) {
                Ok(msg) if msg.role != "system" => messages.push(msg),
                Ok(_) => {}
                Err(e) => eprintln!(
                    "[session] skipping unparseable message at line {} in {}: {}",
                    i + 1,
                    path.display(),
                    e
                ),
            }
        }
    }
    Ok(messages)
}

pub(crate) fn load_plan(path: &Path) -> crate::core::types::Plan {
    load_session_state(path)
        .ok()
        .and_then(|m| m.get("plan").cloned())
        .map(|s| crate::core::types::Plan::from_json(&s))
        .unwrap_or_default()
}

#[allow(dead_code)]
pub(crate) fn save_plan(session: &mut Session, plan: &crate::core::types::Plan) -> io::Result<()> {
    session.set_state("plan", &plan.to_json())
}

pub(crate) fn load_session_state(
    path: &Path,
) -> io::Result<std::collections::HashMap<String, String>> {
    let text = fs::read_to_string(path)?;
    let mut state = std::collections::HashMap::new();
    for line in text.lines().skip(1) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("session_state") {
            if let (Some(key), Some(val)) = (
                value.get("key").and_then(Value::as_str),
                value.get("value").and_then(Value::as_str),
            ) {
                state.insert(key.into(), val.into());
            }
        }
    }
    Ok(state)
}

/// Load the change ledger (FIFO, newest last).
pub(crate) fn load_changes(path: &Path) -> Vec<ChangeRecord> {
    load_session_state(path)
        .ok()
        .and_then(|m| m.get("changes").cloned())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Replace the change ledger.
pub(crate) fn save_changes(session: &mut Session, changes: &[ChangeRecord]) -> io::Result<()> {
    let json = serde_json::to_string(changes).map_err(io::Error::other)?;
    session.set_state("changes", &json)
}

/// Record one change (FIFO-capped). Returns the updated ledger.
pub(crate) fn record_change(
    session: &mut Session,
    record: ChangeRecord,
) -> io::Result<Vec<ChangeRecord>> {
    let mut changes = session.path().map(load_changes).unwrap_or_default();
    changes.push(record);
    while changes.len() > CHANGE_RECORD_CAP {
        changes.remove(0);
    }
    save_changes(session, &changes)?;
    Ok(changes)
}

/// Undo the most recent change: the target file must still match
/// `after_hash` (no concurrent edit since), otherwise refuse. Returns a
/// human-readable summary for the caller to surface.
pub(crate) fn undo_last_change(session: &mut Session) -> io::Result<String> {
    let Some(path) = session.path().map(|p| p.to_path_buf()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session is not persisted",
        ));
    };
    let mut changes = load_changes(&path);
    let Some(record) = changes.pop() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no changes to undo",
        ));
    };
    if crate::tools::hash_file(&record.path) != record.after_hash {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} was modified since the change; refusing to undo",
                record.path
            ),
        ));
    }
    let Some(before) = &record.before else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is too large for undo", record.path),
        ));
    };
    fs::write(&record.path, before).map_err(io::Error::other)?;
    save_changes(session, &changes)?;
    Ok(format!(
        "undid {} on {} ({})",
        record.tool, record.path, record.timestamp
    ))
}

/// Build a change record from a completed write/edit.
pub(crate) fn make_change_record(
    tool: &str,
    path: &str,
    before: Option<&str>,
    after: Option<&str>,
    before_hash: &str,
    after_hash: &str,
) -> ChangeRecord {
    let cap = |s: &str| (s.len() <= CHANGE_CONTENT_CAP).then(|| s.to_string());
    ChangeRecord {
        path: path.to_string(),
        tool: tool.to_string(),
        before_hash: before_hash.to_string(),
        after_hash: after_hash.to_string(),
        before: before.and_then(cap),
        after: after.and_then(cap),
        timestamp: chrono::Utc::now().to_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn unique_path(prefix: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let mut h = DefaultHasher::new();
        std::thread::current().id().hash(&mut h);
        let tid = h.finish();
        let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "{}-{}-{}-{}-{}.jsonl",
            prefix,
            std::process::id(),
            tid,
            nanos,
            nonce
        ))
    }

    #[test]
    fn clear_marker_removes_messages_during_recovery() {
        let path = unique_path("dex-session-test");
        let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
        let message = r#"{"type":"message","id":"1","timestamp":"2020-01-01T00:00:00Z","role":"user","content":"old"}"#;
        let clear = r#"{"type":"clear","id":"2","timestamp":"2020-01-01T00:00:00Z"}"#;
        fs::write(&path, format!("{}\n{}\n{}\n", header, message, clear)).unwrap();
        assert!(load_messages_from_session(&path).unwrap().is_empty());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn session_state_last_write_wins_and_ignores_other_entries() {
        let path = unique_path("dex-session-state");
        let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
        let first = r#"{"type":"session_state","id":"1","timestamp":"2020-01-01T00:00:01Z","key":"model","value":"old-model"}"#;
        let message = r#"{"type":"message","id":"2","timestamp":"2020-01-01T00:00:02Z","role":"user","content":"hi"}"#;
        let second = r#"{"type":"session_state","id":"3","timestamp":"2020-01-01T00:00:03Z","key":"model","value":"new-model"}"#;
        let provider = r#"{"type":"session_state","id":"4","timestamp":"2020-01-01T00:00:04Z","key":"provider","value":"openai-codex"}"#;
        fs::write(
            &path,
            format!(
                "{}\n{}\n{}\n{}\n{}\n",
                header, first, message, second, provider
            ),
        )
        .unwrap();
        let state = load_session_state(&path).unwrap();
        assert_eq!(state.get("model").map(String::as_str), Some("new-model"));
        assert_eq!(
            state.get("provider").map(String::as_str),
            Some("openai-codex")
        );
        assert_eq!(state.len(), 2);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn session_state_missing_file_is_an_error() {
        let path = unique_path("dex-session-state-missing");
        let _ = fs::remove_file(&path);
        assert!(load_session_state(&path).is_err());
    }

    #[test]
    fn events_journal_replays_after_seq_cursor() {
        let mut s = Session::new("/tmp/dex-events-test".into(), None).unwrap();
        s.append_event(0, r#"{"type":"assistant_text","data":"a"}"#)
            .unwrap();
        s.append_event(1, r#"{"type":"assistant_text","data":"b"}"#)
            .unwrap();
        s.append_event(2, r#"{"type":"turn_complete","data":{"response":"done"}}"#)
            .unwrap();
        s.append_event(3, r#"{"type":"assistant_text","data":"c"}"#)
            .unwrap();
        let path = s.path().unwrap().to_path_buf();
        let after = Session::load_events(&path, 1).unwrap();
        let seqs: Vec<u64> = after.iter().map(|(seq, _)| *seq).collect();
        let texts: Vec<String> = after
            .iter()
            .map(|(_, p)| serde_json::from_str::<Value>(p).unwrap()["data"].to_string())
            .collect();
        assert_eq!(seqs, vec![2, 3]);
        assert_eq!(texts.len(), 2);
        assert_eq!(Session::max_event_seq(&path), 3);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("events.jsonl"));
    }

    #[test]
    fn last_turn_state_tracks_terminal_entries() {
        let path = unique_path("dex-turn-state");
        let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
        let start = r#"{"type":"turn_start","id":"1","timestamp":"2020-01-01T00:00:00Z"}"#;
        fs::write(&path, format!("{}\n{}\n", header, start)).unwrap();
        assert_eq!(Session::last_turn_state(&path), "interrupted");
        // Append a terminal entry and it flips.
        let done = r#"{"type":"turn_complete","id":"2","timestamp":"2020-01-01T00:00:00Z"}"#;
        fs::write(&path, format!("{}\n{}\n{}\n", header, start, done)).unwrap();
        assert_eq!(Session::last_turn_state(&path), "complete");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn change_ledger_records_then_undo_restores() {
        let mut s = Session::new("/tmp/dex-undo-test".into(), None).unwrap();
        let work = s.path().unwrap().parent().unwrap().join("work.txt");
        fs::write(&work, b"before\n").unwrap();
        let h_before = crate::tools::hash_file(&work.display().to_string());
        fs::write(&work, b"after\n").unwrap();
        let h_after = crate::tools::hash_file(&work.display().to_string());
        record_change(
            &mut s,
            make_change_record(
                "write",
                &work.display().to_string(),
                Some("before\n"),
                Some("after\n"),
                &h_before,
                &h_after,
            ),
        )
        .unwrap();
        // File is currently "after" — matches after_hash, so undo applies.
        let changes = load_changes(s.path().unwrap());
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].after_hash, h_after);
        let msg = undo_last_change(&mut s).unwrap();
        assert!(msg.contains("undid write"));
        assert_eq!(fs::read_to_string(&work).unwrap(), "before\n");
        assert!(load_changes(s.path().unwrap()).is_empty());
        let _ = fs::remove_file(&work);
        if let Some(p) = s.path() {
            let _ = fs::remove_file(p);
        }
    }

    #[test]
    fn undo_refuses_when_file_moved_on() {
        let mut s = Session::new("/tmp/dex-undo-concurrent".into(), None).unwrap();
        let work = s.path().unwrap().parent().unwrap().join("c.txt");
        fs::write(&work, b"v1\n").unwrap();
        let h = crate::tools::hash_file(&work.display().to_string());
        record_change(
            &mut s,
            make_change_record(
                "write",
                &work.display().to_string(),
                Some("v1\n"),
                Some("v2\n"),
                &h,
                &h,
            ),
        )
        .unwrap();
        // Rewrite the file afterwards but keep the same hash (hash is of
        // content; simulate a concurrent edit changing it):
        // A concurrent edit changes the content -> new hash -> refuse.
        fs::write(&work, b"vX\n").unwrap();
        let err = undo_last_change(&mut s).unwrap_err();
        assert!(err.to_string().contains("refusing to undo"));
        let _ = fs::remove_file(&work);
        if let Some(p) = s.path() {
            let _ = fs::remove_file(p);
        }
    }

    #[test]
    fn effect_journal_records_intent_and_outcome() {
        let mut s = Session::new("/tmp/dex-effect-test".into(), None).unwrap();
        s.effect_start("call-1", "edit", "abc").unwrap();
        s.turn_event("turn_start").unwrap();
        s.effect_result("call-1", true).unwrap();
        s.turn_event("turn_complete").unwrap();
        let text = fs::read_to_string(s.path().unwrap()).unwrap();
        assert!(text.contains("effect_start"));
        assert!(text.contains("call-1"));
        assert!(text.contains("effect_result"));
        assert!(text.contains("\"ok\":true"));
        assert!(Session::last_turn_state(s.path().unwrap()) == "complete");
        if let Some(p) = s.path() {
            let _ = fs::remove_file(p);
        }
    }
}
