//! X11 backend: screen capture + input synthesis + window queries.
//!
//! Design notes (performance-critical):
//! - One long-lived `X11` connection reused by every call. `RUST_LOG` aside,
//!   reconnecting per call would dominate latency.
//! - Capture uses `GetImage` + raw BGRA->RGB swizzle; no subprocess, no libpng
//!   round trip.
//! - Input uses XTEST `fake_input`, which is indistinguishable from physical
//!   input and needs no focus manipulation (we never call `set_input_focus`).

use anyhow::{anyhow, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::xtest::ConnectionExt as XtestExt;
use x11rb::rust_connection::RustConnection;

/// Geometry of one screen in pixels.
#[derive(Debug, Clone, Copy)]
pub struct ScreenGeom {
    pub width: u16,
    pub height: u16,
}

/// A captured frame, already swizzled to RGB8.
pub struct Frame {
    pub width: u16,
    pub height: u16,
    pub rgb: Vec<u8>,
}

/// Handle to the display, owning one connection for the process lifetime.
pub struct Desktop {
    conn: RustConnection,
    root: Window,
    geom: ScreenGeom,
}

/// Keysym -> keycode cache, built once to avoid a round trip per keystroke.
pub struct Keymap {
    /// index = keycode, value = keysym at level 0 and level 1 (shifted).
    symbols: Vec<[u32; 2]>,
}

impl Keymap {
    pub fn build(conn: &RustConnection) -> Result<Self> {
        let setup = conn.setup();
        let min = setup.min_keycode;
        let max = setup.max_keycode;
        let count = max - min + 1;
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
        Ok(Keymap { symbols })
    }

    /// Find (keycode, needs_shift) for a keysym.
    pub fn lookup(&self, keysym: u32) -> Option<(u8, bool)> {
        let setup_min = 8u16; // X11 min keycode is always 8
        for (i, syms) in self.symbols.iter().enumerate() {
            if syms[0] == keysym {
                return Some(((i as u16 + setup_min) as u8, false));
            }
            if syms[1] == keysym {
                return Some(((i as u16 + setup_min) as u8, true));
            }
        }
        None
    }
}

impl Desktop {
    pub fn connect() -> Result<Self> {
        let (conn, screen_num) =
            x11rb::connect(None).context("cannot connect to X display (is DISPLAY set?)")?;
        let screen = &conn.setup().roots[screen_num];
        let geom = ScreenGeom {
            width: screen.width_in_pixels,
            height: screen.height_in_pixels,
        };
        let root = screen.root;
        Ok(Desktop { conn, root, geom })
    }

    pub fn geom(&self) -> ScreenGeom {
        self.geom
    }

    pub fn conn(&self) -> &RustConnection {
        &self.conn
    }

    /// Capture a rectangle. `region = None` means the whole screen.
    pub fn capture(&self, region: Option<(i16, i16, u16, u16)>) -> Result<Frame> {
        let (x, y, w, h) = region.unwrap_or((0, 0, self.geom.width, self.geom.height));
        if w == 0 || h == 0 {
            return Err(anyhow!("capture region has zero width or height"));
        }
        let img = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, x, y, w, h, !0)
            .context("GetImage request failed")?
            .reply()
            .context("GetImage reply failed (region off-screen?)")?;

        let pixel_bytes = (img.depth as usize).div_ceil(8);
        let src = &img.data;
        let expected = w as usize * h as usize * pixel_bytes;
        if src.len() < expected {
            return Err(anyhow!(
                "short image data: got {} expected {}",
                src.len(),
                expected
            ));
        }

        // Swizzle to RGB8. On little-endian TrueColor X11 the layout is B,G,R,X.
        let npix = w as usize * h as usize;
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
                // 16-bit packed 565
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

        // Debug hook: report what this process actually sampled, so a mismatch
        // against an external screenshot is attributable rather than mysterious.
        if std::env::var("LDMCP_DEBUG_PIXELS").is_ok() {
            let at = |x: u16, y: u16| -> (u8, u8, u8) {
                if x >= w || y >= h {
                    return (0, 0, 0);
                }
                let o = (y as usize * w as usize + x as usize) * 3;
                (rgb[o], rgb[o + 1], rgb[o + 2])
            };
            eprintln!(
                "[dbg] screen={}x{} cap=({},{},{}x{}) info_depth={} px(10,10)={:?} px(400,300)={:?}",
                self.geom.width, self.geom.height, x, y, w, h, img.depth,
                at(10, 10), at(400, 300)
            );
        }

        Ok(Frame {
            width: w,
            height: h,
            rgb,
        })
    }

    // ---- input synthesis ----

    pub fn move_pointer(&self, x: i16, y: i16) -> Result<()> {
        self.conn
            .xtest_fake_input(
                x11rb::protocol::xproto::MOTION_NOTIFY_EVENT,
                0,
                0,
                self.root,
                x,
                y,
                0,
            )
            .context("XTEST motion failed")?;
        self.conn.flush().ok();
        Ok(())
    }

    pub fn click(&self, button: u8, press: bool) -> Result<()> {
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
    /// X11 nests reparenting frames and client windows; reporting both would
    /// duplicate every app. We walk the direct children of `root` (the window
    /// manager's frames) and keep only those that carry a real WM name, which
    /// filters out frames, docks and compositor helper windows.
    pub fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        // EWMH is the reliable entry point: a compositor-managed client list
        // already excludes docks, desktops and compositor helper windows.
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

    /// Read a list-of-windows root property (EWMH client lists).
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
            .ok()
            .unwrap()
            .reply()
            .ok()
            .unwrap()
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
    /// Read `WM_CLASS` (instance, class) and use the class as a display name when
    /// no WM_NAME/_NET_WM_NAME exists — common on lightweight compositors.
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
        // Two NUL-terminated strings: instance\0class\0
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
