//! Tool dispatch: each MCP tool maps to one function here.

use crate::capture::Capture;
use crate::input::Input;
use crate::platform::{CoordinateModel, Platform, SessionKind};
use crate::protocol::{image_result, text_result};
use crate::uinput::Button;
use crate::x11::Desktop;
use anyhow::{anyhow, Result};
use base64::Engine;
use image::{ImageEncoder, RgbImage};
use serde_json::{json, Value};
use std::io::Cursor;
use std::sync::Mutex;

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
    pub platform: Platform,
    pub capture: Capture,
    pub input: Input,
    /// The coordinate model, refined once a capture reveals the real size.
    pub model: Mutex<CoordinateModel>,
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

impl App {
    pub fn new() -> Result<Self> {
        let platform = Platform::detect();

        // Connect to X whenever DISPLAY exists. On Wayland this is Xwayland:
        // unusable for capture and XTEST, but still the only way to enumerate
        // Xwayland client windows.
        let desktop = match Desktop::connect(platform.is_real_x11()) {
            Ok(d) => Some(d),
            Err(e) => {
                if platform.session == SessionKind::X11 {
                    return Err(e.context(
                        "cannot initialise the X11 backend (DISPLAY is set but unreachable)",
                    ));
                }
                None
            }
        };

        let capture = Capture::detect(platform.clone());

        // Prefer the compositor's authoritative scale; fall back to the X root.
        let mut model = CoordinateModel::unknown();
        let mut warnings: Vec<String> = Vec::new();
        if platform.compositor == crate::platform::Compositor::Gnome {
            if let Some(m) = crate::dbus::gnome_coordinate_model() {
                model = m;
            }
        }
        if model.logical_width == 0 {
            if let Some((w, h)) = crate::dbus::x11_root_size() {
                model.logical_width = w;
                model.logical_height = h;
                model.scale_source = "x11 root window size";
            }
        }
        if model.logical_width == 0 {
            warnings.push(
                "the logical screen size is unknown; it will be derived on the first capture"
                    .to_string(),
            );
        }

        let input = Input::detect(
            platform.session,
            desktop,
            crate::mutter::available(),
            warnings.clone(),
        );
        if !input.has_absolute_pointer() {
            warnings.push(format!(
                "pointer control unavailable with the '{}' backend — clicks will not land. \
                 See the README section on /dev/uinput.",
                input.backend().as_str()
            ));
        }

        let app = App {
            platform,
            capture,
            input,
            model: Mutex::new(model),
            stats: Mutex::new(Stats::default()),
        };
        for w in &app.input.notes {
            eprintln!("[linux-desktop-mcp] {w}");
        }
        eprintln!(
            "[linux-desktop-mcp] session={} compositor={} capture='{}' input='{}'",
            app.platform.session.as_str(),
            app.platform.compositor.as_str(),
            app.capture.backend().as_str(),
            app.input.backend().as_str()
        );
        Ok(app)
    }

    /// Encode a frame as PNG or JPEG and return base64.
    fn encode(
        &self,
        region: Option<(i32, i32, u32, u32)>,
        max_width: Option<u32>,
        format: &str,
        quality: u8,
    ) -> Result<(String, String, String, CaptureMeta)> {
        let t0 = std::time::Instant::now();
        let frame = match region {
            Some((x, y, w, h)) => self.capture.capture_region(x, y, w, h)?,
            None => self.capture.capture_full()?,
        };
        let cap_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // The first capture tells us the true native resolution. Fold that back
        // into the coordinate model so input converts against real numbers.
        {
            let mut m = self.model.lock().unwrap();
            m.reconcile_with_capture(frame.width as i32, frame.height as i32);
        }

        let mut img = RgbImage::from_raw(frame.width, frame.height, frame.rgb)
            .ok_or_else(|| anyhow!("failed to build the image buffer"))?;

        if let Some(mw) = max_width {
            if mw > 0 && img.width() > mw {
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

        let (ox, oy) = (
            region.map(|r| r.0).unwrap_or(0),
            region.map(|r| r.1).unwrap_or(0),
        );
        let src_w = frame.width;
        let model = self.model.lock().unwrap().clone();
        let meta = CaptureMeta {
            screen_width: model.physical_width.max(0) as u32,
            screen_height: model.physical_height.max(0) as u32,
            image_width: img.width(),
            image_height: img.height(),
            origin_x: ox,
            origin_y: oy,
            scale: if src_w > 0 {
                img.width() as f64 / src_w as f64
            } else {
                1.0
            },
        };

        let mut caption = format!(
            "image {}x{} {} {}KB capture {:.1}ms via {}\ncoordinates: this image and all \
             input tools share one space — a pixel read here (plus the origin) is the screen \
             coordinate to pass to click/mouse_move.",
            img.width(),
            img.height(),
            if mime == "image/png" { "PNG" } else { "JPEG" },
            bytes.len() / 1024,
            cap_ms,
            self.capture.backend().as_str(),
        );
        if (meta.scale - 1.0).abs() > 0.001 {
            caption.push_str(&format!(
                "\nSCALED for display only: the native screen is {}x{} while this image is \
                 {}x{} (scale {:.4}). Multiply image pixels by {:.4} to get screen \
                 coordinates, or pass max_width=0 for 1:1 pixels.",
                meta.screen_width,
                meta.screen_height,
                img.width(),
                img.height(),
                meta.scale,
                1.0 / meta.scale
            ));
        } else {
            caption.push_str(&format!(
                "\n1:1 pixels, origin ({}, {}). Image pixel (u,v) = screen ({}+u, {}+v).",
                ox, oy, ox, oy
            ));
        }
        if (model.scale - 1.0).abs() > 0.001 {
            caption.push_str(&format!(
                "\nHiDPI: native pixels are {}x the logical input space ({:.3}). This is \
                 handled automatically.",
                model.scale, model.scale
            ));
        }

        // A locked session is the classic cause of an all-black capture that
        // still "succeeds"; say so instead of returning a useless black image
        // with no explanation.
        if is_effectively_black(&img) {
            if crate::dbus::session_locked() == Some(true) {
                caption.push_str(
                    "\nWARNING: the screen is LOCKED (org.gnome.ScreenSaver GetActive=true). \
                     The compositor does not paint the desktop into a screenshot while the \
                     lock screen is up, so this frame is black. Unlock the session to capture \
                     the desktop.",
                );
            } else {
                caption.push_str(
                    "\nWARNING: this frame is essentially all black. If the desktop is not \
                     actually black, check whether the session is locked or the compositor \
                     is withholding content from the portal.",
                );
            }
        }

        let mut st = self.stats.lock().unwrap();
        st.captures += 1;
        st.capture_ms_total += cap_ms;
        Ok((b64, mime.to_string(), caption, meta))
    }

    pub fn call(&self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "screenshot" => {
                let region = region_args(args);
                let max_w = match arg_i64(args, "max_width") {
                    Some(0) => None,
                    Some(v) if v > 0 => Some(v as u32),
                    Some(_) => None,
                    None => Some(1280),
                };
                let fmt = arg_str(args, "format").unwrap_or("jpeg");
                let q = arg_i64(args, "quality").unwrap_or(75).clamp(1, 100) as u8;
                let (b64, mime, cap, _meta) = self.encode(region, max_w, fmt, q)?;
                Ok(image_result(b64, &mime, cap))
            }

            "screenshot_file" => {
                let path = arg_str(args, "path").ok_or_else(|| anyhow!("path is required"))?;
                let (b64, _mime, cap, _meta) = self.encode(region_args(args), None, "png", 75)?;
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
                let model = self.model.lock().unwrap().clone();
                let mut v = json!({
                    "session_type": self.platform.session.as_str(),
                    "compositor": self.platform.compositor.as_str(),
                    "display": self.platform.display.clone().unwrap_or_default(),
                    "wayland_display": self.platform.wayland_display.clone().unwrap_or_default(),
                    "capture_backend": self.capture.backend().as_str(),
                    "input_backend": self.input.backend().as_str(),
                    "pointer": {
                        "absolute": self.input.has_absolute_pointer(),
                        "verifiable": self.input.backend().can_verify_pointer(),
                    },
                    "coordinates": {
                        "screen_width": model.physical_width,
                        "screen_height": model.physical_height,
                        "logical_width": model.logical_width,
                        "logical_height": model.logical_height,
                        "scale": model.scale,
                        "scale_source": model.scale_source,
                        "known": model.physical_width > 0,
                    },
                    "monitors": model.monitors,
                    "screen_locked": crate::dbus::session_locked(),
                });
                if !self.input.notes.is_empty() {
                    v["notes"] = json!(self.input.notes);
                }
                if let Some(d) = self.input.desktop() {
                    if let Some(active) = d.active_window() {
                        v["active_window"] = serde_json::to_value(active)?;
                    }
                }
                Ok(text_result(serde_json::to_string_pretty(&v)?, false))
            }

            "list_windows" => {
                let d = self.input.desktop().ok_or_else(|| {
                    anyhow!(
                        "windows cannot be enumerated: no X display. On Wayland only Xwayland \
                         clients are visible to X11."
                    )
                })?;
                let wins = d.list_windows()?;
                let mut v = json!({ "windows": wins });
                if self.platform.session == SessionKind::Wayland {
                    v["warning"] = json!(
                        "This is a Wayland session: the list only contains Xwayland \
                         (X11) clients. Native Wayland windows are intentionally not \
                         exposed to X11 and cannot be enumerated this way."
                    );
                }
                Ok(text_result(serde_json::to_string_pretty(&v)?, false))
            }

            "active_window" => {
                let d = self
                    .input
                    .desktop()
                    .ok_or_else(|| anyhow!("no X display available for window queries"))?;
                match d.active_window() {
                    Some(w) => Ok(text_result(serde_json::to_string_pretty(&w)?, false)),
                    None => Ok(text_result(
                        "no active X11 window found (on Wayland this normally means the \
                         focused window is a native Wayland client, which X11 cannot see)",
                        true,
                    )),
                }
            }

            "focus_window" => {
                let id =
                    arg_i64(args, "window_id").ok_or_else(|| anyhow!("window_id is required"))?;
                let d = self
                    .input
                    .desktop()
                    .ok_or_else(|| anyhow!("no X display available for window activation"))?;
                d.activate(id as u32)?;
                Ok(text_result(format!("focused window 0x{id:x}"), false))
            }

            "mouse_move" => {
                let x = arg_i64(args, "x").ok_or_else(|| anyhow!("x is required"))? as i32;
                let y = arg_i64(args, "y").ok_or_else(|| anyhow!("y is required"))? as i32;
                let model = self.model_snapshot();
                let t0 = std::time::Instant::now();
                let report = self.input.move_to(&model, x, y)?;
                self.record_input(t0);
                Ok(text_result(self.describe(&report, "moved pointer"), false))
            }

            "click" => {
                let at = match (arg_i64(args, "x"), arg_i64(args, "y")) {
                    (Some(x), Some(y)) => Some((x as i32, y as i32)),
                    _ => None,
                };
                let button = Button::parse(arg_str(args, "button").unwrap_or("left"))?;
                let count = arg_i64(args, "count").unwrap_or(1).clamp(1, 3) as u32;
                let model = self.model_snapshot();
                let t0 = std::time::Instant::now();
                let report = self.input.click(&model, button, at, count)?;
                self.record_input(t0);
                Ok(text_result(
                    self.describe(&report, &format!("clicked {button:?} x{count}")),
                    false,
                ))
            }

            "mouse_down" | "mouse_up" => {
                let button = Button::parse(arg_str(args, "button").unwrap_or("left"))?;
                let t0 = std::time::Instant::now();
                self.input.button(button, name == "mouse_down")?;
                self.record_input(t0);
                let verb = if name == "mouse_down" {
                    "pressed"
                } else {
                    "released"
                };
                Ok(text_result(format!("{verb} {button:?}"), false))
            }

            "drag" => {
                let (x1, y1) = (
                    arg_i64(args, "x1").ok_or_else(|| anyhow!("x1 required"))? as i32,
                    arg_i64(args, "y1").ok_or_else(|| anyhow!("y1 required"))? as i32,
                );
                let (x2, y2) = (
                    arg_i64(args, "x2").ok_or_else(|| anyhow!("x2 required"))? as i32,
                    arg_i64(args, "y2").ok_or_else(|| anyhow!("y2 required"))? as i32,
                );
                let button = Button::parse(arg_str(args, "button").unwrap_or("left"))?;
                let steps = arg_i64(args, "steps").unwrap_or(12).clamp(1, 100) as i32;
                let model = self.model_snapshot();
                let t0 = std::time::Instant::now();
                self.input.move_to(&model, x1, y1)?;
                self.input.button(button, true)?;
                for i in 1..=steps {
                    let t = i as f64 / steps as f64;
                    let cx = x1 as f64 + (x2 - x1) as f64 * t;
                    let cy = y1 as f64 + (y2 - y1) as f64 * t;
                    self.input
                        .move_to(&model, cx.round() as i32, cy.round() as i32)?;
                }
                self.input.button(button, false)?;
                self.record_input(t0);
                Ok(text_result(
                    format!("dragged ({x1},{y1}) -> ({x2},{y2}) with {button:?} in {steps} steps"),
                    false,
                ))
            }

            "scroll" => {
                let amount = arg_i64(args, "amount").unwrap_or(3).unsigned_abs().min(50) as i32;
                let direction = arg_str(args, "direction").unwrap_or("down");
                let at = match (arg_i64(args, "x"), arg_i64(args, "y")) {
                    (Some(x), Some(y)) => Some((x as i32, y as i32)),
                    _ => None,
                };
                let model = self.model_snapshot();
                let t0 = std::time::Instant::now();
                let report = self.input.scroll(&model, direction, amount, at)?;
                self.record_input(t0);
                Ok(text_result(
                    self.describe(&report, &format!("scrolled {direction} x{amount}")),
                    false,
                ))
            }

            "type" => {
                let text = arg_str(args, "text").ok_or_else(|| anyhow!("text is required"))?;
                let t0 = std::time::Instant::now();
                self.input.type_text(text)?;
                self.record_input(t0);
                Ok(text_result(
                    format!("typed {} characters", text.chars().count()),
                    false,
                ))
            }

            "hotkey" => {
                let keys = arg_str(args, "keys").ok_or_else(|| anyhow!("keys is required"))?;
                let t0 = std::time::Instant::now();
                self.input.hotkey(keys)?;
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
                let src_w = arg_f64(args, "source_width").unwrap_or(img_w);
                let scale = if img_w > 0.0 { img_w / src_w } else { 1.0 };
                let sx = (origin_x + u / scale).round() as i64;
                let sy = (origin_y + v / scale).round() as i64;
                Ok(text_result(
                    serde_json::to_string_pretty(&json!({
                        "screen_x": sx,
                        "screen_y": sy,
                        "scale_used": scale,
                        "note": "pass screen_x/screen_y to click or mouse_move; these are \
                                 native capture pixels, which is the same space every input \
                                 backend expects"
                    }))?,
                    false,
                ))
            }

            "desktop_stats" => {
                let st = self.stats.lock().unwrap();
                let model = self.model.lock().unwrap().clone();
                Ok(text_result(
                    serde_json::to_string_pretty(&json!({
                        "captures": st.captures,
                        "capture_ms_avg": if st.captures>0 { st.capture_ms_total / st.captures as f64 } else { 0.0 },
                        "capture_ms_total": st.capture_ms_total,
                        "inputs": st.inputs,
                        "input_ms_avg": if st.inputs>0 { st.input_ms_total / st.inputs as f64 } else { 0.0 },
                        "input_ms_total": st.input_ms_total,
                        "session_type": self.platform.session.as_str(),
                        "capture_backend": self.capture.backend().as_str(),
                        "input_backend": self.input.backend().as_str(),
                        "coordinate_scale": model.scale,
                        "scale_source": model.scale_source,
                    }))?,
                    false,
                ))
            }

            other => Err(anyhow!("unknown tool: {other}")),
        }
    }

    fn model_snapshot(&self) -> CoordinateModel {
        self.model.lock().unwrap().clone()
    }

    /// Build a human- and machine-readable description of an input action,
    /// always stating whether the result could be verified.
    fn describe(&self, r: &crate::input::InputReport, what: &str) -> String {
        let mut s = what.to_string();
        if r.requested.0 >= 0 {
            s.push_str(&format!(
                " at requested ({}, {})",
                r.requested.0, r.requested.1
            ));
        }
        if r.emitted.0 >= 0 {
            s.push_str(&format!(" emitted ({}, {})", r.emitted.0, r.emitted.1));
        }
        if r.clamped {
            s.push_str(" [position clamped to the screen edge]");
        }
        s.push_str(&format!("\nbackend: {}", self.input.backend().as_str()));
        if r.verified {
            s.push_str("\nposition verified via the X server pointer");
        } else if self.input.has_absolute_pointer() {
            s.push_str(
                "\nnote: the event was delivered to the kernel, but this backend cannot read \
                 the pointer back, so the landing position is not independently confirmed",
            );
        }
        s
    }

    fn record_input(&self, t0: std::time::Instant) {
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let mut st = self.stats.lock().unwrap();
        st.inputs += 1;
        st.input_ms_total += ms;
    }
}

/// Cheap "is this frame effectively black" test on a sampled grid.
///
/// Used only to decide whether to attach a diagnostic warning, never to alter
/// the pixels.
fn is_effectively_black(img: &RgbImage) -> bool {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return true;
    }
    let step_x = (w / 32).max(1);
    let step_y = (h / 32).max(1);
    let mut samples = 0u32;
    let mut bright = 0u32;
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            let p = img.get_pixel(x, y);
            // Anything clearly above black counts as content.
            if p[0] > 8 || p[1] > 8 || p[2] > 8 {
                bright += 1;
            }
            samples += 1;
            x += step_x;
        }
        y += step_y;
    }
    samples == 0 || (bright as f64 / samples as f64) < 0.005
}

fn region_args(args: &Value) -> Option<(i32, i32, u32, u32)> {
    match (
        arg_i64(args, "x"),
        arg_i64(args, "y"),
        arg_i64(args, "width"),
        arg_i64(args, "height"),
    ) {
        (Some(x), Some(y), Some(w), Some(h)) if w > 0 && h > 0 => {
            Some((x as i32, y as i32, w as u32, h as u32))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_requires_all_four_positive_arguments() {
        assert_eq!(
            region_args(&json!({"x": 1, "y": 2, "width": 3, "height": 4})),
            Some((1, 2, 3, 4))
        );
        assert_eq!(region_args(&json!({"x": 1, "y": 2, "width": 3})), None);
        assert_eq!(
            region_args(&json!({"x": 1, "y": 2, "width": 0, "height": 4})),
            None
        );
    }
}
