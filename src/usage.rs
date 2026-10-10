//! `provider/usage`: Cursor subscription card.
//!
//! The CLI publishes plan metadata only — `cursor-agent about --format
//! json` carries `subscriptionTier`; request usage lives behind the
//! cursor.com dashboard session, not the CLI's credential. The card
//! reports the real tier and no fabricated windows.

use serde_json::{Value, json};

use crate::setup;

/// `provider/usage` entry: the `about` payload is the whole source.
pub fn handle(_params: &Value) -> Result<Value, String> {
    let about = setup::about().ok_or_else(|| {
        "cursor-agent not available or not logged in".to_string()
    })?;
    let email = about.get("userEmail").and_then(Value::as_str).unwrap_or_default();
    if email.is_empty() {
        return Err("cursor-agent not logged in".to_string());
    }
    let tier = about
        .get("subscriptionTier")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty());
    Ok(json!({
        "available": true,
        "title": "Cursor",
        "plan": tier,
        "windows": [],
        "note": "request usage lives on cursor.com/dashboard — the CLI publishes tier only",
    }))
}
