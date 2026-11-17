//! Tool dispatch: each MCP tool maps to one function here.

use crate::protocol::{image_result, text_result};
use crate::x11::{Desktop, Keymap};
use anyhow::{anyhow, Result};
use base64::Engine;
use image::{ImageEncoder, RgbImage};
use serde_json::{json, Value};
use std::io::Cursor;
use std::sync::Mutex;

/// Tool call latency accounting, exposed through `desktop_stats`.
/// Facts about one capture that an agent needs in order to click accurately.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CaptureMeta {
    pub screen_width: u32,
    pub screen_height: u32,
    pub image_width: u32,
    pub image_height: u32,
    pub origin_x: i32,
    pub origin_y: i32,
    /// image_pixels / screen_pixels. 1.0 means the image is 1:1.
    pub scale: f64,
}

#[derive(Default)]
pub struct Stats {
    pub captures: u64,
    pub capture_ms_total: f64,
    pub inputs: u64,
    pub input_ms_total: f64,
}

pub struct App {
    pub desktop: Desktop,
    pub keymap: Mutex<Option<Keymap>>,
    pub stats: Mutex<Stats>,
}

fn arg_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64())
}
fn arg_f64(args: &Value, key: &str) -> Option<f64> {
    args.get(key)
        .and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|i| i as f64)))
}
fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

/// Map a name like "ctrl+shift+t" to X keysyms.
fn parse_hotkey(spec: &str) -> Result<Vec<u32>> {
    let mut syms = Vec::new();
    for raw in spec.split('+') {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        let lower = part.to_ascii_lowercase();
        let sym = match lower.as_str() {
            "ctrl" | "control" => 0xffe3,
            "shift" => 0xffe1,
            "alt" | "mod1" => 0xffe9,
            "super" | "meta" | "win" | "mod4" => 0xffeb,
            "enter" | "return" => 0xff0d,
            "tab" => 0xff09,
            "esc" | "escape" => 0xff1b,
            "space" => 0x0020,
            "backspace" => 0xff08,
            "delete" | "del" => 0xffff,
            "home" => 0xff50,
            "end" => 0xff57,
            "pageup" => 0xff55,
            "pagedown" => 0xff56,
            "up" => 0xff52,
            "down" => 0xff54,
            "left" => 0xff51,
            "right" => 0xff53,
            "insert" => 0xff63,
            other => {
                if other.len() == 1 {
                    let c = other.chars().next().unwrap();
                    char_to_keysym(c)
                } else if let Some(rest) = other.strip_prefix('f') {
                    if let Ok(n) = rest.parse::<u32>() {
                        if (1..=24).contains(&n) {
                            0xffbe + (n - 1)
                        } else {
                            return Err(anyhow!("unsupported function key {}", other));
                        }
                    } else {
                        return Err(anyhow!("unknown key '{}'", part));
                    }
                } else {
                    return Err(anyhow!("unknown key '{}'", part));
                }
            }
        };
        syms.push(sym);
    }
    if syms.is_empty() {
        return Err(anyhow!("empty hotkey"));
    }
    Ok(syms)
}

fn char_to_keysym(c: char) -> u32 {
    // Latin-1 maps directly; ASCII maps directly; everything else via a small table.
    let cp = c as u32;
    if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) {
        cp
    } else {
        match c {
            '\n' => 0xff0d,
            '\t' => 0xff09,
            _ => cp | 0x0100_0000, // Unicode keysym convention
        }
    }
}

impl App {
    pub fn new() -> Result<Self> {
        let desktop = Desktop::connect()?;
        Ok(App {
            desktop,
            keymap: Mutex::new(None),
            stats: Mutex::new(Stats::default()),
        })
    }

    fn ensure_keymap(&self) -> Result<()> {
        let mut guard = self.keymap.lock().unwrap();
        if guard.is_none() {
            *guard = Some(Keymap::build(self.desktop.conn())?);
        }
        Ok(())
    }

    fn type_text(&self, text: &str) -> Result<()> {
        self.ensure_keymap()?;
        let guard = self.keymap.lock().unwrap();
        let km = guard.as_ref().unwrap();
        let shift_code = km.lookup(0xffe1).map(|(c, _)| c);
        for ch in text.chars() {
            let sym = char_to_keysym(ch);
            let (code, needs_shift) = km
                .lookup(sym)
                .ok_or_else(|| anyhow!("no keycode for character {:?}", ch))?;
            let shift = needs_shift.then_some(shift_code).flatten();
            if let Some(s) = shift {
                self.desktop.key(s, true)?;
            }
            self.desktop.key(code, true)?;
            self.desktop.key(code, false)?;
            if let Some(s) = shift {
                self.desktop.key(s, false)?;
            }
            self.desktop.sync();
        }
        Ok(())
    }

    fn hotkey(&self, spec: &str) -> Result<()> {
        self.ensure_keymap()?;
        let syms = parse_hotkey(spec)?;
        let guard = self.keymap.lock().unwrap();
        let km = guard.as_ref().unwrap();
        let mut codes = Vec::new();
        for sym in syms {
            let (code, _) = km
                .lookup(sym)
                .ok_or_else(|| anyhow!("no keycode for keysym 0x{:x} in '{}'", sym, spec))?;
            codes.push(code);
        }
        // Press all modifiers, then the final key, then release in reverse.
        for &c in &codes {
            self.desktop.key(c, true)?;
        }
        for &c in codes.iter().rev() {
            self.desktop.key(c, false)?;
        }
        self.desktop.sync();
        Ok(())
    }

    /// Encode a frame as PNG or JPEG and return base64.
    fn encode(
        &self,
        region: Option<(i16, i16, u16, u16)>,
        max_width: Option<u32>,
        format: &str,
        quality: u8,
    ) -> Result<(String, String, String, CaptureMeta)> {
        let t0 = std::time::Instant::now();
        let frame = self.desktop.capture(region)?;
        let cap_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let mut img = RgbImage::from_raw(frame.width as u32, frame.height as u32, frame.rgb)
            .ok_or_else(|| anyhow!("failed to build image buffer"))?;

        if let Some(mw) = max_width {
            if img.width() > mw && mw > 0 {
                let nh = (img.height() as f64 * (mw as f64 / img.width() as f64)).round() as u32;
                img = image::imageops::resize(
                    &img,
                    mw,
                    nh.max(1),
                    image::imageops::FilterType::Triangle,
                );
            }
        }

        let mut buf = Cursor::new(Vec::new());
        let mime = if format.eq_ignore_ascii_case("png") {
            img.write_to(&mut buf, image::ImageFormat::Png)?;
            "image/png"
        } else {
            let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
            enc.write_image(
                img.as_raw(),
                img.width(),
                img.height(),
                image::ExtendedColorType::Rgb8,
            )?;
            "image/jpeg"
        };

        let bytes = buf.into_inner();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let (ox, oy, sw, _sh) = (
            region.map(|r| r.0).unwrap_or(0) as i32,
            region.map(|r| r.1).unwrap_or(0) as i32,
            frame.width as u32,
            frame.height as u32,
        );
        let meta = CaptureMeta {
            screen_width: self.desktop.geom().width as u32,
            screen_height: self.desktop.geom().height as u32,
            image_width: img.width(),
            image_height: img.height(),
            origin_x: ox,
            origin_y: oy,
            scale: if sw > 0 {
                img.width() as f64 / sw as f64
            } else {
                1.0
            },
        };

        // State the scale explicitly: an agent that reads pixels off this image
        // MUST multiply by 1/scale before clicking, or every click lands in the
        // upper-left of the intended target.
        let mut caption = format!(
            "image {}x{} {} {}KB capture {:.1}ms",
            img.width(),
            img.height(),
            if mime == "image/png" { "PNG" } else { "JPEG" },
            bytes.len() / 1024,
            cap_ms
        );
        if (meta.scale - 1.0).abs() > 0.001 {
            caption.push_str(&format!(
                "\nSCALED: screen is {}x{} but this image is {}x{} (scale {:.4}). \
                 Image pixel (u,v) belongs to screen coordinate ({},{}) — multiply by {:.4} before calling click/mouse_move, \
                 or pass max_width={} to get 1:1 pixels.",
                meta.screen_width,
                meta.screen_height,
                img.width(),
                img.height(),
                meta.scale,
                ox,
                oy,
                1.0 / meta.scale,
                meta.screen_width
            ));
        } else {
            caption.push_str(&format!(
                "\n1:1 pixels, origin ({}, {}). Image pixel (u,v) = screen ({}+u, {}+v).",
                ox, oy, ox, oy
            ));
        }

        let mut st = self.stats.lock().unwrap();
        st.captures += 1;
        st.capture_ms_total += cap_ms;
        Ok((b64, mime.to_string(), caption, meta))
    }

    pub fn call(&self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "screenshot" => {
                let region = match (
                    arg_i64(args, "x"),
                    arg_i64(args, "y"),
                    arg_i64(args, "width"),
                    arg_i64(args, "height"),
                ) {
                    (Some(x), Some(y), Some(w), Some(h)) => {
                        Some((x as i16, y as i16, w as u16, h as u16))
                    }
                    _ => None,
                };
                // max_width=0 or "native" means: keep 1:1 pixels so coordinates
                // read off the image map directly onto the screen.
                let max_w = match arg_i64(args, "max_width") {
                    Some(0) => None,
                    Some(v) => Some(v as u32),
                    None => Some(1280),
                };
                let fmt = arg_str(args, "format").unwrap_or("jpeg");
                let q = arg_i64(args, "quality").unwrap_or(75) as u8;
                let (b64, mime, cap, _meta) = self.encode(region, max_w, fmt, q)?;
                Ok(image_result(b64, &mime, cap))
            }

            "screenshot_file" => {
                let path = arg_str(args, "path").ok_or_else(|| anyhow!("path is required"))?;
                let region = match (
                    arg_i64(args, "x"),
                    arg_i64(args, "y"),
                    arg_i64(args, "width"),
                    arg_i64(args, "height"),
                ) {
                    (Some(x), Some(y), Some(w), Some(h)) => {
                        Some((x as i16, y as i16, w as u16, h as u16))
                    }
                    _ => None,
                };
                let (b64, _mime, cap, _meta) = self.encode(region, None, "png", 75)?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| anyhow!("decode failed: {e}"))?;
                std::fs::write(path, &bytes)?;
                Ok(text_result(
                    format!("saved {} ({} bytes) — {}", path, bytes.len(), cap),
                    false,
                ))
            }

            "screen_info" => {
                let g = self.desktop.geom();
                let mut v = json!({
                    "width": g.width,
                    "height": g.height,
                    "display": std::env::var("DISPLAY").unwrap_or_default(),
                    "session_type": std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
                });
                if let Some(active) = self.desktop.active_window() {
                    v["active_window"] = serde_json::to_value(active)?;
                }
                Ok(text_result(serde_json::to_string_pretty(&v)?, false))
            }

            "list_windows" => {
                let wins = self.desktop.list_windows()?;
                Ok(text_result(serde_json::to_string_pretty(&wins)?, false))
            }

            "active_window" => match self.desktop.active_window() {
                Some(w) => Ok(text_result(serde_json::to_string_pretty(&w)?, false)),
                None => Ok(text_result("no active window found", true)),
            },

            "focus_window" => {
                let id =
                    arg_i64(args, "window_id").ok_or_else(|| anyhow!("window_id is required"))?;
                self.desktop.activate(id as u32)?;
                Ok(text_result(format!("focused window 0x{:x}", id), false))
            }

            "mouse_move" => {
                let x = arg_i64(args, "x").ok_or_else(|| anyhow!("x is required"))? as i16;
                let y = arg_i64(args, "y").ok_or_else(|| anyhow!("y is required"))? as i16;
                let t0 = std::time::Instant::now();
                self.desktop.move_pointer(x, y)?;
                self.record_input(t0);
                Ok(text_result(format!("moved pointer to ({x}, {y})"), false))
            }

            "click" => {
                let x = arg_i64(args, "x").map(|v| v as i16);
                let y = arg_i64(args, "y").map(|v| v as i16);
                let button = match arg_str(args, "button").unwrap_or("left") {
                    "left" => 1u8,
                    "middle" => 2,
                    "right" => 3,
                    other => other
                        .parse::<u8>()
                        .map_err(|_| anyhow!("bad button '{other}'"))?,
                };
                let count = arg_i64(args, "count").unwrap_or(1).clamp(1, 3);
                let t0 = std::time::Instant::now();
                if let (Some(x), Some(y)) = (x, y) {
                    self.desktop.move_pointer(x, y)?;
                }
                for _ in 0..count {
                    self.desktop.click(button, true)?;
                    self.desktop.click(button, false)?;
                }
                self.desktop.sync();
                self.record_input(t0);
                Ok(text_result(
                    format!("clicked button {button} x{count}{}", coords_note(x, y)),
                    false,
                ))
            }

            "mouse_down" | "mouse_up" => {
                let button = match arg_str(args, "button").unwrap_or("left") {
                    "left" => 1u8,
                    "middle" => 2,
                    "right" => 3,
                    other => other
                        .parse::<u8>()
                        .map_err(|_| anyhow!("bad button '{other}'"))?,
                };
                let t0 = std::time::Instant::now();
                self.desktop.click(button, name == "mouse_down")?;
                self.desktop.sync();
                self.record_input(t0);
                Ok(text_result(format!("{name} button {button}"), false))
            }

            "drag" => {
                let (x1, y1) = (
                    arg_i64(args, "x1").ok_or_else(|| anyhow!("x1 required"))? as i16,
                    arg_i64(args, "y1").ok_or_else(|| anyhow!("y1 required"))? as i16,
                );
                let (x2, y2) = (
                    arg_i64(args, "x2").ok_or_else(|| anyhow!("x2 required"))? as i16,
                    arg_i64(args, "y2").ok_or_else(|| anyhow!("y2 required"))? as i16,
                );
                let button = match arg_str(args, "button").unwrap_or("left") {
                    "left" => 1u8,
                    "right" => 3,
                    "middle" => 2,
                    _ => 1,
                };
                let steps = arg_i64(args, "steps").unwrap_or(12).clamp(1, 100) as i32;
                let t0 = std::time::Instant::now();
                self.desktop.move_pointer(x1, y1)?;
                self.desktop.click(button, true)?;
                for i in 1..=steps {
                    let t = i as f64 / steps as f64;
                    let cx = x1 as f64 + (x2 - x1) as f64 * t;
                    let cy = y1 as f64 + (y2 - y1) as f64 * t;
                    self.desktop.move_pointer(cx as i16, cy as i16)?;
                }
                self.desktop.click(button, false)?;
                self.desktop.sync();
                self.record_input(t0);
                Ok(text_result(
                    format!("dragged ({x1},{y1}) -> ({x2},{y2}) button {button} in {steps} steps"),
                    false,
                ))
            }

            "scroll" => {
                let amount = arg_i64(args, "amount").unwrap_or(3);
                let direction = arg_str(args, "direction").unwrap_or("down");
                let (button, times) = match direction {
                    "up" => (4u8, amount.unsigned_abs().min(50)),
                    "down" => (5u8, amount.unsigned_abs().min(50)),
                    "left" => (6u8, amount.unsigned_abs().min(50)),
                    "right" => (7u8, amount.unsigned_abs().min(50)),
                    other => return Err(anyhow!("bad direction '{other}'")),
                };
                let t0 = std::time::Instant::now();
                if let (Some(x), Some(y)) = (arg_i64(args, "x"), arg_i64(args, "y")) {
                    self.desktop.move_pointer(x as i16, y as i16)?;
                }
                for _ in 0..times {
                    self.desktop.click(button, true)?;
                    self.desktop.click(button, false)?;
                }
                self.desktop.sync();
                self.record_input(t0);
                Ok(text_result(
                    format!(
                        "scrolled {direction} x{times}{}",
                        coords_note(
                            arg_i64(args, "x").map(|v| v as i16),
                            arg_i64(args, "y").map(|v| v as i16)
                        )
                    ),
                    false,
                ))
            }

            "type" => {
                let text = arg_str(args, "text").ok_or_else(|| anyhow!("text is required"))?;
                let t0 = std::time::Instant::now();
                self.type_text(text)?;
                self.record_input(t0);
                Ok(text_result(
                    format!("typed {} characters", text.chars().count()),
                    false,
                ))
            }

            "hotkey" => {
                let keys = arg_str(args, "keys").ok_or_else(|| anyhow!("keys is required"))?;
                let t0 = std::time::Instant::now();
                self.hotkey(keys)?;
                self.record_input(t0);
                Ok(text_result(format!("pressed {keys}"), false))
            }

            // Convert a pixel read off a (possibly downscaled) screenshot into
            // the screen coordinate that click/mouse_move expect.
            "to_screen" => {
                let u = arg_f64(args, "u")
                    .or_else(|| arg_f64(args, "x"))
                    .ok_or_else(|| anyhow!("u (image x) is required"))?;
                let v = arg_f64(args, "v")
                    .or_else(|| arg_f64(args, "y"))
                    .ok_or_else(|| anyhow!("v (image y) is required"))?;
                let img_w = arg_f64(args, "image_width").ok_or_else(|| {
                    anyhow!("image_width is required (from the screenshot caption)")
                })?;
                let origin_x = arg_f64(args, "origin_x").unwrap_or(0.0);
                let origin_y = arg_f64(args, "origin_y").unwrap_or(0.0);
                // If the caller supplies the source region width we can derive the
                // scale exactly; otherwise they must pass it in.
                let src_w = arg_f64(args, "source_width").unwrap_or(img_w);
                let scale = if img_w > 0.0 { img_w / src_w } else { 1.0 };
                let sx = (origin_x + u / scale).round() as i64;
                let sy = (origin_y + v / scale).round() as i64;
                Ok(text_result(
                    serde_json::to_string_pretty(&json!({
                        "screen_x": sx,
                        "screen_y": sy,
                        "scale_used": scale,
                        "note": "pass screen_x/screen_y to click or mouse_move"
                    }))?,
                    false,
                ))
            }

            "desktop_stats" => {
                let st = self.stats.lock().unwrap();
                Ok(text_result(
                    serde_json::to_string_pretty(&json!({
                        "captures": st.captures,
                        "capture_ms_avg": if st.captures>0 { st.capture_ms_total / st.captures as f64 } else { 0.0 },
                        "capture_ms_total": st.capture_ms_total,
                        "inputs": st.inputs,
                        "input_ms_avg": if st.inputs>0 { st.input_ms_total / st.inputs as f64 } else { 0.0 },
                        "input_ms_total": st.input_ms_total,
                    }))?,
                    false,
                ))
            }

            other => Err(anyhow!("unknown tool: {other}")),
        }
    }

    fn record_input(&self, t0: std::time::Instant) {
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let mut st = self.stats.lock().unwrap();
        st.inputs += 1;
        st.input_ms_total += ms;
    }
}

fn coords_note(x: Option<i16>, y: Option<i16>) -> String {
    match (x, y) {
        (Some(x), Some(y)) => format!(" at ({x}, {y})"),
        _ => String::new(),
    }
}
