//! Progress and log notifications, and the log levels that filter them.

use super::protocol::SERVER_NAME;
use serde_json::{Value, json};

/// MCP log levels, lowest first. A client that sets a level asks to hear only
/// that level and above.
pub(super) const LEVELS: [&str; 8] = [
    "debug",
    "info",
    "notice",
    "warning",
    "error",
    "critical",
    "alert",
    "emergency",
];

/// Progress goes out as `info` log messages.
pub(super) const PROGRESS_LEVEL: &str = "info";

pub(super) fn rank(level: &str) -> Option<usize> {
    LEVELS.iter().position(|l| *l == level)
}

/// `notifications/progress`: a counter that rises with each line, and the line.
pub(super) fn progress_message(token: &Value, progress: u64, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": { "progressToken": token, "progress": progress, "message": text }
    })
}

pub(super) fn log_message(text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/message",
        "params": { "level": PROGRESS_LEVEL, "logger": SERVER_NAME, "data": text }
    })
}
