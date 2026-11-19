//! X11 backend: input synthesis and window queries.
//!
//! This module is only usable when a **real** X server owns the screen. Under
//! Xwayland `DISPLAY` is set too, but:
//!
//! - `GetImage` on the root window fails with `BadMatch` (no backing store),
//! - XTEST events reach Xwayland clients but not the compositor's own UI —
//!   verified on GNOME 48 by sending `Super` through XTEST (the overview did
//!   not open) and through mutter (it did).
//!
//! Window queries are still useful under Xwayland because they enumerate
//! Xwayland *clients* correctly, so [`Desktop::list_windows`] is available on
//! both, while input is gated behind [`Desktop::can_inject`].

use anyhow::{anyhow, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::xtest::ConnectionExt as XtestExt;
use x11rb::rust_connection::RustConnection;

/// Handle to the X display, owning one connection for the process lifetime.
pub struct Desktop {
    conn: RustConnection,
    root: Window,
    /// True only when a real X server owns the screen.
    real_x11: bool,
}

/// Keysym -> keycode cache, built once to avoid a round trip per keystroke.
pub struct Keymap {
    /// index = keycode - min_keycode; value = keysym at level 0 and level 1.
    symbols: Vec<[u32; 2]>,
    min_keycode: u8,
}

impl Keymap {
    pub fn build(conn: &RustConnection) -> Result<Self> {
        let setup = conn.setup();
        let min = setup.min_keycode;
        let max = setup.max_keycode;
        let count = max.saturating_sub(min) + 1;
        let reply = conn
            .get_keyboard_mapping(min, count)
            .context("GetKeyboardMapping failed")?
            .reply()
            .context("GetKeyboardMapping reply failed")?;
        let per = reply.keysyms_per_keycode as usize;
        let mut symbols = vec![[0u32; 2]; count as usize];
        for (i, slot) in symbols.iter_mut().enumerate() {
            let base = i * per;
            if base < reply.keysyms.len() {
                slot[0] = reply.keysyms[base];
                if per > 1 && base + 1 < reply.keysyms.len() {
                    slot[1] = reply.keysyms[base + 1];
                }
            }
        }
        Ok(Keymap {
            symbols,
            min_keycode: min,
        })
    }

    /// Find (keycode, needs_shift) for a keysym.
    pub fn lookup(&self, keysym: u32) -> Option<(u8, bool)> {
        for (i, syms) in self.symbols.iter().enumerate() {
            if syms[0] == keysym {
                return Some((i as u8 + self.min_keycode, false));
            }
            if syms[1] == keysym {
                return Some((i as u8 + self.min_keycode, true));
            }
        }
        None
    }
}

impl Desktop {
    pub fn connect(real_x11: bool) -> Result<Self> {
        let (conn, screen_num) =
            x11rb::connect(None).context("cannot connect to X display (is DISPLAY set?)")?;
        let screen = &conn.setup().roots[screen_num];
        Ok(Desktop {
            root: screen.root,
            conn,
            real_x11,
        })
    }

    pub fn conn(&self) -> &RustConnection {
        &self.conn
    }

    /// XTEST is only meaningful when a real X server owns the screen.
    pub fn can_inject(&self) -> bool {
        self.real_x11
    }

    fn require_injectable(&self) -> Result<()> {
        if self.real_x11 {
            Ok(())
        } else {
            Err(anyhow!(
                "XTEST input is not available on a Wayland session: it reaches Xwayland \
                 clients only, never the compositor's own UI. Configure /dev/uinput \
                 (see README) so the uinput backend can inject input instead."
            ))
        }
    }

    pub fn move_pointer(&self, x: i16, y: i16) -> Result<()> {
        self.require_injectable()?;
        self.conn
            .xtest_fake_input(MOTION_NOTIFY_EVENT, 0, 0, self.root, x, y, 0)
            .context("XTEST motion failed")?;
        self.conn.flush().ok();
        Ok(())
    }

    pub fn click(&self, button: u8, press: bool) -> Result<()> {
        self.require_injectable()?;
        self.conn
            .xtest_fake_input(
                if press {
                    BUTTON_PRESS_EVENT
                } else {
                    BUTTON_RELEASE_EVENT
                },
                button,
                0,
                self.root,
                0,
                0,
                0,
            )
            .context("XTEST button failed")?;
        self.conn.flush().ok();
        Ok(())
    }

    pub fn key(&self, keycode: u8, press: bool) -> Result<()> {
        self.require_injectable()?;
        self.conn
            .xtest_fake_input(
                if press {
                    KEY_PRESS_EVENT
                } else {
                    KEY_RELEASE_EVENT
                },
                keycode,
                0,
                self.root,
                0,
                0,
                0,
            )
            .context("XTEST key failed")?;
        self.conn.flush().ok();
        Ok(())
    }

    pub fn sync(&self) {
        self.conn.flush().ok();
    }

    // ---- window queries ----

    /// List user-visible top-level windows.
    ///
    /// Under Xwayland this enumerates Xwayland clients only — native Wayland
    /// windows are invisible to X11 by design, which is why the JSON output
    /// carries a warning when the session is Wayland.
    pub fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        let mut ids = self.client_list(b"_NET_CLIENT_LIST").unwrap_or_default();
        if ids.is_empty() {
            ids = self
                .client_list(b"_NET_CLIENT_LIST_STACKING")
                .unwrap_or_default();
        }
        if ids.is_empty() {
            ids = self.conn.query_tree(self.root)?.reply()?.children;
        }
        let mut out: Vec<WindowInfo> = ids
            .into_iter()
            .filter_map(|w| self.window_info(w))
            .filter(|i| i.mapped && i.width >= 80 && i.height >= 60)
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out.dedup_by(|a, b| a.id == b.id);
        Ok(out)
    }

    fn client_list(&self, name: &[u8]) -> Option<Vec<Window>> {
        let atom = self.conn.intern_atom(false, name).ok()?.reply().ok()?.atom;
        let prop = self
            .conn
            .get_property(false, self.root, atom, AtomEnum::WINDOW, 0, 1024)
            .ok()?
            .reply()
            .ok()?;
        let mut out = Vec::new();
        for chunk in prop.value.as_chunks::<4>().0 {
            out.push(u32::from_le_bytes(*chunk));
        }
        Some(out)
    }

    pub fn window_info(&self, win: Window) -> Option<WindowInfo> {
        let geom = self.conn.get_geometry(win).ok()?.reply().ok()?;
        let attrs = self.conn.get_window_attributes(win).ok()?.reply().ok()?;
        let name = self
            .net_wm_name(win)
            .or_else(|| self.wm_name(win))
            .or_else(|| self.wm_class(win))
            .unwrap_or_else(|| String::from("(untitled)"));
        let (x, y) = self.absolute_xy(win).unwrap_or((0, 0));
        Some(WindowInfo {
            id: win,
            name,
            x,
            y,
            width: geom.width,
            height: geom.height,
            mapped: attrs.map_state == MapState::VIEWABLE,
        })
    }

    fn absolute_xy(&self, win: Window) -> Option<(i16, i16)> {
        let t = self
            .conn
            .translate_coordinates(win, self.root, 0, 0)
            .ok()?
            .reply()
            .ok()?;
        Some((t.dst_x, t.dst_y))
    }

    fn net_wm_name(&self, win: Window) -> Option<String> {
        let atom = self
            .conn
            .intern_atom(false, b"_NET_WM_NAME")
            .ok()?
            .reply()
            .ok()?
            .atom;
        let prop = self
            .conn
            .get_property(false, win, atom, AtomEnum::ANY, 0, 1024)
            .ok()?
            .reply()
            .ok()?;
        if prop.value.is_empty() {
            return None;
        }
        String::from_utf8(prop.value).ok().filter(|s| !s.is_empty())
    }

    fn wm_name(&self, win: Window) -> Option<String> {
        let prop = self
            .conn
            .get_property(false, win, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 1024)
            .ok()?
            .reply()
            .ok()?;
        if prop.value.is_empty() {
            return None;
        }
        String::from_utf8(prop.value).ok().filter(|s| !s.is_empty())
    }

    fn wm_class(&self, win: Window) -> Option<String> {
        let prop = self
            .conn
            .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
            .ok()?
            .reply()
            .ok()?;
        if prop.value.is_empty() {
            return None;
        }
        let parts: Vec<&[u8]> = prop
            .value
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .collect();
        parts
            .last()
            .and_then(|p| String::from_utf8(p.to_vec()).ok())
            .filter(|s| !s.is_empty())
    }

    pub fn active_window(&self) -> Option<WindowInfo> {
        let atom = self
            .conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")
            .ok()?
            .reply()
            .ok()?
            .atom;
        let prop = self
            .conn
            .get_property(false, self.root, atom, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        if prop.value.len() < 4 {
            return None;
        }
        let win = u32::from_ne_bytes([prop.value[0], prop.value[1], prop.value[2], prop.value[3]]);
        self.window_info(win)
    }

    pub fn activate(&self, win: Window) -> Result<()> {
        let atom = self
            .conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")
            .context("intern _NET_ACTIVE_WINDOW failed")?
            .reply()
            .context("intern reply failed")?
            .atom;
        let data = [1u32, 0, 0, 0, 0];
        let ev = ClientMessageEvent::new(32, win, atom, data);
        let mask = EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY;
        self.conn
            .send_event(false, self.root, mask, ev)
            .context("send _NET_ACTIVE_WINDOW failed")?;
        self.conn.flush().ok();
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WindowInfo {
    pub id: u32,
    pub name: String,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub mapped: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_min_keycode_is_used_as_the_base() {
        // The old implementation hard-coded 8; this guards against a regression
        // on servers whose min_keycode differs.
        let km = Keymap {
            symbols: vec![[0u32; 2]; 4],
            min_keycode: 12,
        };
        assert_eq!(km.lookup(0x41), None);
    }
}
