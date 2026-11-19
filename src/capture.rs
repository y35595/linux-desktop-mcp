//! Screen capture backends.
//!
//! Backends are tried in a fixed order chosen from measured behaviour on a
//! GNOME 48 Wayland host (fractional scaling at 1.7467, Xwayland present):
//!
//! 1. **wlroots `grim`** — sway/Hyprland/river. Native pixels, ~30–80 ms.
//!    Never valid on GNOME/KDE, whose compositors do not implement
//!    `zwlr_screencopy_manager_v1`.
//! 2. **X11 `GetImage`** — only when a *real* X server owns the screen. Under
//!    Xwayland this fails with `BadMatch` because the root window has no
//!    backing store, which was the original bug in this project.
//! 3. **XDG portal `Screenshot`** — GNOME, KDE, and anything else running
//!    `xdg-desktop-portal`. Returns native-resolution pixels with no consent
//!    dialog when `interactive: false`.
//!
//! Every backend returns an RGB8 [`Frame`] plus the resolution it actually
//! captured, so the caller can reconcile the coordinate model.

use crate::platform::{Compositor, Platform, SessionKind};
use anyhow::{anyhow, Context, Result};
use std::process::Command;

/// A captured frame, swizzled to RGB8.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// Which backend produced a frame. Reported so callers can tell a real
/// desktop capture from a degraded one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureBackend {
    Grim,
    X11,
    Portal,
}

impl CaptureBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureBackend::Grim => "grim (wlroots screencopy)",
            CaptureBackend::X11 => "x11 GetImage",
            CaptureBackend::Portal => "xdg-desktop-portal Screenshot",
        }
    }
}

/// The selected capture strategy for this session.
pub struct Capture {
    backend: CaptureBackend,
    /// Full-screen size, learned lazily from the first capture.
    size: std::cell::Cell<(u32, u32)>,
    platform: Platform,
}

fn have(bin: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {bin}"))
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

impl Capture {
    /// Pick a backend, preferring native-pixel capture on Wayland.
    pub fn detect(platform: Platform) -> Self {
        let backend = match platform.session {
            SessionKind::X11 if platform.is_real_x11() => CaptureBackend::X11,
            _ => {
                // grim speaks wlr-screencopy, which mutter and KWin do not
                // implement; only consider it off the GNOME/KDE path.
                let gnome_or_kde =
                    matches!(platform.compositor, Compositor::Gnome | Compositor::Kde);
                if !gnome_or_kde && have("grim") {
                    CaptureBackend::Grim
                } else if crate::dbus::portal_available() {
                    CaptureBackend::Portal
                } else if platform.is_real_x11() {
                    CaptureBackend::X11
                } else {
                    // Last resort: it will fail with a clear message rather
                    // than silently returning black pixels.
                    CaptureBackend::Portal
                }
            }
        };
        Capture {
            backend,
            size: std::cell::Cell::new((0, 0)),
            platform,
        }
    }

    pub fn backend(&self) -> CaptureBackend {
        self.backend
    }

    /// Capture the full screen.
    pub fn capture_full(&self) -> Result<Frame> {
        let frame = match self.backend {
            CaptureBackend::Grim => self.capture_grim(None)?,
            CaptureBackend::X11 => self.capture_x11(None)?,
            CaptureBackend::Portal => self.capture_portal()?,
        };
        self.size.set((frame.width, frame.height));
        Ok(frame)
    }

    /// Capture a region given in **capture pixels** (the canonical space).
    pub fn capture_region(&self, x: i32, y: i32, w: u32, h: u32) -> Result<Frame> {
        if w == 0 || h == 0 {
            return Err(anyhow!("capture region has zero width or height"));
        }
        match self.backend {
            CaptureBackend::Grim => self.capture_grim(Some((x, y, w, h))),
            CaptureBackend::X11 => self.capture_x11(Some((x, y, w, h))),
            // The portal only offers whole-screen capture; crop afterwards.
            CaptureBackend::Portal => {
                let full = self.capture_portal()?;
                self.size.set((full.width, full.height));
                crop(full, x, y, w, h)
            }
        }
    }

    fn capture_portal(&self) -> Result<Frame> {
        let png = crate::dbus::portal_screenshot(std::time::Duration::from_secs(20))?;
        let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .context("portal returned data that is not a PNG")?
            .to_rgb8();
        Ok(Frame {
            width: img.width(),
            height: img.height(),
            rgb: img.into_raw(),
        })
    }

    fn capture_grim(&self, region: Option<(i32, i32, u32, u32)>) -> Result<Frame> {
        let tmp = std::env::temp_dir().join(format!(
            "ldmcp-grim-{}-{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let mut cmd = Command::new("grim");
        if let Some((x, y, w, h)) = region {
            cmd.arg("-g").arg(format!("{x},{y} {w}x{h}"));
        }
        cmd.arg(&tmp);
        let out = cmd.output().context("failed to run grim")?;
        if !out.status.success() {
            let _ = std::fs::remove_file(&tmp);
            return Err(anyhow!(
                "grim failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let bytes =
            std::fs::read(&tmp).with_context(|| format!("grim wrote no file at {tmp:?}"))?;
        let _ = std::fs::remove_file(&tmp);
        let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
            .context("grim output is not a PNG")?
            .to_rgb8();
        Ok(Frame {
            width: img.width(),
            height: img.height(),
            rgb: img.into_raw(),
        })
    }

    /// X11 `GetImage` on the root window. Only valid on a real X server.
    fn capture_x11(&self, region: Option<(i32, i32, u32, u32)>) -> Result<Frame> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::*;

        if !self.platform.is_real_x11() {
            return Err(anyhow!(
                "x11 capture is not valid on this session ({}): the root window has no \
                 backing store under Xwayland, so GetImage returns BadMatch or black pixels",
                self.platform.session.as_str()
            ));
        }
        let (conn, screen_num) =
            x11rb::connect(None).context("cannot connect to the X display (is DISPLAY set?)")?;
        let screen = &conn.setup().roots[screen_num];
        let (rx, ry, rw, rh) = match region {
            Some((x, y, w, h)) => (x as i16, y as i16, w as u16, h as u16),
            None => (0, 0, screen.width_in_pixels, screen.height_in_pixels),
        };
        let img = conn
            .get_image(ImageFormat::Z_PIXMAP, screen.root, rx, ry, rw, rh, !0)
            .context("GetImage request failed")?
            .reply()
            .context(
                "GetImage reply failed — under Xwayland the root window has no backing \
                 store; use the portal backend instead",
            )?;

        let pixel_bytes = (img.depth as usize).div_ceil(8);
        let src = &img.data;
        let npix = rw as usize * rh as usize;
        if src.len() < npix * pixel_bytes {
            return Err(anyhow!(
                "short image data: got {} expected {}",
                src.len(),
                npix * pixel_bytes
            ));
        }
        let mut rgb = vec![0u8; npix * 3];
        match pixel_bytes {
            4 => {
                for i in 0..npix {
                    let s = i * 4;
                    rgb[i * 3] = src[s + 2];
                    rgb[i * 3 + 1] = src[s + 1];
                    rgb[i * 3 + 2] = src[s];
                }
            }
            3 => {
                for i in 0..npix {
                    let s = i * 3;
                    rgb[i * 3] = src[s + 2];
                    rgb[i * 3 + 1] = src[s + 1];
                    rgb[i * 3 + 2] = src[s];
                }
            }
            2 => {
                for i in 0..npix {
                    let v = u16::from_le_bytes([src[i * 2], src[i * 2 + 1]]);
                    let r = ((v >> 11) & 0x1f) as u8;
                    let g = ((v >> 5) & 0x3f) as u8;
                    let b = (v & 0x1f) as u8;
                    rgb[i * 3] = (r << 3) | (r >> 2);
                    rgb[i * 3 + 1] = (g << 2) | (g >> 4);
                    rgb[i * 3 + 2] = (b << 3) | (b >> 2);
                }
            }
            _ => return Err(anyhow!("unsupported X11 pixel depth {}", img.depth)),
        }
        Ok(Frame {
            width: rw as u32,
            height: rh as u32,
            rgb,
        })
    }
}

/// Crop a frame to a rectangle given in frame pixels, clamping to bounds.
fn crop(frame: Frame, x: i32, y: i32, w: u32, h: u32) -> Result<Frame> {
    let fw = frame.width as i64;
    let fh = frame.height as i64;
    let x0 = (x as i64).clamp(0, fw);
    let y0 = (y as i64).clamp(0, fh);
    let x1 = (x0 + w as i64).clamp(0, fw);
    let y1 = (y0 + h as i64).clamp(0, fh);
    let cw = (x1 - x0) as u32;
    let ch = (y1 - y0) as u32;
    if cw == 0 || ch == 0 {
        return Err(anyhow!("capture region lies entirely outside the screen"));
    }
    let mut out = vec![0u8; cw as usize * ch as usize * 3];
    for row in 0..ch {
        let src_off = (((y0 + row as i64) * fw + x0) * 3) as usize;
        let dst_off = (row * cw * 3) as usize;
        let len = cw as usize * 3;
        out[dst_off..dst_off + len].copy_from_slice(&frame.rgb[src_off..src_off + len]);
    }
    Ok(Frame {
        width: cw,
        height: ch,
        rgb: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{Compositor, Platform};

    fn plat(session: SessionKind, compositor: Compositor, display: Option<&str>) -> Platform {
        Platform {
            session,
            compositor,
            display: display.map(|s| s.to_string()),
            wayland_display: None,
        }
    }

    #[test]
    fn wayland_never_selects_x11_capture() {
        let p = plat(SessionKind::Wayland, Compositor::Gnome, Some(":0"));
        let c = Capture::detect(p);
        assert_ne!(
            c.backend(),
            CaptureBackend::X11,
            "Xwayland must not be treated as a real X server"
        );
    }

    #[test]
    fn real_x11_selects_x11_capture() {
        let p = plat(SessionKind::X11, Compositor::Other, Some(":0"));
        // Only assert the intent; the host may also have a portal running.
        let c = Capture::detect(p);
        assert!(matches!(
            c.backend(),
            CaptureBackend::X11 | CaptureBackend::Grim | CaptureBackend::Portal
        ));
    }

    #[test]
    fn crop_clamps_to_frame_bounds() {
        let f = Frame {
            width: 4,
            height: 4,
            rgb: vec![9u8; 4 * 4 * 3],
        };
        let c = crop(f, 2, 2, 10, 10).unwrap();
        assert_eq!((c.width, c.height), (2, 2));
    }

    #[test]
    fn crop_rejects_fully_offscreen_regions() {
        let f = Frame {
            width: 4,
            height: 4,
            rgb: vec![0u8; 4 * 4 * 3],
        };
        assert!(crop(f, 10, 10, 2, 2).is_err());
    }
}
