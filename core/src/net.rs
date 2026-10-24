//! The one shared HTTP agent.

use crate::{Error, Result};
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Duration;

pub const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

/// Shared agent: connection reuse across every request, HTTP errors returned as responses.
pub fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_response(Some(Duration::from_secs(15)))
            .timeout_recv_body(Some(Duration::from_secs(30)))
            // Artwork loads eight at a time from a few hosts: keep those connections open.
            .max_idle_connections(32)
            .max_idle_connections_per_host(8)
            .build()
            .into()
    })
}

/// POST JSON and parse a JSON reply. Non-2xx statuses become `Error::Net`.
pub fn post_json(url: &str, headers: &[(&str, &str)], body: &Value) -> Result<Value> {
    let mut rb = agent().post(url).header("Content-Type", "application/json");
    for (k, v) in headers {
        rb = rb.header(*k, *v);
    }
    let mut r = rb
        .send(serde_json::to_vec(body).expect("json"))
        .map_err(|e| Error::Net(e.to_string()))?;
    let status = r.status().as_u16();
    let bytes = r
        .body_mut()
        .with_config()
        .limit(32 << 20)
        .read_to_vec()
        .map_err(|e| Error::Net(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(Error::Net(format!("HTTP {status} from {}", short(url))));
    }
    serde_json::from_slice(&bytes).map_err(|e| Error::Net(format!("bad JSON: {e}")))
}

/// GET and parse a JSON reply. Non-2xx statuses become `Error::Net` ("HTTP 404 from …").
pub fn get_json(url: &str, headers: &[(&str, &str)]) -> Result<Value> {
    let mut rb = agent().get(url);
    for (k, v) in headers {
        rb = rb.header(*k, *v);
    }
    let mut r = rb.call().map_err(|e| Error::Net(e.to_string()))?;
    let status = r.status().as_u16();
    let bytes = r
        .body_mut()
        .with_config()
        .limit(8 << 20)
        .read_to_vec()
        .map_err(|e| Error::Net(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(Error::Net(format!("HTTP {status} from {}", short(url))));
    }
    serde_json::from_slice(&bytes).map_err(|e| Error::Net(format!("bad JSON: {e}")))
}

/// URL without its query string, for log and error messages.
pub fn short(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "M12"]
    fn bodies_have_no_total_time_limit_but_each_read_does() {
        let c = agent().config();
        assert_eq!(c.timeouts().recv_body, None);
        assert_eq!(c.timeout_per_read(), Some(Duration::from_secs(15)));
        assert_eq!(c.timeouts().connect, Some(Duration::from_secs(10)));
        assert_eq!(c.timeouts().recv_response, Some(Duration::from_secs(15)));
    }
}
