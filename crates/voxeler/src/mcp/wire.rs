//! MCP / JSON-RPC 2.0 wire types for a **tools-only server**.
//!
//! Hand-rolled rather than taken from an SDK, for the reason `kessel`'s are:
//! `rmcp`'s value is its `#[tool]` macros over typed Rust functions, and these
//! tools are dynamically dispatched against hand-authored JSON schemas, so the
//! macros buy nothing — while the SDK would bring an async runtime into a
//! program whose whole architecture is a blocking event loop.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// MCP revision this server implements. The tools-only surface is unchanged
/// across recent revisions, so [`negotiate_version`] echoes whatever the client
/// asks for rather than forcing a downgrade.
pub const PROTOCOL_VERSION: &str = "2024-11-05";
pub const JSONRPC_VERSION: &str = "2.0";

// Standard JSON-RPC 2.0 error codes.
pub const PARSE_ERROR: i32 = -32700;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INTERNAL_ERROR: i32 = -32603;

/// A JSON-RPC 2.0 request or notification (`id` absent ⇒ notification).
#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

impl Request {
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

#[derive(Debug, Serialize)]
pub struct ErrorObject {
    pub code: i32,
    pub message: String,
}

impl Response {
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: None,
            error: Some(ErrorObject {
                code,
                message: message.into(),
            }),
        }
    }
}

/// A tool as advertised by `tools/list`.
#[derive(Debug, Clone, Serialize)]
pub struct ToolInfo {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// Parameters of a `tools/call` request.
#[derive(Debug, Clone, Deserialize)]
pub struct CallParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

/// One content block in a tool result. Text only: an agent driving this editor
/// reads counts, and the picture is on the user's screen already — which is the
/// whole reason this transport is SSE and not stdio.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum Content {
    #[serde(rename = "text")]
    Text { text: String },
}

/// Result of a `tools/call`.
///
/// Note `is_error` is *not* a JSON-RPC error: a tool that ran and failed reports
/// it here so the model can read the message and correct itself. JSON-RPC
/// errors are reserved for protocol faults.
#[derive(Debug, Clone, Serialize)]
pub struct CallResult {
    pub content: Vec<Content>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

impl CallResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Text { text: text.into() }],
            is_error: None,
        }
    }

    pub fn failure(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Text { text: text.into() }],
            is_error: Some(true),
        }
    }
}

/// Pick the protocol version to report back. The client sends the revision it
/// wants in `initialize`; echoing it keeps us compatible with both older and
/// newer hosts, since the tools-only wire format they rely on is identical.
pub fn negotiate_version(params: Option<&Value>) -> String {
    params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn notification_has_no_id() {
        let r: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap();
        assert!(r.is_notification());
    }

    #[test]
    fn clean_result_omits_is_error() {
        let s = serde_json::to_string(&CallResult::text("ok")).unwrap();
        assert!(!s.contains("isError"), "{s}");
        let s = serde_json::to_string(&CallResult::failure("no")).unwrap();
        assert!(s.contains(r#""isError":true"#), "{s}");
    }

    #[test]
    fn version_echoes_the_client_then_falls_back() {
        let p = json!({"protocolVersion": "2025-06-18"});
        assert_eq!(negotiate_version(Some(&p)), "2025-06-18");
        assert_eq!(negotiate_version(None), PROTOCOL_VERSION);
    }

    #[test]
    fn an_error_response_carries_no_result_field() {
        let s = serde_json::to_string(&Response::error(json!(1), METHOD_NOT_FOUND, "nope")).unwrap();
        assert!(!s.contains("result"), "{s}");
        assert!(s.contains("-32601"), "{s}");
    }
}
