//! Session / compositor detection and the coordinate-space model.
//!
//! ## Why this module exists
//!
//! On a Wayland session the desktop is exposed through several *different*
//! coordinate spaces, and mixing them up silently misplaces every click:
//!
//! | source                       | space                     | this host |
//! |------------------------------|---------------------------|-----------|
//! | X11 / Xwayland root window   | logical                   | 1832x1145 |
//! | portal `Screenshot` output   | physical (native pixels)  | 3200x2000 |
//! | mutter `DisplayConfig`       | logical + `scale`          | 1.7467    |
//! | uinput ABS pointer           | whatever range we declare | our choice|
//!
//! The original X11-only implementation reported the Xwayland root geometry
//! (1832x1145) and then tried to `GetImage` the same root window — which fails
//! with `BadMatch` under Xwayland because the root window has no backing store.
//!
//! ## The invariant this crate keeps
//!
//! **Screenshot pixels are the canonical coordinate space for all input
//! tools.** If a caller reads pixel `(u, v)` off a native-resolution
//! screenshot and passes it to `click`, it lands on that exact pixel. Each
//! input backend converts that canonical coordinate into whatever it needs:
//!
//! - uinput absolute pointer: declares `ABS_X/ABS_Y` range = capture size, so
//!   the canonical coordinate passes straight through.
//! - X11 XTEST (real X11 only): the capture came from the X11 root, so it is
//!   already the same space.
//! - mutter RemoteDesktop: divides by `scale` to reach logical coordinates.

use std::env;

/// Which display server owns the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// A real X11 server owns the screen; XTEST and `GetImage` both work.
    X11,
    /// A Wayland compositor owns the screen. `DISPLAY` may still be set
    /// because Xwayland is running, but X11 capture/input is unreliable.
    Wayland,
}

/// The compositor, as far as we can tell from environment variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compositor {
    Gnome,
    Kde,
    /// sway, Hyprland, river, Wayfire, labwc, … (wlr-screencopy family).
    Wlroots,
    Other,
}

impl Compositor {
    pub fn as_str(self) -> &'static str {
        match self {
            Compositor::Gnome => "gnome",
            Compositor::Kde => "kde",
            Compositor::Wlroots => "wlroots",
            Compositor::Other => "other",
        }
    }
}

impl SessionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionKind::X11 => "x11",
            SessionKind::Wayland => "wayland",
        }
    }
}

/// Probe result for the current process environment.
#[derive(Debug, Clone)]
pub struct Platform {
    pub session: SessionKind,
    pub compositor: Compositor,
    pub display: Option<String>,
    pub wayland_display: Option<String>,
}

fn env_nonempty(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.trim().is_empty())
}

impl Platform {
    pub fn detect() -> Self {
        let wayland_display = env_nonempty("WAYLAND_DISPLAY");
        let display = env_nonempty("DISPLAY");
        let declared = env_nonempty("XDG_SESSION_TYPE").map(|v| v.to_ascii_lowercase());

        // A present WAYLAND_DISPLAY is the strongest signal: Xwayland also sets
        // DISPLAY, so trusting DISPLAY alone would mis-classify Wayland as X11.
        let session = match (declared.as_deref(), wayland_display.is_some()) {
            (Some("wayland"), _) => SessionKind::Wayland,
            (_, true) => SessionKind::Wayland,
            (Some("x11"), _) => SessionKind::X11,
            _ => {
                if display.is_some() {
                    SessionKind::X11
                } else {
                    SessionKind::Wayland
                }
            }
        };

        let desktop = env_nonempty("XDG_CURRENT_DESKTOP")
            .or_else(|| env_nonempty("XDG_SESSION_DESKTOP"))
            .or_else(|| env_nonempty("DESKTOP_SESSION"))
            .unwrap_or_default()
            .to_ascii_lowercase();

        let compositor = if desktop.contains("gnome") || desktop.contains("unity") {
            Compositor::Gnome
        } else if desktop.contains("kde") || desktop.contains("plasma") {
            Compositor::Kde
        } else if desktop.contains("sway")
            || desktop.contains("hyprland")
            || desktop.contains("river")
            || desktop.contains("wayfire")
            || desktop.contains("labwc")
        {
            Compositor::Wlroots
        } else {
            Compositor::Other
        };

        Platform {
            session,
            compositor,
            display,
            wayland_display,
        }
    }

    /// True only when a real X11 server owns the screen.
    ///
    /// This is deliberately distinct from "DISPLAY is set": on Wayland,
    /// Xwayland sets DISPLAY but its root window has no retirable backing
    /// store, so `GetImage` returns `BadMatch` and XTEST keystrokes do not
    /// reach the compositor's own UI.
    pub fn is_real_x11(&self) -> bool {
        self.session == SessionKind::X11 && self.display.is_some()
    }
}

/// Geometry of one logical monitor, as reported by the compositor.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LogicalMonitor {
    pub connector: String,
    pub x: i32,
    pub y: i32,
    /// Size in logical (input) coordinates.
    pub width: i32,
    pub height: i32,
    pub scale: f64,
    pub primary: bool,
}

/// The overall coordinate model of the desktop.
///
/// `scale` converts logical (input) coordinates to physical (capture) pixels:
/// `physical = logical * scale`.
#[derive(Debug, Clone, Default)]
pub struct CoordinateModel {
    /// Native capture size in pixels. This is the canonical input space.
    pub physical_width: i32,
    pub physical_height: i32,
    /// Logical size. Equals the physical size when `scale == 1.0`.
    pub logical_width: i32,
    pub logical_height: i32,
    pub scale: f64,
    /// Where the scale figure came from, for honest reporting.
    pub scale_source: &'static str,
    pub monitors: Vec<LogicalMonitor>,
}

impl CoordinateModel {
    pub fn unknown() -> Self {
        CoordinateModel {
            scale: 1.0,
            scale_source: "assumed (no compositor geometry available)",
            ..Default::default()
        }
    }

    /// Convert a canonical (capture-pixel) x coordinate into logical input space.
    pub fn to_logical_x(&self, x: f64) -> f64 {
        if self.scale > 0.0 {
            x / self.scale
        } else {
            x
        }
    }

    pub fn to_logical_y(&self, y: f64) -> f64 {
        if self.scale > 0.0 {
            y / self.scale
        } else {
            y
        }
    }

    /// Rebuild the model once the true capture size is known.
    ///
    /// The compositor may report a logical size but we only learn the true
    /// physical size by capturing. When the capture is larger than the logical
    /// size we can derive the effective scale exactly.
    pub fn reconcile_with_capture(&mut self, capture_w: i32, capture_h: i32) {
        self.physical_width = capture_w;
        self.physical_height = capture_h;
        if self.logical_width > 0 && capture_w > 0 {
            let derived = capture_w as f64 / self.logical_width as f64;
            // Only trust a derivation that is close to a plausible scale;
            // otherwise keep what the compositor told us.
            if (derived - 1.0).abs() > 0.005
                && (0.5..=4.0).contains(&derived)
                && (self.scale - 1.0).abs() <= 0.005
            {
                self.scale = derived;
                self.scale_source = "derived (capture width / logical width)";
            }
        }
        if self.logical_width == 0 {
            self.logical_width = (capture_w as f64 / self.scale.max(0.01)).round() as i32;
            self.logical_height = (capture_h as f64 / self.scale.max(0.01)).round() as i32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_converts_physical_to_logical() {
        let m = CoordinateModel {
            physical_width: 3200,
            physical_height: 2000,
            logical_width: 1832,
            logical_height: 1145,
            scale: 1.7467248439788818,
            ..Default::default()
        };
        assert!((m.to_logical_x(3200.0) - 1832.0).abs() < 1.0);
        assert!((m.to_logical_y(2000.0) - 1145.0).abs() < 1.0);
    }

    #[test]
    fn reconcile_derives_scale_when_unknown() {
        let mut m = CoordinateModel {
            logical_width: 1832,
            logical_height: 1145,
            scale: 1.0,
            ..Default::default()
        };
        m.reconcile_with_capture(3200, 2000);
        assert!((m.scale - 1.7467).abs() < 0.001);
        assert_eq!(m.physical_width, 3200);
    }

    #[test]
    fn reconcile_keeps_authoritative_scale() {
        let mut m = CoordinateModel {
            logical_width: 1832,
            logical_height: 1145,
            scale: 1.5,
            scale_source: "compositor",
            ..Default::default()
        };
        m.reconcile_with_capture(3200, 2000);
        assert_eq!(
            m.scale, 1.5,
            "a known compositor scale must not be overwritten"
        );
    }
}
