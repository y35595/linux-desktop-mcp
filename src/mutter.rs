//! Keyboard-only injection through mutter's `RemoteDesktop` D-Bus interface.
//!
//! This is a **fallback**, used only when `/dev/uinput` is not writable and the
//! session is Wayland. It carries keyboard input and nothing else.
//!
//! Scope is deliberately narrow because of what could be verified on GNOME 48:
//!
//! - `NotifyKeyboardKeycode` / `NotifyKeyboardKeysym` genuinely reach the
//!   compositor (confirmed by watching the session volume follow keysym
//!   `0x1008ff13` / keycode `115`).
//! - `NotifyPointerMotionRelative` returns success while moving nothing
//!   (confirmed with an `XQueryPointer` baseline that does track XTEST), so
//!   the pointer is *not* implemented here.
//! - `NotifyPointerMotionAbsolute` requires an attached screencast and fails
//!   with `No screen cast active` otherwise.
//!
//! `CreateSession` needs no consent dialog: mutter's permission check is only
//! "same D-Bus peer that created the session", so a local process can open one
//! without a portal or a prompt.

use anyhow::{anyhow, Context, Result};
use zbus::blocking::Connection;
use zbus::zvariant::OwnedObjectPath;

pub const SERVICE: &str = "org.gnome.Mutter.RemoteDesktop";
pub const PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
const IFACE: &str = "org.gnome.Mutter.RemoteDesktop";

/// Is mutter's private RemoteDesktop service on the bus?
pub fn available() -> bool {
    let Ok(conn) = Connection::session() else {
        return false;
    };
    conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "NameHasOwner",
        &(SERVICE,),
    )
    .ok()
    .and_then(|r| r.body().deserialize::<bool>().ok())
    .unwrap_or(false)
}

/// An open, started RemoteDesktop session.
pub struct MutterSession {
    conn: Connection,
    path: OwnedObjectPath,
}

impl MutterSession {
    /// Create and start a session. No user interaction is involved.
    pub fn start() -> Result<Self> {
        let conn = Connection::session().context("cannot open session bus")?;
        let reply = conn
            .call_method(Some(SERVICE), PATH, Some(IFACE), "CreateSession", &())
            .context("mutter CreateSession failed")?;
        let path: OwnedObjectPath = reply
            .body()
            .deserialize()
            .context("mutter returned an unexpected session path")?;
        let session = MutterSession {
            conn: conn.clone(),
            path: path.clone(),
        };
        conn.call_method(
            Some(SERVICE),
            path.as_str(),
            Some("org.gnome.Mutter.RemoteDesktop.Session"),
            "Start",
            &(),
        )
        .context("mutter session Start failed")?;
        Ok(session)
    }

    fn call(
        &self,
        method: &str,
        body: impl serde::Serialize + zbus::zvariant::DynamicType,
    ) -> Result<()> {
        self.conn
            .call_method(
                Some(SERVICE),
                self.path.as_str(),
                Some("org.gnome.Mutter.RemoteDesktop.Session"),
                method,
                &body,
            )
            .with_context(|| format!("mutter {method} failed"))?;
        Ok(())
    }

    fn keysym(&self, keysym: u32, press: bool) -> Result<()> {
        self.call("NotifyKeyboardKeysym", (keysym, press))
    }

    /// Type text one keysym at a time.
    ///
    /// Only keysyms the X11 table can express are supported; anything above
    /// Latin-1 is refused with a precise message rather than mistyped.
    pub fn type_text(&mut self, text: &str) -> Result<()> {
        for ch in text.chars() {
            let keysym = char_to_keysym(ch)?;
            self.keysym(keysym, true)?;
            self.keysym(keysym, false)?;
        }
        Ok(())
    }

    pub fn hotkey(&mut self, spec: &str) -> Result<()> {
        let mut mods = Vec::new();
        let mut main = None;
        for raw in spec.split('+') {
            let part = raw.trim();
            if part.is_empty() {
                continue;
            }
            let lower = part.to_ascii_lowercase();
            let sym = match lower.as_str() {
                "ctrl" | "control" => 0xffe3u32,
                "shift" => 0xffe1,
                "alt" | "mod1" => 0xffe9,
                "super" | "meta" | "win" | "mod4" => 0xffeb,
                other if other.chars().count() == 1 => {
                    char_to_keysym(other.chars().next().unwrap())?
                }
                other => return Err(anyhow!("mutter fallback cannot press '{other}'")),
            };
            if matches!(
                lower.as_str(),
                "ctrl" | "control" | "shift" | "alt" | "mod1" | "super" | "meta" | "win" | "mod4"
            ) {
                mods.push(sym);
            } else {
                main = Some(sym);
            }
        }
        let main = main.ok_or_else(|| anyhow!("no non-modifier key in '{spec}'"))?;
        for &m in &mods {
            self.keysym(m, true)?;
        }
        self.keysym(main, true)?;
        self.keysym(main, false)?;
        for &m in mods.iter().rev() {
            self.keysym(m, false)?;
        }
        Ok(())
    }
}

impl Drop for MutterSession {
    fn drop(&mut self) {
        // Always release the session; leaked sessions keep virtual devices
        // alive inside the compositor.
        let _ = self.conn.call_method(
            Some(SERVICE),
            self.path.as_str(),
            Some("org.gnome.Mutter.RemoteDesktop.Session"),
            "Stop",
            &(),
        );
    }
}

fn char_to_keysym(c: char) -> Result<u32> {
    let cp = c as u32;
    if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) {
        Ok(cp)
    } else {
        Err(anyhow!(
            "the mutter keyboard fallback cannot type {c:?} (U+{cp:04X}); \
             install the uinput udev rule to get full text input"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_keysyms_pass_through() {
        assert_eq!(char_to_keysym('a').unwrap(), 0x61);
        assert_eq!(char_to_keysym(' ').unwrap(), 0x20);
    }

    #[test]
    fn non_latin1_is_refused_rather_than_mistyped() {
        assert!(char_to_keysym('中').is_err());
    }
}
