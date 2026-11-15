//! One shared HTTP agent with timeouts, size caps and a browser user agent.

use crate::{Error, Result};
use std::time::Duration;

/// A browser user agent: some Twitch endpoints refuse unknown clients.
pub const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

/// Largest JSON answer we read (7TV channel sets are about 2.5 MB).
pub const JSON_LIMIT: u64 = 16 * 1024 * 1024;

/// A cheap-to-clone HTTP client. Connections are pooled across clones.
#[derive(Clone)]
pub struct Http {
    agent: ureq::Agent,
    user_agent: String,
}

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn net(e: ureq::Error) -> Error {
    match e {
        ureq::Error::StatusCode(s) => Error::Status(s),
        ureq::Error::BodyExceedsLimit(n) => Error::Parse(format!("answer larger than {n} bytes")),
        ureq::Error::Json(e) => Error::Parse(e.to_string()),
        other => Error::Network(other.to_string()),
    }
}

impl Http {
    pub fn new() -> Self {
        Self::with_user_agent(USER_AGENT)
    }

    /// A client that identifies as the viewer's own browser.
    pub fn with_user_agent(user_agent: &str) -> Self {
        let ua = if user_agent.trim().is_empty() || !user_agent.is_ascii() { USER_AGENT } else { user_agent.trim() };
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .timeout_connect(Some(Duration::from_secs(8)))
            .user_agent(ua)
            .max_redirects(4)
            .build();
        Self { agent: ureq::Agent::new_with_config(config), user_agent: ua.to_owned() }
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    pub fn agent(&self) -> &ureq::Agent {
        &self.agent
    }

    /// GET returning the body, at most `limit` bytes.
    pub fn get_bytes(&self, url: &str, headers: &[(&str, &str)], limit: u64) -> Result<Vec<u8>> {
        let mut req = self.agent.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let mut resp = req.call().map_err(net)?;
        resp.body_mut().with_config().limit(limit).read_to_vec().map_err(net)
    }

    /// GET returning parsed JSON.
    pub fn get_json(&self, url: &str, headers: &[(&str, &str)]) -> Result<serde_json::Value> {
        let bytes = self.get_bytes(url, headers, JSON_LIMIT)?;
        serde_json::from_slice(&bytes).map_err(|e| Error::Parse(e.to_string()))
    }

    /// GET returning text.
    pub fn get_text(&self, url: &str, headers: &[(&str, &str)]) -> Result<String> {
        let bytes = self.get_bytes(url, headers, JSON_LIMIT)?;
        String::from_utf8(bytes).map_err(|e| Error::Parse(e.to_string()))
    }

    /// POST a JSON body, parse a JSON answer. Statuses in `accept` are read
    /// as normal answers (GQL reports errors with 400/401 and a JSON body).
    pub fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &serde_json::Value,
        accept: &[u16],
    ) -> Result<serde_json::Value> {
        let mut req = self.agent.post(url).config().http_status_as_error(false).build();
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let mut resp = req.send_json(body).map_err(net)?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) && !accept.contains(&status) {
            return Err(Error::Status(status));
        }
        let bytes = resp.body_mut().with_config().limit(JSON_LIMIT).read_to_vec().map_err(net)?;
        serde_json::from_slice(&bytes).map_err(|e| Error::Parse(e.to_string()))
    }
}
