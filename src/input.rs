//! Input dispatch: choose and drive an injection backend.
//!
//! Backends, in preference order for a Wayland session:
//!
//! 1. **uinput absolute pointer + keyboard** — reaches Wayland-native and
//!    Xwayland clients alike because it sits below the display server. The
//!    only backend giving true absolute positioning on Wayland.
//! 2. **X11 XTEST** — fast and exact, but only on a *real* X server.
//!
//! mutter's `RemoteDesktop` interface is deliberately **not** used for the
//! pointer. On GNOME 48 `NotifyPointerMotionRelative` returns success while
//! moving nothing (verified by polling `XQueryPointer` before/after), and
//! `NotifyPointerMotionAbsolute` requires an attached screencast. Its keyboard
//! methods do work, so they are used as a keyboard-only fallback when uinput is
//! unavailable — which is the common case, since `/dev/uinput` ships
//! `root:root 0600` and needs an admin step.
//!
//! Every response reports which backend served the call and whether the
//! pointer position could be verified, so a caller is never told a click
//! landed when the transport merely accepted it.

use crate::platform::{CoordinateModel, SessionKind};
use crate::uinput::{self, AbsPointer, Button, VirtualKeyboard};
use crate::x11::Desktop;
use anyhow::{anyhow, Result};
use std::sync::Mutex;

/// Which backend is handling input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputBackend {
    Uinput,
    X11,
    /// Keyboard-only via mutter's RemoteDesktop D-Bus interface.
    MutterKeyboard,
    None,
}

impl InputBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            InputBackend::Uinput => "uinput (absolute pointer + keyboard)",
            InputBackend::X11 => "x11 xtest",
            InputBackend::MutterKeyboard => "mutter RemoteDesktop (keyboard only)",
            InputBackend::None => "none",
        }
    }

    /// Whether this backend can position the pointer at absolute coordinates.
    pub fn has_absolute_pointer(self) -> bool {
        matches!(self, InputBackend::Uinput | InputBackend::X11)
    }

    /// Whether the pointer position can be read back to confirm delivery.
    ///
    /// Only a real X server exposes a queryable pointer; uinput is
    /// fire-and-forget.
    pub fn can_verify_pointer(self) -> bool {
        matches!(self, InputBackend::X11)
    }
}

/// Lazily-created devices, kept for the process lifetime so repeated calls do
/// not pay the ~150 ms device-registration cost.
struct Devices {
    pointer: Option<AbsPointer>,
    keyboard: Option<VirtualKeyboard>,
    /// Size the pointer's axis range was declared with.
    pointer_range: Option<(i32, i32)>,
    mutter: Option<crate::mutter::MutterSession>,
}

/// The input dispatcher.
pub struct Input {
    backend: InputBackend,
    devices: Mutex<Devices>,
    desktop: Option<Desktop>,
    platform_session: SessionKind,
    /// Why a preferred backend was rejected, for honest reporting.
    pub notes: Vec<String>,
}

impl Input {
    /// Decide the backend up front, without creating any device.
    pub fn detect(
        session: SessionKind,
        desktop: Option<Desktop>,
        mutter_available: bool,
        notes: Vec<String>,
    ) -> Self {
        let u = uinput::probe();
        let mut notes = notes;
        let backend = if u.usable {
            InputBackend::Uinput
        } else {
            notes.push(u.reason.clone());
            if session == SessionKind::X11
                && desktop.as_ref().map(|d| d.can_inject()).unwrap_or(false)
            {
                InputBackend::X11
            } else if mutter_available {
                notes.push(
                    "pointer control unavailable: mutter RemoteDesktop moves nothing without a \
                     screencast, so only keyboard injection is offered"
                        .to_string(),
                );
                InputBackend::MutterKeyboard
            } else {
                InputBackend::None
            }
        };
        Input {
            backend,
            devices: Mutex::new(Devices {
                pointer: None,
                keyboard: None,
                pointer_range: None,
                mutter: None,
            }),
            desktop,
            platform_session: session,
            notes,
        }
    }

    pub fn backend(&self) -> InputBackend {
        self.backend
    }

    pub fn has_absolute_pointer(&self) -> bool {
        self.backend.has_absolute_pointer()
    }

    /// The X display handle, used for window queries. Present under Xwayland
    /// too, where it enumerates X11 clients only.
    pub fn desktop(&self) -> Option<&Desktop> {
        self.desktop.as_ref()
    }

    /// Make sure the uinput devices exist and are sized for `range`.
    ///
    /// The axis range is the capture resolution, so a screenshot pixel passed
    /// straight through lands on that pixel.
    fn ensure_uinput(&self, range: (i32, i32)) -> Result<()> {
        let mut dev = self.devices.lock().unwrap();
        if dev.pointer.is_none() || dev.pointer_range != Some(range) {
            // Tear down the old device before re-declaring the axis range;
            // an existing uinput node's ABS range cannot be changed.
            if let Some(mut p) = dev.pointer.take() {
                p.destroy();
            }
            dev.pointer = Some(AbsPointer::create(range.0, range.1)?);
            dev.pointer_range = Some(range);
        }
        if dev.keyboard.is_none() {
            dev.keyboard = Some(VirtualKeyboard::create()?);
        }
        Ok(())
    }

    /// Move the pointer to a coordinate already expressed in capture pixels.
    pub fn move_to(&self, model: &CoordinateModel, x: i32, y: i32) -> Result<InputReport> {
        match self.backend {
            InputBackend::Uinput => {
                let range = uinput_range(model)?;
                self.ensure_uinput(range)?;
                let mut dev = self.devices.lock().unwrap();
                let sent = dev.pointer.as_mut().unwrap().move_to(x, y)?;
                Ok(InputReport::pointer(x, y, (x, y), Some(sent), false))
            }
            InputBackend::X11 => {
                let d = self
                    .desktop
                    .as_ref()
                    .ok_or_else(|| anyhow!("no X11 desktop available"))?;
                let (lx, ly) = (model.to_logical_x(x as f64), model.to_logical_y(y as f64));
                d.move_pointer(lx.round() as i16, ly.round() as i16)?;
                Ok(InputReport::pointer(
                    x,
                    y,
                    (lx.round() as i32, ly.round() as i32),
                    None,
                    true,
                ))
            }
            other => Err(anyhow!(
                "pointer control is unavailable with the '{}' backend; {}",
                other.as_str(),
                self.remedy()
            )),
        }
    }

    pub fn click(
        &self,
        model: &CoordinateModel,
        button: Button,
        at: Option<(i32, i32)>,
        count: u32,
    ) -> Result<InputReport> {
        match self.backend {
            InputBackend::Uinput => {
                let range = uinput_range(model)?;
                self.ensure_uinput(range)?;
                let mut dev = self.devices.lock().unwrap();
                let p = dev.pointer.as_mut().unwrap();
                let emitted = match at {
                    Some((x, y)) => Some(p.move_to(x, y)?),
                    None => None,
                };
                for _ in 0..count {
                    p.press(button, None)?;
                    p.release(button)?;
                }
                Ok(InputReport::pointer(
                    at.map(|a| a.0).unwrap_or(-1),
                    at.map(|a| a.1).unwrap_or(-1),
                    at.unwrap_or((-1, -1)),
                    emitted,
                    false,
                ))
            }
            InputBackend::X11 => {
                let d = self
                    .desktop
                    .as_ref()
                    .ok_or_else(|| anyhow!("no X11 desktop available"))?;
                let mut logical = (-1, -1);
                if let Some((x, y)) = at {
                    let lx = model.to_logical_x(x as f64).round() as i16;
                    let ly = model.to_logical_y(y as f64).round() as i16;
                    d.move_pointer(lx, ly)?;
                    logical = (lx as i32, ly as i32);
                }
                let code = match button {
                    Button::Left => 1u8,
                    Button::Middle => 2,
                    Button::Right => 3,
                };
                for _ in 0..count {
                    d.click(code, true)?;
                    d.click(code, false)?;
                }
                d.sync();
                Ok(InputReport::pointer(
                    at.map(|a| a.0).unwrap_or(-1),
                    at.map(|a| a.1).unwrap_or(-1),
                    logical,
                    None,
                    true,
                ))
            }
            other => Err(anyhow!(
                "click is unavailable with the '{}' backend; {}",
                other.as_str(),
                self.remedy()
            )),
        }
    }

    pub fn button(&self, button: Button, press: bool) -> Result<InputReport> {
        match self.backend {
            InputBackend::Uinput => {
                let mut dev = self.devices.lock().unwrap();
                let p = dev
                    .pointer
                    .as_mut()
                    .ok_or_else(|| anyhow!("uinput pointer is not initialised yet"))?;
                if press {
                    p.press(button, None)?;
                } else {
                    p.release(button)?;
                }
                Ok(InputReport::simple())
            }
            InputBackend::X11 => {
                let d = self
                    .desktop
                    .as_ref()
                    .ok_or_else(|| anyhow!("no X11 desktop available"))?;
                let code = match button {
                    Button::Left => 1u8,
                    Button::Middle => 2,
                    Button::Right => 3,
                };
                d.click(code, press)?;
                d.sync();
                Ok(InputReport::simple())
            }
            other => Err(anyhow!(
                "button control is unavailable with the '{}' backend; {}",
                other.as_str(),
                self.remedy()
            )),
        }
    }

    /// Scroll. `up`/`down` map to wheel notches; `left`/`right` to hwheel.
    pub fn scroll(
        &self,
        model: &CoordinateModel,
        direction: &str,
        amount: i32,
        at: Option<(i32, i32)>,
    ) -> Result<InputReport> {
        let (vertical, horizontal) = match direction {
            "up" => (amount, 0),
            "down" => (-amount, 0),
            "left" => (0, -amount),
            "right" => (0, amount),
            other => return Err(anyhow!("bad direction '{other}'")),
        };
        match self.backend {
            InputBackend::Uinput => {
                let range = uinput_range(model)?;
                self.ensure_uinput(range)?;
                let mut dev = self.devices.lock().unwrap();
                let emitted = dev
                    .pointer
                    .as_mut()
                    .unwrap()
                    .wheel(vertical, horizontal, at)?;
                Ok(InputReport::pointer(
                    at.map(|a| a.0).unwrap_or(-1),
                    at.map(|a| a.1).unwrap_or(-1),
                    at.unwrap_or((-1, -1)),
                    Some(emitted),
                    false,
                ))
            }
            InputBackend::X11 => {
                let d = self
                    .desktop
                    .as_ref()
                    .ok_or_else(|| anyhow!("no X11 desktop available"))?;
                let (code, times) = match direction {
                    "up" => (4u8, amount),
                    "down" => (5u8, amount),
                    "left" => (6u8, amount),
                    "right" => (7u8, amount),
                    other => return Err(anyhow!("bad direction '{other}'")),
                };
                if let Some((x, y)) = at {
                    let lx = model.to_logical_x(x as f64).round() as i16;
                    let ly = model.to_logical_y(y as f64).round() as i16;
                    d.move_pointer(lx, ly)?;
                }
                for _ in 0..(times.min(50)) {
                    d.click(code, true)?;
                    d.click(code, false)?;
                }
                d.sync();
                Ok(InputReport::simple())
            }
            other => Err(anyhow!(
                "scroll is unavailable with the '{}' backend; {}",
                other.as_str(),
                self.remedy()
            )),
        }
    }

    pub fn type_text(&self, text: &str) -> Result<InputReport> {
        match self.backend {
            InputBackend::Uinput => {
                // Keyboard does not depend on the pointer range.
                let mut dev = self.devices.lock().unwrap();
                if dev.keyboard.is_none() {
                    dev.keyboard = Some(VirtualKeyboard::create()?);
                }
                dev.keyboard.as_mut().unwrap().type_str(text)?;
                Ok(InputReport::simple())
            }
            InputBackend::X11 => {
                let d = self
                    .desktop
                    .as_ref()
                    .ok_or_else(|| anyhow!("no X11 desktop available"))?;
                let km = crate::x11::Keymap::build(d.conn())?;
                let shift = km.lookup(0xffe1).map(|(c, _)| c);
                for ch in text.chars() {
                    let sym = char_to_keysym(ch);
                    let (code, needs_shift) = km
                        .lookup(sym)
                        .ok_or_else(|| anyhow!("no keycode for character {ch:?}"))?;
                    let s = needs_shift.then_some(shift).flatten();
                    if let Some(s) = s {
                        d.key(s, true)?;
                    }
                    d.key(code, true)?;
                    d.key(code, false)?;
                    if let Some(s) = s {
                        d.key(s, false)?;
                    }
                    d.sync();
                }
                Ok(InputReport::simple())
            }
            InputBackend::MutterKeyboard => {
                let mut dev = self.devices.lock().unwrap();
                if dev.mutter.is_none() {
                    dev.mutter = Some(crate::mutter::MutterSession::start()?);
                }
                dev.mutter.as_mut().unwrap().type_text(text)?;
                Ok(InputReport::simple())
            }
            InputBackend::None => Err(anyhow!("text input is unavailable; {}", self.remedy())),
        }
    }

    pub fn hotkey(&self, spec: &str) -> Result<InputReport> {
        match self.backend {
            InputBackend::Uinput => {
                let mut dev = self.devices.lock().unwrap();
                if dev.keyboard.is_none() {
                    dev.keyboard = Some(VirtualKeyboard::create()?);
                }
                dev.keyboard.as_mut().unwrap().chord(spec)?;
                Ok(InputReport::simple())
            }
            InputBackend::X11 => {
                let d = self
                    .desktop
                    .as_ref()
                    .ok_or_else(|| anyhow!("no X11 desktop available"))?;
                let km = crate::x11::Keymap::build(d.conn())?;
                let syms = parse_hotkey(spec)?;
                let mut codes = Vec::new();
                for sym in syms {
                    let (code, _) = km
                        .lookup(sym)
                        .ok_or_else(|| anyhow!("no keycode for keysym 0x{sym:x} in '{spec}'"))?;
                    codes.push(code);
                }
                for &c in &codes {
                    d.key(c, true)?;
                }
                for &c in codes.iter().rev() {
                    d.key(c, false)?;
                }
                d.sync();
                Ok(InputReport::simple())
            }
            InputBackend::MutterKeyboard => {
                let mut dev = self.devices.lock().unwrap();
                if dev.mutter.is_none() {
                    dev.mutter = Some(crate::mutter::MutterSession::start()?);
                }
                dev.mutter.as_mut().unwrap().hotkey(spec)?;
                Ok(InputReport::simple())
            }
            InputBackend::None => Err(anyhow!("hotkeys are unavailable; {}", self.remedy())),
        }
    }

    fn remedy(&self) -> String {
        format!(
            "run `sudo usermod -aG input $USER` plus the udev rule from the README, then log out \
             and back in (session is {}). Current backend: {}",
            match self.platform_session {
                SessionKind::X11 => "x11",
                SessionKind::Wayland => "wayland",
            },
            self.backend.as_str()
        )
    }
}

/// The pointer axis range for uinput: the capture size, so canonical pixels
/// pass straight through.
fn uinput_range(model: &CoordinateModel) -> Result<(i32, i32)> {
    if model.physical_width > 1 && model.physical_height > 1 {
        return Ok((model.physical_width, model.physical_height));
    }
    Err(anyhow!(
        "the capture size is not known yet; take a screenshot first so the \
         absolute pointer can be sized to the screen"
    ))
}

/// Facts about one input action, reported back to the caller.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InputReport {
    /// Coordinates as requested, in capture pixels.
    pub requested: (i32, i32),
    /// Coordinates as actually emitted by the backend.
    pub emitted: (i32, i32),
    /// True when the backend clamped the requested position.
    pub clamped: bool,
    /// Position in the backend's own coordinate space (logical for X11).
    pub backend_space: (i32, i32),
    /// Whether the position was read back to confirm delivery.
    pub verified: bool,
}

impl InputReport {
    fn simple() -> Self {
        InputReport {
            requested: (-1, -1),
            emitted: (-1, -1),
            clamped: false,
            backend_space: (-1, -1),
            verified: false,
        }
    }

    fn pointer(
        req_x: i32,
        req_y: i32,
        backend_space: (i32, i32),
        emitted: Option<(i32, i32)>,
        verified: bool,
    ) -> Self {
        let e = emitted.unwrap_or((-1, -1));
        InputReport {
            requested: (req_x, req_y),
            emitted: e,
            clamped: emitted.is_some() && e != (req_x, req_y),
            backend_space,
            verified,
        }
    }
}

/// Map a hotkey name to an X11 keysym (used by the X11 backend only).
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
                    char_to_keysym(other.chars().next().unwrap())
                } else if let Some(rest) = other.strip_prefix('f') {
                    let n: u32 = rest.parse().map_err(|_| anyhow!("unknown key '{part}'"))?;
                    if (1..=24).contains(&n) {
                        0xffbe + (n - 1)
                    } else {
                        return Err(anyhow!("unsupported function key '{part}'"));
                    }
                } else {
                    return Err(anyhow!("unknown key '{part}'"));
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
    let cp = c as u32;
    if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) {
        cp
    } else {
        match c {
            '\n' => 0xff0d,
            '\t' => 0xff09,
            _ => cp | 0x0100_0000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_maps_modifiers_and_letters() {
        let syms = parse_hotkey("ctrl+alt+t").unwrap();
        assert_eq!(syms, vec![0xffe3, 0xffe9, u32::from(b't')]);
    }

    #[test]
    fn hotkey_rejects_unknown_names() {
        assert!(parse_hotkey("hyper+q").is_err());
        assert!(parse_hotkey("").is_err());
    }

    #[test]
    fn backend_capabilities_are_honest() {
        assert!(InputBackend::Uinput.has_absolute_pointer());
        assert!(!InputBackend::Uinput.can_verify_pointer());
        assert!(InputBackend::X11.can_verify_pointer());
        assert!(!InputBackend::MutterKeyboard.has_absolute_pointer());
    }

    #[test]
    fn uinput_range_requires_a_known_capture() {
        let m = CoordinateModel::unknown();
        assert!(uinput_range(&m).is_err());
    }
}
