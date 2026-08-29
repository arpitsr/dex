use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::types::ChatMessage;

const SESSION_VERSION: u32 = 1;

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
            return PathBuf::from(dir).join("ak/sessions");
        }
        env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".local/share/ak/sessions"))
            .unwrap_or_else(|| PathBuf::from(".ak/sessions"))
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

#[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clear_marker_removes_messages_during_recovery() {
        let path =
            std::env::temp_dir().join(format!("ak-session-test-{}.jsonl", std::process::id()));
        let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
        let message = r#"{"type":"message","id":"1","timestamp":"2020-01-01T00:00:00Z","role":"user","content":"old"}"#;
        let clear = r#"{"type":"clear","id":"2","timestamp":"2020-01-01T00:00:00Z"}"#;
        fs::write(&path, format!("{}\n{}\n{}\n", header, message, clear)).unwrap();
        assert!(load_messages_from_session(&path).unwrap().is_empty());
        let _ = fs::remove_file(path);
    }
}
