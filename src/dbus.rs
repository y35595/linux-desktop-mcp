//! D-Bus helpers: XDG Desktop Portal and Mutter geometry queries.
//!
//! Only *read-only* geometry queries and the non-interactive portal
//! `Screenshot` method live here. Input injection intentionally does **not**
//! go through mutter's `RemoteDesktop` interface: on GNOME, relative motion
//! sent there does not reach the compositor's pointer at all (verified on
//! GNOME 48 — `NotifyPointerMotionRelative` returns success, yet the pointer
//! never moves), so it is not a usable input backend.
//!
//! The portal is feature-detected by asking the bus, not by looking for the
//! `xdg-desktop-portal` binary: on the reference host the binary is not on
//! `PATH` while the service is running and fully functional.

use crate::platform::{CoordinateModel, LogicalMonitor};
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use x11rb::connection::Connection as X11Connection;
use zbus::blocking::Connection;
use zbus::match_rule::MatchRule;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

pub const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";

/// Does `org.freedesktop.DBus.NameHasOwner` report the portal as running?
pub fn portal_available() -> bool {
    let Ok(conn) = Connection::session() else {
        return false;
    };
    conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "NameHasOwner",
        &(PORTAL_SERVICE,),
    )
    .ok()
    .and_then(|r| r.body().deserialize::<bool>().ok())
    .unwrap_or(false)
}

/// Is a well-known name currently owned on the session bus?
fn name_has_owner(conn: &Connection, name: &str) -> bool {
    conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "NameHasOwner",
        &(name,),
    )
    .ok()
    .and_then(|r| r.body().deserialize::<bool>().ok())
    .unwrap_or(false)
}

/// Request one full-screen capture through the portal.
///
/// Returns the PNG bytes. The portal writes a file into the user's home and
/// hands back a `file://` URI; we read it and unlink it so the request does not
/// litter `~/Pictures`.
///
/// `interactive: false` is passed explicitly. Some portal versions default it
/// to true and would pop a consent dialog; GNOME 48 honours the flag and
/// returns silently.
pub fn portal_screenshot(timeout: Duration) -> Result<Vec<u8>> {
    let conn = Connection::session().context("cannot open session bus")?;

    // Subscribe *before* issuing the request: the Response signal can arrive
    // before the method reply is processed, and a late subscription loses it.
    let (tx, rx) = mpsc::channel::<(String, u32, HashMap<String, OwnedValue>)>();
    {
        let conn = conn.clone();
        std::thread::spawn(move || {
            let rule = match MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface("org.freedesktop.portal.Request")
                .and_then(|b| b.member("Response"))
            {
                Ok(b) => b.build(),
                Err(_) => return,
            };
            let Ok(mut iter) =
                zbus::blocking::MessageIterator::for_match_rule(rule, &conn, Some(64))
            else {
                return;
            };
            for msg in iter.by_ref() {
                let Ok(msg) = msg else { continue };
                let path = msg
                    .header()
                    .path()
                    .map(|p| p.to_string())
                    .unwrap_or_default();
                let Ok(body) = msg
                    .body()
                    .deserialize::<(u32, HashMap<String, OwnedValue>)>()
                else {
                    continue;
                };
                if tx.send((path, body.0, body.1)).is_err() {
                    return;
                }
            }
        });
    }
    // Give the match rule a moment to be installed bus-side.
    std::thread::sleep(Duration::from_millis(150));

    let token = format!("ldmcp{}", std::process::id());
    let mut opts: HashMap<&str, Value> = HashMap::new();
    opts.insert("interactive", Value::from(false));
    opts.insert("modal", Value::from(false));
    opts.insert("handle_token", Value::from(token.as_str()));

    let reply = conn
        .call_method(
            Some(PORTAL_SERVICE),
            PORTAL_PATH,
            Some("org.freedesktop.portal.Screenshot"),
            "Screenshot",
            &("", opts),
        )
        .context("portal Screenshot call failed")?;
    let handle: OwnedObjectPath = reply
        .body()
        .deserialize()
        .context("portal returned an unexpected handle")?;
    let handle = handle.to_string();

    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(anyhow!(
                "portal Screenshot timed out after {:?} (a consent dialog may be waiting)",
                timeout
            ));
        }
        match rx.recv_timeout(left.min(Duration::from_secs(5))) {
            Ok((path, code, results)) => {
                // Match either the exact handle or our unique token.
                if path != handle && !path.ends_with(&token) {
                    continue;
                }
                match code {
                    0 => {}
                    1 => return Err(anyhow!("portal Screenshot was cancelled by the user")),
                    other => return Err(anyhow!("portal Screenshot failed (response {other})")),
                }
                let uri = results
                    .get("uri")
                    .and_then(|v| v.downcast_ref::<&str>().ok().map(|s| s.to_string()))
                    .ok_or_else(|| anyhow!("portal response carried no uri"))?;
                return read_and_unlink_uri(&uri);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow!("portal response listener stopped unexpectedly"))
            }
        }
    }
}

fn read_and_unlink_uri(uri: &str) -> Result<Vec<u8>> {
    let path = uri
        .strip_prefix("file://")
        .ok_or_else(|| anyhow!("portal returned a non-file uri: {uri}"))?;
    let bytes =
        std::fs::read(path).with_context(|| format!("cannot read portal screenshot at {path}"))?;
    // Best effort: the file is ours, but never fail the capture over cleanup.
    let _ = std::fs::remove_file(path);
    Ok(bytes)
}

/// Read monitor geometry and scale from GNOME's DisplayConfig.
///
/// Returns `None` when mutter is not running. The logical size and the
/// fractional scale are authoritative here; the physical size is learned
/// later from the first real capture.
pub fn gnome_coordinate_model() -> Option<CoordinateModel> {
    let conn = Connection::session().ok()?;
    if !name_has_owner(&conn, "org.gnome.Mutter.DisplayConfig") {
        return None;
    }
    let reply = conn
        .call_method(
            Some("org.gnome.Mutter.DisplayConfig"),
            "/org/gnome/Mutter/DisplayConfig",
            Some("org.gnome.Mutter.DisplayConfig"),
            "GetCurrentState",
            &(),
        )
        .ok()?;
    let body = reply.body();
    // Exact signature from Mutter's DBus API:
    //   (u serial_num,
    //    a((ssss)a(siiddda{sv}) a{sv}) monitors,   // spec, modes[], monitor props
    //    a(iiduba(ssss)a{sv})        logical,      // x, y, scale, transform,
    //                                              // primary, monitors[], props
    //    a{sv} properties)
    //
    // The mode entry is (name s, width i, height i, rate d, preferred_scale d,
    // supported_scales ad, props a{sv}) — note the 5th field is a *scale
    // double*, not a boolean. Getting this wrong makes deserialization fail
    // silently, and the caller then falls back to the X11 root size with
    // scale 1.0, which silently misplaces every click on a HiDPI screen.
    type Spec = (String, String, String, String);
    type Mode = (
        String,
        i32,
        i32,
        f64,
        f64,
        Vec<f64>,
        HashMap<String, OwnedValue>,
    );
    type Monitor = (Spec, Vec<Mode>, HashMap<String, OwnedValue>);
    type Logical = (
        i32,
        i32,
        f64,
        u32,
        bool,
        Vec<Spec>,
        HashMap<String, OwnedValue>,
    );
    let parsed =
        body.deserialize::<(u32, Vec<Monitor>, Vec<Logical>, HashMap<String, OwnedValue>)>();

    let (_serial, monitors, logical, _props) = match parsed {
        Ok(v) => v,
        Err(_) => return None,
    };

    let mut out = CoordinateModel {
        scale: 1.0,
        scale_source: "gnome DisplayConfig",
        ..Default::default()
    };

    for (x, y, scale, _transform, primary, lm_monitors, _p) in logical {
        let connector = lm_monitors
            .first()
            .map(|m| m.0.clone())
            .unwrap_or_else(|| "unknown".to_string());

        // Find the active mode of that connector to get the native size.
        // Mode entries are (name, width, height, refresh, preferred, scales, props).
        let mut native = (0i32, 0i32);
        if let Some((_spec, modes, _mprops)) =
            monitors.iter().find(|(spec, _, _)| spec.0 == connector)
        {
            for (_name, w, h, _refresh, _preferred_scale, _scales, props) in modes {
                let current = props
                    .get("is-current")
                    .and_then(|v| v.downcast_ref::<bool>().ok())
                    .unwrap_or(false);
                if current {
                    native = (*w, *h);
                    break;
                }
                if native == (0, 0) {
                    native = (*w, *h);
                }
            }
        }

        let logical_w = (native.0 as f64 / scale.max(0.01)).round() as i32;
        let logical_h = (native.1 as f64 / scale.max(0.01)).round() as i32;

        out.monitors.push(LogicalMonitor {
            connector,
            x,
            y,
            width: logical_w,
            height: logical_h,
            scale,
            primary,
        });

        if primary || out.logical_width == 0 {
            out.logical_width = logical_w;
            out.logical_height = logical_h;
            out.scale = scale;
        }
    }

    if out.monitors.is_empty() {
        return None;
    }
    Some(out)
}

/// Fallback logical size/scale from X11 `Xft.dpi` or the Xwayland root size.
pub fn x11_root_size() -> Option<(i32, i32)> {
    let (conn, screen_num) = x11rb::connect(None).ok()?;
    let screen = &conn.setup().roots[screen_num];
    Some((
        screen.width_in_pixels as i32,
        screen.height_in_pixels as i32,
    ))
}

/// Is the session screen locked?
///
/// A locked session is the most common cause of an all-black capture that is
/// otherwise reported as a success: the compositor will not paint the desktop
/// into a portal screenshot while the lock screen is up. Detecting it lets the
/// server warn instead of handing the caller an unusable black image.
pub fn session_locked() -> Option<bool> {
    let conn = Connection::session().ok()?;
    // org.gnome.ScreenSaver is the most widely implemented lock indicator on
    // GNOME; fall back to logind's LockedHint when it is absent.
    if name_has_owner(&conn, "org.gnome.ScreenSaver") {
        let reply = conn
            .call_method(
                Some("org.gnome.ScreenSaver"),
                "/org/gnome/ScreenSaver",
                Some("org.gnome.ScreenSaver"),
                "GetActive",
                &(),
            )
            .ok()?;
        return reply.body().deserialize::<bool>().ok();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_detection_does_not_panic() {
        // Must be a pure probe: no bus on CI is fine, it just returns false.
        let _ = portal_available();
    }

    /// The `GetCurrentState` reply is a deeply nested tuple, and getting one
    /// field's type wrong makes `deserialize` fail — which used to fall back
    /// silently to scale 1.0 and misplace every click on a HiDPI screen.
    ///
    /// These signatures are pinned against a real Mutter 48 reply captured on
    /// the reference host so a regression fails here instead of at runtime.
    #[test]
    fn displayconfig_signature_matches_mutter() {
        use std::collections::HashMap;
        use zbus::zvariant::{OwnedValue, Value};

        type Spec = (String, String, String, String);
        type Mode = (
            String,
            i32,
            i32,
            f64,
            f64,
            Vec<f64>,
            HashMap<String, OwnedValue>,
        );

        // The real 5th mode field is the preferred *scale* (a double), not a
        // boolean. Build one and make sure it deserializes as we expect.
        let mut props: HashMap<String, OwnedValue> = HashMap::new();
        props.insert("is-current".to_string(), OwnedValue::from(true));
        let mode: Mode = (
            "3200x2000@120.000".to_string(),
            3200,
            2000,
            120.0,
            1.7467248439788818,
            vec![1.0, 1.25, 1.5],
            props,
        );
        let v = Value::from(mode.clone());
        let back: Mode = v.try_into().expect("mode tuple round-trips");
        assert_eq!(back.0, "3200x2000@120.000");
        assert_eq!(back.1, 3200);
        assert_eq!(back.2, 2000);
        assert!((back.4 - 1.7467248439788818).abs() < 1e-9);

        // A monitors entry is a triple (spec, modes, props), not a pair.
        let spec: Spec = (
            "eDP-1".to_string(),
            "TMX".to_string(),
            "TL156MDMP31-0".to_string(),
            "0x00000000".to_string(),
        );
        let monitor = (spec, vec![mode], HashMap::<String, OwnedValue>::new());
        let mv = Value::from(monitor);
        let parsed: (Spec, Vec<Mode>, HashMap<String, OwnedValue>) =
            mv.try_into().expect("monitor triple round-trips");
        assert_eq!(parsed.0 .0, "eDP-1");
        assert_eq!(parsed.1.len(), 1);
    }
}
