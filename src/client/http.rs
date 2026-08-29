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

    /// Submit a chat prompt and collect all streamed events.
    pub fn chat(
        &self,
        session_id: &str,
        prompt: &str,
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

        let text = response.text()?;
        let mut events = Vec::new();

        // Parse SSE events from the response body.
        for block in text.split("\n\n") {
            let data: String = block
                .lines()
                .filter(|line| line.starts_with("data: "))
                .map(|line| &line[6..])
                .collect::<Vec<_>>()
                .join("\n");

            if data.is_empty() || data == "ping" {
                continue;
            }

            if let Ok(event) = serde_json::from_str::<StreamEvent>(&data) {
                events.push(event);
            }
        }

        Ok(events)
    }

    /// Send an approval decision for a pending tool execution.
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
