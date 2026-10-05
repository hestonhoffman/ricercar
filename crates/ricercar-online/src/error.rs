//! Error type shared by every online integration.

/// Errors returned by online integrations.
#[derive(Debug, thiserror::Error)]
pub enum OnlineError {
    /// The server answered with a non-success HTTP status. `body` is kept
    /// for callers that parse it; the message only shows a readable part.
    #[error("HTTP {status}{}", http_detail(*status, body))]
    Http { status: u16, body: String },
    /// DNS, connection, TLS or timeout failure: worth retrying later.
    #[error("network error: {0}")]
    Network(String),
    /// The response (or a local file) could not be decoded.
    #[error("parse error: {0}")]
    Parse(String),
    /// Credentials were rejected, or authorization is still pending.
    #[error("authentication error: {0}")]
    Auth(String),
    /// The service lacks the credentials it needs (empty token/key).
    #[error("service not configured")]
    NotConfigured,
    /// Local filesystem failure (queue or cache files).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl OnlineError {
    /// Whether retrying the same request later may succeed (offline, server
    /// overloaded, rate limited). Used to decide whether to queue a scrobble.
    pub fn is_transient(&self) -> bool {
        match self {
            OnlineError::Network(_) => true,
            OnlineError::Http { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }
}

pub type Result<T> = std::result::Result<T, OnlineError>;

/// " Bad Gateway: <detail>" for an HTTP error: the reason phrase of the
/// status, then the readable part of the body if there is one.
fn http_detail(status: u16, body: &str) -> String {
    let mut out = String::new();
    if let Some(reason) = reason_phrase(status) {
        out.push(' ');
        out.push_str(reason);
    }
    if let Some(detail) = body_summary(body) {
        out.push_str(": ");
        out.push_str(&detail);
    }
    out
}

fn reason_phrase(status: u16) -> Option<&'static str> {
    Some(match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => return None,
    })
}

/// The readable part of an error body: the `error` or `message` field of a
/// JSON answer, or one short line of plain text. Never an HTML page.
pub(crate) fn body_summary(body: &str) -> Option<String> {
    let body = body.trim();
    if body.is_empty() || body.starts_with('<') {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        return ["error", "message", "detail"]
            .iter()
            .find_map(|k| v.get(*k).and_then(|m| m.as_str()))
            .map(|m| short(m.trim()))
            .filter(|m| !m.is_empty());
    }
    (!body.contains('\n') && !body.contains('<') && body.chars().count() <= 160)
        .then(|| short(body))
}

fn short(s: &str) -> String {
    if s.chars().count() <= 160 {
        return s.to_string();
    }
    let cut: String = s.chars().take(157).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_classification() {
        assert!(OnlineError::Network("x".into()).is_transient());
        let http = |status| OnlineError::Http {
            status,
            body: String::new(),
        };
        assert!(http(503).is_transient());
        assert!(http(429).is_transient());
        assert!(!http(400).is_transient());
        assert!(!OnlineError::Auth("bad".into()).is_transient());
        assert!(!OnlineError::NotConfigured.is_transient());
    }

    #[test]
    fn http_errors_read_well() {
        let e = |status, body: &str| {
            OnlineError::Http {
                status,
                body: body.into(),
            }
            .to_string()
        };
        let page = "<!DOCTYPE html>\n<html><head><title>502 Bad Gateway</title>";
        assert_eq!(e(502, page), "HTTP 502 Bad Gateway");
        assert_eq!(
            e(
                401,
                r#"{"code": 401, "error": "Invalid authorization token."}"#
            ),
            "HTTP 401 Unauthorized: Invalid authorization token."
        );
        assert_eq!(
            e(503, "busy, retry later"),
            "HTTP 503 Service Unavailable: busy, retry later"
        );
        assert_eq!(e(418, ""), "HTTP 418");
        assert_eq!(
            e(500, "line one\nline two"),
            "HTTP 500 Internal Server Error"
        );
    }
}
