//! linux-desktop-mcp — a fast, dependency-light computer-use MCP server for
//! Linux desktops on **both** X11 and Wayland.
//!
//! Speaks MCP over stdio. Capture and input are each chosen from an ordered
//! set of backends at startup, and the choice is reported through
//! `screen_info` so a caller always knows what is actually serving it.

mod capture;
mod dbus;
mod input;
mod mutter;
mod platform;
mod protocol;
mod tools;
mod uinput;
mod x11;

use protocol::*;
use serde_json::{json, Value};
use std::io::{BufReader, BufWriter, Write};
use tools::App;

const SERVER_NAME: &str = "linux-desktop";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

fn tool_list() -> Value {
    json!([
        tool_schema(
            "screenshot",
            "Capture the screen (or a region) and return it as a base64 image the model can see. \
             IMPORTANT: the image is downscaled by default (max_width=1280), so image pixel \
             coordinates are NOT screen coordinates. The caption states the exact scale; multiply \
             image pixels by 1/scale, or pass max_width=0 for 1:1 pixels, or call to_screen.",
            json!({
                "type": "object",
                "properties": {
                    "x": {"type": "integer", "description": "Region left edge (omit for full screen)"},
                    "y": {"type": "integer", "description": "Region top edge"},
                    "width": {"type": "integer", "description": "Region width"},
                    "height": {"type": "integer", "description": "Region height"},
                    "max_width": {"type": "integer", "description": "Downscale to at most this width (default 1280)"},
                    "format": {"type": "string", "enum": ["jpeg", "png"], "description": "Default jpeg"},
                    "quality": {"type": "integer", "description": "JPEG quality 1-100 (default 75)"}
                }
            }),
        ),
        tool_schema(
            "screenshot_file",
            "Capture the screen to a PNG file on disk, for when you need the pixels without \
             spending context tokens.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Absolute output path"},
                    "x": {"type": "integer"}, "y": {"type": "integer"},
                    "width": {"type": "integer"}, "height": {"type": "integer"}
                },
                "required": ["path"]
            }),
        ),
        tool_schema(
            "screen_info",
            "Report screen geometry, session type, and the currently focused window.",
            json!({"type": "object", "properties": {}}),
        ),
        tool_schema(
            "list_windows",
            "List mapped top-level windows with id, title, geometry, and position. \
             Use the id with focus_window, and the geometry to crop screenshots.",
            json!({"type": "object", "properties": {}}),
        ),
        tool_schema(
            "active_window",
            "Return the currently focused window.",
            json!({"type": "object", "properties": {}}),
        ),
        tool_schema(
            "focus_window",
            "Raise and focus a window by id (from list_windows).",
            json!({
                "type": "object",
                "properties": {"window_id": {"type": "integer"}},
                "required": ["window_id"]
            }),
        ),
        tool_schema(
            "mouse_move",
            "Move the pointer to absolute screen coordinates, without clicking.",
            json!({
                "type": "object",
                "properties": {
                    "x": {"type": "integer"}, "y": {"type": "integer"}
                },
                "required": ["x", "y"]
            }),
        ),
        tool_schema(
            "click",
            "Click a mouse button, optionally moving to (x, y) first. \
             count=2 for double click.",
            json!({
                "type": "object",
                "properties": {
                    "x": {"type": "integer"}, "y": {"type": "integer"},
                    "button": {"type": "string", "enum": ["left", "middle", "right"], "description": "Default left"},
                    "count": {"type": "integer", "description": "1-3, default 1"}
                }
            }),
        ),
        tool_schema(
            "mouse_down",
            "Press and hold a mouse button (pair with mouse_move for custom drags).",
            json!({"type": "object", "properties": {"button": {"type": "string", "enum": ["left","middle","right"]}}}),
        ),
        tool_schema(
            "mouse_up",
            "Release a held mouse button.",
            json!({"type": "object", "properties": {"button": {"type": "string", "enum": ["left","middle","right"]}}}),
        ),
        tool_schema(
            "drag",
            "Press at (x1,y1), move to (x2,y2) in steps, release.",
            json!({
                "type": "object",
                "properties": {
                    "x1": {"type": "integer"}, "y1": {"type": "integer"},
                    "x2": {"type": "integer"}, "y2": {"type": "integer"},
                    "button": {"type": "string", "enum": ["left","middle","right"]},
                    "steps": {"type": "integer", "description": "Default 12"}
                },
                "required": ["x1", "y1", "x2", "y2"]
            }),
        ),
        tool_schema(
            "scroll",
            "Scroll the wheel at the pointer (or at x,y if given).",
            json!({
                "type": "object",
                "properties": {
                    "direction": {"type": "string", "enum": ["up","down","left","right"]},
                    "amount": {"type": "integer", "description": "Wheel clicks, default 3"},
                    "x": {"type": "integer"}, "y": {"type": "integer"}
                }
            }),
        ),
        tool_schema(
            "type",
            "Type literal text into the focused window as keyboard input.",
            json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            }),
        ),
        tool_schema(
            "hotkey",
            "Press a key combination, e.g. 'ctrl+alt+t', 'super+Return', 'ctrl+shift+z'.",
            json!({
                "type": "object",
                "properties": {"keys": {"type": "string"}},
                "required": ["keys"]
            }),
        ),
        tool_schema(
            "to_screen",
            "Convert a pixel coordinate read off a screenshot into the screen coordinate that \
             click/mouse_move expect. Uses the scale from that screenshot's caption.",
            json!({
                "type": "object",
                "properties": {
                    "u": {"type": "number", "description": "x pixel in the screenshot image"},
                    "v": {"type": "number", "description": "y pixel in the screenshot image"},
                    "image_width": {"type": "number", "description": "width of that image (from caption)"},
                    "source_width": {"type": "number", "description": "width of the captured region in screen px (defaults to image_width)"},
                    "origin_x": {"type": "number", "description": "region left edge, default 0"},
                    "origin_y": {"type": "number", "description": "region top edge, default 0"}
                },
                "required": ["u", "v", "image_width"]
            }),
        ),
        tool_schema(
            "desktop_stats",
            "Report cumulative capture and input latency measured by this server.",
            json!({"type": "object", "properties": {}}),
        ),
    ])
}

fn main() {
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    let app = match App::new() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[linux-desktop-mcp] fatal: cannot initialise any desktop backend: {e:#}");
            eprintln!(
                "[linux-desktop-mcp] hint: this server supports X11 sessions directly and \
                 Wayland sessions through xdg-desktop-portal (capture) plus /dev/uinput \
                 (input). See the README for the one-time uinput udev rule."
            );
            std::process::exit(1);
        }
    };

    while let Some(msg) = read_message(&mut input) {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(json!({}));

        // Notifications carry no id and expect no response.
        let is_notification = id.is_none();

        let response = match method {
            "initialize" => Some(result(
                id.clone().unwrap_or(Value::Null),
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION}
                }),
            )),
            "notifications/initialized" | "initialized" => None,
            "ping" => Some(result(id.clone().unwrap_or(Value::Null), json!({}))),
            "tools/list" => Some(result(
                id.clone().unwrap_or(Value::Null),
                json!({"tools": tool_list()}),
            )),
            "tools/call" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let payload = match app.call(name, &args) {
                    Ok(v) => v,
                    Err(e) => text_result(format!("error: {e:#}"), true),
                };
                Some(result(id.clone().unwrap_or(Value::Null), payload))
            }
            "resources/list" => Some(result(
                id.clone().unwrap_or(Value::Null),
                json!({"resources": []}),
            )),
            "prompts/list" => Some(result(
                id.clone().unwrap_or(Value::Null),
                json!({"prompts": []}),
            )),
            other => {
                if is_notification {
                    None
                } else {
                    Some(error(
                        id.clone().unwrap_or(Value::Null),
                        -32601,
                        &format!("method not found: {other}"),
                    ))
                }
            }
        };

        if let Some(r) = response {
            if write_message(&mut out, &r).is_err() {
                break;
            }
        }
        let _ = out.flush();
    }
}
