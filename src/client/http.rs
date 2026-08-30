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
}

/// HTTP client for communicating with the oye daemon.
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
            .send()?
            .error_for_status()?
            .json::<DaemonInfo>()?;
        Ok(info)
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
            .json(&CreateSessionRequest {
                cwd: cwd.to_string(),
                name: name.map(String::from),
            })
            .send()?
            .error_for_status()?
            .json::<CreateSessionResponse>()?;
        Ok(resp)
    }

    /// List all sessions on the daemon.
    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, Box<dyn std::error::Error>> {
        let resp: serde_json::Value = self
            .http
            .get(format!("{}/api/sessions", self.base_url))
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

        let response = self
            .http
            .post(&url)
            .json(&ChatRequest {
                prompt: prompt.to_string(),
                skill_dirs: options.skill_dirs,
                base_url: options.base_url,
                model: options.model,
                permission: options.permission,
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

            let Ok(event) = serde_json::from_str::<StreamEvent>(data) else {
                continue;
            };

            if let StreamEvent::ApprovalRequired { ref request_id, .. } = event {
                // The callback decides (it may block waiting for the user);
                // a `None` default denies the tool.
                let decision = on_event(event.clone()).unwrap_or(ApprovalDecision::Deny);
                if let Err(e) = self.approve(session_id, request_id, decision) {
                    // Surface but do not kill the stream: the daemon denies
                    // pending approvals on turn teardown anyway.
                    eprintln!("[approval] failed to deliver decision: {e}");
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
            .send()?
            .error_for_status()?;
        Ok(())
    }
}
