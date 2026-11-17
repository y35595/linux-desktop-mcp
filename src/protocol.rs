//! JSON-RPC 2.0 over stdio — the MCP wire layer.

use serde_json::{json, Value};
use std::io::{BufRead, Write};

pub const PROTOCOL_VERSION: &str = "2024-11-05";

pub fn read_message<R: BufRead>(input: &mut R) -> Option<Value> {
    let mut line = String::new();
    loop {
        line.clear();
        match input.read_line(&mut line) {
            Ok(0) => return None,
            Ok(_) => {
                let t = line.trim();
                if t.is_empty() {
                    continue;
                }
                return serde_json::from_str(t).ok();
            }
            Err(_) => return None,
        }
    }
}

pub fn write_message<W: Write>(out: &mut W, v: &Value) -> std::io::Result<()> {
    let s = serde_json::to_string(v).unwrap_or_else(|_| "{}".into());
    out.write_all(s.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

pub fn result(id: Value, value: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": value})
}

pub fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// A tool result carrying one text payload.
pub fn text_result(text: impl Into<String>, is_error: bool) -> Value {
    json!({
        "content": [{"type": "text", "text": text.into()}],
        "isError": is_error,
    })
}

/// A tool result carrying an image plus a short text label.
pub fn image_result(b64: String, mime: &str, caption: String) -> Value {
    json!({
        "content": [
            {"type": "image", "data": b64, "mimeType": mime},
            {"type": "text", "text": caption},
        ],
        "isError": false,
    })
}

pub fn tool_schema(name: &str, description: &str, input: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input,
    })
}
