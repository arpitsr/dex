use std::io::{self, BufRead};
use std::time::{Duration, Instant};

use crate::protocol::*;

/// Per-request overrides forwarded to the daemon with a chat turn.
#[derive(Debug, Clone, Default)]
pub(crate) struct ChatOptions {
    pub(crate) skill_dirs: Vec<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) permission: Option<String>,
    pub(crate) plan: Option<String>,
    /// P10: replay-safe submission key; the daemon dedups identical keys within 60s.
    pub(crate) idempotency_key: Option<String>,
}

/// HTTP client for communicating with the dex daemon.
///
/// The blocking reqwest client is deliberate: the TUI runs a dedicated worker
/// thread per turn that consumes the SSE stream, so no async runtime is
/// needed on the client side. Cheap to clone for worker threads.
#[derive(Clone)]
pub(crate) struct DaemonClient {
    base_url: String,
    http: reqwest::blocking::Client,
}

impl DaemonClient {
    pub fn new(base_url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let base_url = base_url.trim_end_matches('/').to_string();
        Ok(Self {
            base_url,
            http: reqwest::blocking::Client::new(),
        })
    }

    /// Wait until `GET /health` answers (the daemon may still be booting).
    /// Returns an error if it never becomes ready within `timeout`.
    pub fn wait_until_ready(&self, timeout: Duration) -> Result<(), Box<dyn std::error::Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            match self
                .http
                .get(format!("{}/health", self.base_url))
                .timeout(Duration::from_secs(2))
                .send()
            {
                Ok(resp) if resp.status().is_success() => return Ok(()),
                _ if Instant::now() >= deadline => {
                    return Err(format!("daemon at {} did not become ready", self.base_url).into())
                }
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    /// Fetch the daemon's runtime info (model, provider, workspace, git).
    pub fn get_config(&self) -> Result<DaemonInfo, Box<dyn std::error::Error>> {
        let info = self
            .http
            .get(format!("{}/api/config", self.base_url))
            .headers(self.api_headers())
            .send()?
            .error_for_status()?
            .json::<DaemonInfo>()?;
        Ok(info)
    }

    /// Versioned-protocol headers (P10): declared on every `/api/*` request
    /// so the daemon can reject a mismatch. Health checks stay header-free.
    fn api_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Ok(value) = reqwest::header::HeaderValue::from_str("application/vnd.dex.v1+json") {
            headers.insert(reqwest::header::ACCEPT, value);
        }
        if let Ok(value) = reqwest::header::HeaderValue::from_str("1") {
            headers.insert("x-dex-protocol", value);
        }
        headers
    }

    /// Create a new session on the daemon.
    pub fn create_session(
        &self,
        cwd: &str,
        name: Option<&str>,
    ) -> Result<CreateSessionResponse, Box<dyn std::error::Error>> {
        let resp = self
            .http
            .post(format!("{}/api/sessions", self.base_url))
            .headers(self.api_headers())
            .json(&CreateSessionRequest {
                cwd: cwd.to_string(),
                name: name.map(String::from),
            })
            .send()?
            .error_for_status()?
            .json::<CreateSessionResponse>()?;
        Ok(resp)
    }

    /// List all sessions on the daemon (P10: disk-backed, survives restarts).
    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, Box<dyn std::error::Error>> {
        let resp: serde_json::Value = self
            .http
            .get(format!("{}/api/sessions", self.base_url))
            .headers(self.api_headers())
            .send()?
            .error_for_status()?
            .json()?;

        let sessions = resp["sessions"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();

        Ok(sessions)
    }

    /// Submit a chat prompt and process `StreamEvent`s as they arrive.
    ///
    /// `on_event` is called synchronously for every event in stream order.
    /// When an `ApprovalRequired` event is received, its return value is the
    /// user's decision; a `None` default denies the tool. Returning a
    /// decision blocks this call until the daemon confirms, which is exactly
    /// what callers want: the agent thread on the daemon is parked until the
    /// approval is resolved.
    ///
    /// Returns only after the stream closes (turn complete/failed/disconnect).
    pub fn chat(
        &self,
        session_id: &str,
        prompt: &str,
        options: ChatOptions,
        on_event: &mut dyn FnMut(StreamEvent) -> Option<ApprovalDecision>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let url = format!("{}/api/sessions/{}/chat", self.base_url, session_id);

        let mut builder = self.http.post(&url).headers(self.api_headers());
        if let Some(key) = &options.idempotency_key {
            builder = builder.header("idempotency-key", key);
        }
        let response = builder
            .json(&ChatRequest {
                prompt: prompt.to_string(),
                skill_dirs: options.skill_dirs,
                base_url: options.base_url,
                model: options.model,
                permission: options.permission,
                plan: options.plan,
            })
            .send()?
            .error_for_status()?;

        let mut reader = io::BufReader::new(response);
        let mut line = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF: stream closed
                Ok(_) => {}
                Err(e) => return Err(e.into()),
            }

            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                continue;
            }

            let Some(data) = trimmed.strip_prefix("data:") else {
                continue; // keep-alive comments etc.
            };
            let data = data.trim();
            if data.is_empty() || data == "ping" {
                continue;
            }

            // Events are numbered envelopes on the versioned path (P10).
            let event = match serde_json::from_str::<StreamEnvelope>(data) {
                Ok(env) => env.event,
                Err(_) => continue, // unparsable envelope; skip
            };

            if let StreamEvent::ApprovalRequired { ref request_id, .. } = &event {
                // Extract request_id before moving event into the callback to
                // avoid cloning the whole event.
                let request_id = request_id.clone();
                let decision = on_event(event).unwrap_or(ApprovalDecision::Deny);
                if let Err(e) = self.approve(session_id, &request_id, decision) {
                    // Surface but do not kill the stream: the daemon denies
                    // pending approvals on turn teardown anyway. Log, don't
                    // eprintln — a raw write here paints over the TUI's
                    // alternate screen and lingers until a resize repaint.
                    crate::llm::client::provider_log("approval_delivery_failed", &e.to_string());
                }
                continue;
            }

            on_event(event);
        }

        Ok(())
    }

    /// Send an approval decision for a pending tool execution.
    pub fn approve(
        &self,
        session_id: &str,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.http
            .post(format!(
                "{}/api/sessions/{}/approve",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .json(&ApprovalResponse {
                request_id: request_id.to_string(),
                decision,
            })
            .send()?
            .error_for_status()?;
        Ok(())
    }

    /// Cancel the active turn for a session.
    pub fn cancel(&self, session_id: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.http
            .post(format!(
                "{}/api/sessions/{}/cancel",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .send()?
            .error_for_status()?;
        Ok(())
    }

    /// List skills discovered on the daemon (its workspace).
    pub fn list_skills(&self) -> Result<Vec<SkillInfo>, Box<dyn std::error::Error>> {
        let resp: serde_json::Value = self
            .http
            .get(format!("{}/api/skills", self.base_url))
            .headers(self.api_headers())
            .send()?
            .error_for_status()?
            .json()?;
        let skills = resp["skills"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        Ok(skills)
    }

    /// Load a skill by name into the daemon's session history.
    pub fn load_skill(
        &self,
        session_id: &str,
        name: &str,
        skill_dirs: &[String],
    ) -> Result<LoadSkillResponse, Box<dyn std::error::Error>> {
        let resp = self
            .http
            .post(format!(
                "{}/api/sessions/{}/skill",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .json(&LoadSkillRequest {
                name: name.to_string(),
                skill_dirs: skill_dirs.to_vec(),
            })
            .send()?
            .error_for_status()?
            .json::<LoadSkillResponse>()?;
        Ok(resp)
    }

    /// Re-register a persisted session and get the replay cursor (P10).
    pub fn reattach(
        &self,
        session_id: &str,
    ) -> Result<ReattachResponse, Box<dyn std::error::Error>> {
        let resp = self
            .http
            .post(format!(
                "{}/api/sessions/{}/reattach",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .send()?
            .error_for_status()?
            .json::<ReattachResponse>()?;
        Ok(resp)
    }

    /// Replay journaled stream events after `since` (P10).
    pub fn events(
        &self,
        session_id: &str,
        since: u64,
    ) -> Result<EventsResponse, Box<dyn std::error::Error>> {
        let resp = self
            .http
            .get(format!(
                "{}/api/sessions/{}/events?since={}",
                self.base_url, session_id, since
            ))
            .headers(self.api_headers())
            .send()?
            .error_for_status()?
            .json::<EventsResponse>()?;
        Ok(resp)
    }

    /// Fetch the redacted trace rows for a session (P9).
    pub fn trace(
        &self,
        session_id: &str,
    ) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
        let resp: serde_json::Value = self
            .http
            .get(format!(
                "{}/api/sessions/{}/trace",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .send()?
            .error_for_status()?
            .json()?;
        Ok(resp["trace"].as_array().cloned().unwrap_or_default())
    }

    /// Undo the last recorded change; Ok(true) when an undo happened.
    pub fn undo(&self, session_id: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let resp = self
            .http
            .post(format!(
                "{}/api/sessions/{}/undo",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .send()?
            .error_for_status()?
            .json::<serde_json::Value>()?;
        Ok(resp["status"].as_str() == Some("ok"))
    }

    /// Record a `waived` verification disposition with a reason (P9).
    pub fn waive(&self, session_id: &str, reason: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.http
            .post(format!(
                "{}/api/sessions/{}/waive",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .json(&serde_json::json!({ "reason": reason }))
            .send()?
            .error_for_status()?;
        Ok(())
    }

    /// Rename a session on the daemon.
    pub fn rename_session(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.http
            .post(format!(
                "{}/api/sessions/{}/name",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .json(&serde_json::json!({ "name": name }))
            .send()?
            .error_for_status()?;
        Ok(())
    }
}
