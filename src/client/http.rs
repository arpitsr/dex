use std::io::{self, BufRead};

use crate::protocol::*;

/// HTTP client for communicating with the ak daemon.
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
            .json::<CreateSessionResponse>()?;
        Ok(resp)
    }

    /// List all sessions on the daemon.
    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, Box<dyn std::error::Error>> {
        let resp: serde_json::Value = self
            .http
            .get(format!("{}/api/sessions", self.base_url))
            .send()?
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

    /// Submit a chat prompt and process events as they arrive.
    ///
    /// When an `ApprovalRequired` event is received, `on_approval` is called
    /// with the request details. The callback should return the user's decision.
    /// This allows interactive approval during a streaming turn.
    pub fn chat_with_approval(
        &self,
        session_id: &str,
        prompt: &str,
        mut on_approval: impl FnMut(&str, &str) -> ApprovalDecision,
    ) -> Result<Vec<StreamEvent>, Box<dyn std::error::Error>> {
        let url = format!("{}/api/sessions/{}/chat", self.base_url, session_id);

        let response = self
            .http
            .post(&url)
            .json(&ChatRequest {
                prompt: prompt.to_string(),
                skill_dirs: vec![],
            })
            .send()?;

        let mut events = Vec::new();
        let mut reader = io::BufReader::new(response);
        let mut line = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(_) => {}
                Err(e) => return Err(e.into()),
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let data = if let Some(rest) = trimmed.strip_prefix("data: ") {
                rest
            } else {
                continue;
            };

            if data == "ping" {
                continue;
            }

            let event: StreamEvent = match serde_json::from_str(data) {
                Ok(e) => e,
                Err(_) => continue,
            };

            // Handle approval requests interactively.
            if let StreamEvent::ApprovalRequired {
                request_id: _,
                ref name,
                ref input,
            } = event
            {
                let decision = on_approval(name, input);
                self.approve(session_id, decision)?;
            }

            events.push(event);
        }

        Ok(events)
    }

    /// Send an approval decision for a pending tool execution.
    pub fn approve(
        &self,
        session_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.http
            .post(format!(
                "{}/api/sessions/{}/approve",
                self.base_url, session_id
            ))
            .json(&ApprovalResponse { decision })
            .send()?;
        Ok(())
    }

    /// Cancel the active turn for a session.
    pub fn cancel(&self, session_id: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.http
            .post(format!(
                "{}/api/sessions/{}/cancel",
                self.base_url, session_id
            ))
            .send()?;
        Ok(())
    }
}
