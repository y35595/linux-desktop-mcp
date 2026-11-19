//! Kernel-level input injection through `/dev/uinput`.
//!
//! ## Why uinput
//!
//! On a Wayland session, XTEST only reaches Xwayland clients. Verified on
//! GNOME 48: after `XTestFakeMotionEvent` / `XTestFakeKeyEvent`, Xwayland's
//! own pointer reports the new position, yet the compositor's UI never reacts
//! (sending `Super` via XTEST does not open the overview, while the same
//! keystroke through mutter's D-Bus interface does).
//!
//! mutter's `org.gnome.Mutter.RemoteDesktop` has the mirror-image problem: its
//! keyboard methods work (verified by watching the volume change), but
//! `NotifyPointerMotionRelative` returns success and moves nothing, and
//! `NotifyPointerMotionAbsolute` refuses with `No screen cast active` unless a
//! PipeWire screencast is attached.
//!
//! uinput sits *below* the display server, so the compositor routes our events
//! through the normal libinput path for Wayland-native clients **and**
//! Xwayland clients alike.
//!
//! ## Absolute positioning
//!
//! A virtual **direct** absolute device (`INPUT_PROP_DIRECT`) with
//! `ABS_X`/`ABS_Y` declared over `0..capture_width-1` makes libinput map the
//! axis range onto the output layout, so a value is literally a screen pixel.
//! That is the only way to position the pointer exactly without a screencast.
//!
//! ## Requirement
//!
//! `/dev/uinput` must be writable. Distros ship it `root:root 0600`; the
//! one-time admin setup is documented in the README. This module never tries
//! to escalate privileges and never creates a device during detection.

use anyhow::{anyhow, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;

// ---- ioctl encoding (see <asm-generic/ioctl.h>) ----

const IOC_NRBITS: u64 = 8;
const IOC_TYPEBITS: u64 = 8;
const IOC_SIZEBITS: u64 = 14;
const IOC_NRSHIFT: u64 = 0;
const IOC_TYPESHIFT: u64 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u64 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u64 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_WRITE: u64 = 1;

const fn ioc(dir: u64, ty: u8, nr: u8, size: u64) -> u64 {
    (dir << IOC_DIRSHIFT)
        | ((ty as u64) << IOC_TYPESHIFT)
        | ((nr as u64) << IOC_NRSHIFT)
        | (size << IOC_SIZESHIFT)
}

/// `_IO(type, nr)`
const fn io(ty: u8, nr: u8) -> u64 {
    ioc(0, ty, nr, 0)
}

/// `_IOW(type, nr, T)`
const fn iow<T>(ty: u8, nr: u8) -> u64 {
    ioc(IOC_WRITE, ty, nr, std::mem::size_of::<T>() as u64)
}

/// uinput ioctls all use type `'U'`.
const U: u8 = b'U';

const fn ui_dev_create() -> u64 {
    io(U, 1)
}
const fn ui_dev_destroy() -> u64 {
    io(U, 2)
}
const fn ui_dev_setup() -> u64 {
    iow::<UinputSetup>(U, 3)
}
const fn ui_abs_setup() -> u64 {
    iow::<UinputAbsSetup>(U, 4)
}
const fn ui_set_evbit() -> u64 {
    iow::<u32>(U, 100)
}
const fn ui_set_keybit() -> u64 {
    iow::<u32>(U, 101)
}
const fn ui_set_relbit() -> u64 {
    iow::<u32>(U, 102)
}
const fn ui_set_absbit() -> u64 {
    iow::<u32>(U, 103)
}
const fn ui_set_propbit() -> u64 {
    iow::<u32>(U, 110)
}

extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

use std::os::unix::io::AsRawFd;

// ---- kernel ABI structs ----

#[repr(C)]
#[derive(Clone, Copy)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsinfo,
}

/// `struct input_event` on 64-bit little-endian Linux.
#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    tv_sec: i64,
    tv_usec: i64,
    kind: u16,
    code: u16,
    value: i32,
}

// ---- evdev codes (<linux/input-event-codes.h>) ----

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;

const INPUT_PROP_POINTER: u16 = 0x00;
const INPUT_PROP_DIRECT: u16 = 0x01;

const BUS_VIRTUAL: u16 = 0x06;

/// Which physical button a tool asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
}

impl Button {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "left" | "1" => Ok(Button::Left),
            "middle" | "2" => Ok(Button::Middle),
            "right" | "3" => Ok(Button::Right),
            other => Err(anyhow!("bad button '{other}' (expected left|middle|right)")),
        }
    }

    fn code(self) -> u16 {
        match self {
            Button::Left => BTN_LEFT,
            Button::Middle => BTN_MIDDLE,
            Button::Right => BTN_RIGHT,
        }
    }
}

/// Whether a uinput device can be created right now, and if not, why.
#[derive(Debug, Clone)]
pub struct Availability {
    pub usable: bool,
    pub reason: String,
}

/// Probe `/dev/uinput` by opening it and closing again.
///
/// This never creates a device, so it is safe to call at startup.
/// Read the groups the *current process* is a member of.
///
/// `/etc/group` may list the user while the running session has not inherited
/// membership yet — group changes only apply at login. Distinguishing those two
/// cases turns a confusing "permission denied" into an actionable message.
fn process_groups() -> Vec<u32> {
    #[repr(C)]
    struct Group {
        gr_name: *const u8,
    }
    extern "C" {
        fn getgroups(size: i32, list: *mut u32) -> i32;
    }
    let mut buf = vec![0u32; 64];
    // SAFETY: buf is a valid, correctly-sized mutable buffer.
    let n = unsafe { getgroups(buf.len() as i32, buf.as_mut_ptr()) };
    if n < 0 {
        return Vec::new();
    }
    buf.truncate(n as usize);
    let _ = std::mem::size_of::<Group>();
    buf
}

/// Does `/etc/group` list the current user in `input`?
fn user_listed_in_input_group() -> bool {
    let Ok(groups) = std::fs::read_to_string("/etc/group") else {
        return false;
    };
    let Ok(uid_name) = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")) else {
        return false;
    };
    groups.lines().any(|line| {
        let mut parts = line.split(':');
        matches!(parts.next(), Some("input"))
            && parts
                .nth(2)
                .map(|members| members.split(',').any(|m| m.trim() == uid_name))
                .unwrap_or(false)
    })
}

/// The gid of the `input` group, if it exists.
fn input_gid() -> Option<u32> {
    let groups = std::fs::read_to_string("/etc/group").ok()?;
    for line in groups.lines() {
        let mut parts = line.split(':');
        if parts.next() == Some("input") {
            return parts.next().and_then(|g| g.parse().ok());
        }
    }
    None
}

/// Whether a uinput device can be created right now, and if not, why.
///
/// The reason strings are written to be actionable: they name the single next
/// step rather than restating the error.
pub fn probe() -> Availability {
    match OpenOptions::new().write(true).open("/dev/uinput") {
        Ok(_) => Availability {
            usable: true,
            reason: "ok".to_string(),
        },
        Err(e) => {
            use std::os::unix::fs::PermissionsExt;
            let exists = std::fs::metadata("/dev/uinput").is_ok();
            if !exists {
                return Availability {
                    usable: false,
                    reason:
                        "uinput-missing: /dev/uinput does not exist — run `sudo modprobe uinput`"
                            .to_string(),
                };
            }
            let mode = std::fs::metadata("/dev/uinput")
                .map(|m| m.permissions().mode() & 0o777)
                .unwrap_or(0);
            let gid_now = process_groups();
            let gid_input = input_gid();
            let in_group_now = gid_input.map(|g| gid_now.contains(&g)).unwrap_or(false);
            let listed = user_listed_in_input_group();

            let reason = if mode == 0o600 {
                // The udev rule has not been applied to the device node yet.
                if listed || in_group_now {
                    format!(
                        "uinput-stale-node: /dev/uinput is still mode {mode:04o}, so the udev \
                         rule has not been applied to the node yet. It was installed but the \
                         node was created before it took effect — run `sudo udevadm control \
                         --reload-rules && sudo udevadm trigger --name-match=uinput` (or \
                         reboot) so it becomes crw-rw---- root input."
                    )
                } else {
                    format!(
                        "uinput-permission: /dev/uinput is mode {mode:04o} and not writable. \
                         Install the udev rule and join the `input` group (see README), then \
                         reboot. os error: {e}"
                    )
                }
            } else if listed && !in_group_now {
                format!(
                    "uinput-relogin: /dev/uinput is mode {mode:04o} and you are listed in the \
                     `input` group in /etc/group, but this login session has not inherited that \
                     group yet. Group membership only applies at login: log out and back in \
                     (a new terminal is not enough). os error: {e}"
                )
            } else if !in_group_now {
                format!(
                    "uinput-permission: /dev/uinput is mode {mode:04o} and this process is not \
                     in the `input` group. Run `sudo usermod -aG input $USER`, then log out and \
                     back in. os error: {e}"
                )
            } else {
                format!(
                    "uinput-denied: in the `input` group and /dev/uinput is mode {mode:04o}, \
                     but opening it still failed. os error: {e}"
                )
            };
            Availability {
                usable: false,
                reason,
            }
        }
    }
}

fn check(rc: i32, what: &str) -> Result<()> {
    if rc < 0 {
        Err(anyhow!(
            "{what} failed: {}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

fn set_bit(fd: i32, request: u64, bit: u16, what: &str) -> Result<()> {
    let v = bit as u32;
    // SAFETY: fd is an open uinput descriptor owned by the caller, and `v`
    // outlives the call.
    let rc = unsafe { ioctl(fd, request, &v as *const u32) };
    check(rc, what)
}

fn write_setup(fd: i32, request: u64, ptr: *const u8, len: usize, what: &str) -> Result<()> {
    // SAFETY: caller guarantees `ptr` points at `len` initialised bytes.
    let rc = unsafe { ioctl(fd, request, ptr) };
    let _ = len;
    check(rc, what)
}

/// A virtual absolute pointer that also carries wheel axes.
pub struct AbsPointer {
    file: File,
    width: i32,
    height: i32,
}

impl AbsPointer {
    /// Create the device with `width`x`height` as the absolute axis range.
    ///
    /// Passing the capture resolution makes capture pixels and pointer
    /// coordinates identical, so no scale conversion is needed at click time.
    pub fn create(width: i32, height: i32) -> Result<Self> {
        if width <= 1 || height <= 1 {
            return Err(anyhow!(
                "absolute pointer needs an axis range > 1 (got {width}x{height})"
            ));
        }
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .map_err(|e| {
                let a = probe();
                anyhow!("cannot open /dev/uinput: {e} ({})", a.reason)
            })?;
        let fd = file.as_raw_fd();

        for ev in [EV_KEY, EV_REL, EV_ABS, EV_SYN] {
            set_bit(fd, ui_set_evbit(), ev, "UI_SET_EVBIT")?;
        }
        for btn in [BTN_LEFT, BTN_MIDDLE, BTN_RIGHT] {
            set_bit(fd, ui_set_keybit(), btn, "UI_SET_KEYBIT")?;
        }
        for rel in [REL_WHEEL, REL_HWHEEL] {
            set_bit(fd, ui_set_relbit(), rel, "UI_SET_RELBIT")?;
        }

        // Declare the absolute axes over the capture-pixel range.
        for (code, max) in [(ABS_X, width - 1), (ABS_Y, height - 1)] {
            let setup = UinputAbsSetup {
                code,
                absinfo: InputAbsinfo {
                    value: 0,
                    minimum: 0,
                    maximum: max,
                    fuzz: 0,
                    flat: 0,
                    // 1 unit per pixel; libinput uses this to scale the axis.
                    resolution: 1,
                },
            };
            write_setup(
                fd,
                ui_abs_setup(),
                &setup as *const UinputAbsSetup as *const u8,
                std::mem::size_of::<UinputAbsSetup>(),
                "UI_ABS_SETUP",
            )?;
            set_bit(fd, ui_set_absbit(), code, "UI_SET_ABSBIT")?;
        }

        // INPUT_PROP_DIRECT is what tells libinput this is a direct absolute
        // device rather than a relative touchpad; without it the axis values
        // are ignored for cursor positioning.
        set_bit(
            fd,
            ui_set_propbit(),
            INPUT_PROP_DIRECT,
            "UI_SET_PROPBIT(DIRECT)",
        )?;
        set_bit(
            fd,
            ui_set_propbit(),
            INPUT_PROP_POINTER,
            "UI_SET_PROPBIT(POINTER)",
        )?;

        let mut name = [0u8; 80];
        let label = b"linux-desktop-mcp abs pointer";
        name[..label.len()].copy_from_slice(label);
        let setup = UinputSetup {
            id: InputId {
                bustype: BUS_VIRTUAL,
                vendor: 0x1d6b,
                product: 0x0104,
                version: 1,
            },
            name,
            ff_effects_max: 0,
        };
        write_setup(
            fd,
            ui_dev_setup(),
            &setup as *const UinputSetup as *const u8,
            std::mem::size_of::<UinputSetup>(),
            "UI_DEV_SETUP",
        )?;

        // SAFETY: fd is a valid open uinput descriptor.
        check(unsafe { ioctl(fd, ui_dev_create()) }, "UI_DEV_CREATE")?;

        // udev/libinput need a moment to enumerate the new node, otherwise the
        // first few events are dropped.
        std::thread::sleep(std::time::Duration::from_millis(150));

        Ok(AbsPointer {
            file,
            width,
            height,
        })
    }

    fn event(&mut self, kind: u16, code: u16, value: i32) -> Result<()> {
        let ev = InputEvent {
            tv_sec: 0,
            tv_usec: 0,
            kind,
            code,
            value,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &ev as *const InputEvent as *const u8,
                std::mem::size_of::<InputEvent>(),
            )
        };
        self.file
            .write_all(bytes)
            .context("write to /dev/uinput failed")
    }

    fn syn(&mut self) -> Result<()> {
        self.event(EV_SYN, SYN_REPORT, 0)
    }

    fn clamp(&self, x: i32, y: i32) -> (i32, i32) {
        (x.clamp(0, self.width - 1), y.clamp(0, self.height - 1))
    }

    /// Move to an absolute capture-pixel coordinate. Returns what was emitted.
    pub fn move_to(&mut self, x: i32, y: i32) -> Result<(i32, i32)> {
        let (cx, cy) = self.clamp(x, y);
        self.event(EV_ABS, ABS_X, cx)?;
        self.event(EV_ABS, ABS_Y, cy)?;
        self.syn()?;
        Ok((cx, cy))
    }

    pub fn press(&mut self, button: Button, at: Option<(i32, i32)>) -> Result<(i32, i32)> {
        let pos = match at {
            Some((x, y)) => self.move_to(x, y)?,
            None => (-1, -1),
        };
        self.event(EV_KEY, button.code(), 1)?;
        self.syn()?;
        Ok(pos)
    }

    pub fn release(&mut self, button: Button) -> Result<()> {
        self.event(EV_KEY, button.code(), 0)?;
        self.syn()
    }

    /// One wheel "notch"; positive is up/left, matching REL_WHEEL semantics.
    pub fn wheel(
        &mut self,
        vertical: i32,
        horizontal: i32,
        at: Option<(i32, i32)>,
    ) -> Result<(i32, i32)> {
        let pos = match at {
            Some((x, y)) => self.move_to(x, y)?,
            None => (-1, -1),
        };
        if vertical != 0 {
            // REL_WHEEL is positive for wheel-up.
            self.event(EV_REL, REL_WHEEL, vertical)?;
        }
        if horizontal != 0 {
            self.event(EV_REL, REL_HWHEEL, horizontal)?;
        }
        self.syn()?;
        Ok(pos)
    }

    /// Remove the device so udev does not accumulate stale nodes.
    pub fn destroy(&mut self) {
        // SAFETY: fd belongs to self.file and is still open.
        unsafe {
            ioctl(self.file.as_raw_fd(), ui_dev_destroy());
        }
    }
}

impl Drop for AbsPointer {
    fn drop(&mut self) {
        self.destroy();
    }
}

// ---- keyboard ----

/// A virtual keyboard that types Linux keycodes.
///
/// Independent of the pointer device: mixing them on one node confuses some
/// compositors, and a separate node also keeps the pointer's absolute axes
/// unambiguous.
pub struct VirtualKeyboard {
    file: File,
}

/// Linux keycodes we can press directly (evdev numbering, **not** X keycodes).
pub fn char_to_keycode(c: char) -> Option<(u16, bool)> {
    // Returns (keycode, needs_shift) for the US layout. Latin-1 punctuation
    // that differs between layouts is intentionally omitted rather than
    // guessed, so the caller gets a precise error instead of wrong output.
    let (code, shift) = match c {
        'a'..='z' => {
            const ROW: [(u16, char); 26] = [
                (30, 'a'),
                (48, 'b'),
                (46, 'c'),
                (32, 'd'),
                (18, 'e'),
                (33, 'f'),
                (34, 'g'),
                (35, 'h'),
                (23, 'i'),
                (36, 'j'),
                (37, 'k'),
                (38, 'l'),
                (50, 'm'),
                (49, 'n'),
                (24, 'o'),
                (25, 'p'),
                (16, 'q'),
                (19, 'r'),
                (31, 's'),
                (20, 't'),
                (22, 'u'),
                (47, 'v'),
                (17, 'w'),
                (45, 'x'),
                (21, 'y'),
                (44, 'z'),
            ];
            let idx = (c as u8 - b'a') as usize;
            debug_assert_eq!(ROW[idx].1, c);
            (ROW[idx].0, false)
        }
        'A'..='Z' => {
            let lower = c.to_ascii_lowercase();
            let (code, _) = char_to_keycode(lower)?;
            (code, true)
        }
        '0'..='9' => match c {
            '0' => (11, false),
            '1' => (2, false),
            '2' => (3, false),
            '3' => (4, false),
            '4' => (5, false),
            '5' => (6, false),
            '6' => (7, false),
            '7' => (8, false),
            '8' => (9, false),
            '9' => (10, false),
            _ => unreachable!(),
        },
        ' ' => (57, false),
        '\n' => (28, false),
        '\t' => (15, false),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        ']' => (27, false),
        '\\' => (43, false),
        ';' => (39, false),
        '\'' => (40, false),
        ',' => (51, false),
        '.' => (52, false),
        '/' => (53, false),
        '`' => (41, false),
        _ => return None,
    };
    Some((code, shift))
}

const KEY_ENTER: u16 = 28;
const KEY_TAB: u16 = 15;
const KEY_ESC: u16 = 1;
const KEY_BACKSPACE: u16 = 14;
const KEY_DELETE: u16 = 111;
const KEY_HOME: u16 = 102;
const KEY_END: u16 = 107;
const KEY_PAGEUP: u16 = 104;
const KEY_PAGEDOWN: u16 = 109;
const KEY_UP: u16 = 103;
const KEY_DOWN: u16 = 108;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_INSERT: u16 = 110;
const KEY_SPACE: u16 = 57;
const KEY_LEFTCTRL: u16 = 29;
const KEY_LEFTSHIFT_CODE: u16 = 42;
const KEY_LEFTALT: u16 = 56;
const KEY_LEFTMETA: u16 = 125;

const KEY_MAX: u16 = 0x2ff;

impl VirtualKeyboard {
    pub fn create() -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .map_err(|e| {
                let a = probe();
                anyhow!("cannot open /dev/uinput: {e} ({})", a.reason)
            })?;
        let fd = file.as_raw_fd();

        set_bit(fd, ui_set_evbit(), EV_KEY, "UI_SET_EVBIT(EV_KEY)")?;
        set_bit(fd, ui_set_evbit(), EV_SYN, "UI_SET_EVBIT(EV_SYN)")?;
        // Register the whole keycode range so any mapped character works.
        for code in 1..=KEY_MAX {
            set_bit(fd, ui_set_keybit(), code, "UI_SET_KEYBIT")?;
        }

        let mut name = [0u8; 80];
        let label = b"linux-desktop-mcp keyboard";
        name[..label.len()].copy_from_slice(label);
        let setup = UinputSetup {
            id: InputId {
                bustype: BUS_VIRTUAL,
                vendor: 0x1d6b,
                product: 0x0105,
                version: 1,
            },
            name,
            ff_effects_max: 0,
        };
        write_setup(
            fd,
            ui_dev_setup(),
            &setup as *const UinputSetup as *const u8,
            std::mem::size_of::<UinputSetup>(),
            "UI_DEV_SETUP",
        )?;
        // SAFETY: fd is a valid open uinput descriptor.
        check(unsafe { ioctl(fd, ui_dev_create()) }, "UI_DEV_CREATE")?;
        std::thread::sleep(std::time::Duration::from_millis(150));

        Ok(VirtualKeyboard { file })
    }

    fn event(&mut self, kind: u16, code: u16, value: i32) -> Result<()> {
        let ev = InputEvent {
            tv_sec: 0,
            tv_usec: 0,
            kind,
            code,
            value,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &ev as *const InputEvent as *const u8,
                std::mem::size_of::<InputEvent>(),
            )
        };
        self.file
            .write_all(bytes)
            .context("write to /dev/uinput failed")
    }

    fn syn(&mut self) -> Result<()> {
        self.event(EV_SYN, SYN_REPORT, 0)
    }

    pub fn key(&mut self, code: u16, press: bool) -> Result<()> {
        self.event(EV_KEY, code, if press { 1 } else { 0 })?;
        self.syn()
    }

    /// Tap a keycode, optionally with Shift held (for shifted symbols).
    pub fn tap(&mut self, code: u16, shift: bool) -> Result<()> {
        if shift {
            self.key(KEY_LEFTSHIFT_CODE, true)?;
        }
        self.key(code, true)?;
        self.key(code, false)?;
        if shift {
            self.key(KEY_LEFTSHIFT_CODE, false)?;
        }
        Ok(())
    }

    /// Type a string, one character at a time.
    ///
    /// Returns an error naming the first character that has no US-layout
    /// mapping, instead of silently dropping it.
    pub fn type_str(&mut self, text: &str) -> Result<()> {
        for ch in text.chars() {
            let (code, shift) = char_to_keycode(ch).ok_or_else(|| {
                anyhow!(
                    "no keystroke mapping for {ch:?} (U+{:04X}); non-US-layout text \
                     needs the clipboard path, which this backend does not implement",
                    ch as u32
                )
            })?;
            self.tap(code, shift)?;
        }
        Ok(())
    }

    /// Press a chord such as `ctrl+alt+t`, holding modifiers around the key.
    pub fn chord(&mut self, spec: &str) -> Result<()> {
        let mut mods: Vec<u16> = Vec::new();
        let mut main: Option<u16> = None;
        for raw in spec.split('+') {
            let part = raw.trim();
            if part.is_empty() {
                continue;
            }
            let lower = part.to_ascii_lowercase();
            let code = match lower.as_str() {
                "ctrl" | "control" => KEY_LEFTCTRL,
                "shift" => KEY_LEFTSHIFT_CODE,
                "alt" | "meta" | "mod1" => KEY_LEFTALT,
                "super" | "win" | "mod4" => KEY_LEFTMETA,
                "enter" | "return" => KEY_ENTER,
                "tab" => KEY_TAB,
                "esc" | "escape" => KEY_ESC,
                "space" => KEY_SPACE,
                "backspace" => KEY_BACKSPACE,
                "delete" | "del" => KEY_DELETE,
                "home" => KEY_HOME,
                "end" => KEY_END,
                "pageup" => KEY_PAGEUP,
                "pagedown" => KEY_PAGEDOWN,
                "insert" => KEY_INSERT,
                "up" => KEY_UP,
                "down" => KEY_DOWN,
                "left" => KEY_LEFT,
                "right" => KEY_RIGHT,
                other => {
                    if let Some(rest) = other.strip_prefix('f') {
                        let n: u16 = rest.parse().map_err(|_| anyhow!("unknown key '{part}'"))?;
                        if (1..=12).contains(&n) {
                            // KEY_F1 = 59 .. KEY_F12 = 88, contiguous.
                            58 + n
                        } else {
                            return Err(anyhow!("unsupported function key '{part}'"));
                        }
                    } else if other.chars().count() == 1 {
                        let ch = other.chars().next().unwrap();
                        let (code, _) =
                            char_to_keycode(ch).ok_or_else(|| anyhow!("unknown key '{part}'"))?;
                        code
                    } else {
                        return Err(anyhow!("unknown key '{part}'"));
                    }
                }
            };
            let is_modifier = matches!(
                lower.as_str(),
                "ctrl" | "control" | "shift" | "alt" | "meta" | "mod1" | "super" | "win" | "mod4"
            );
            if is_modifier {
                mods.push(code);
            } else {
                main = Some(code);
            }
        }
        let main = main.ok_or_else(|| anyhow!("no non-modifier key in '{spec}'"))?;
        for &m in &mods {
            self.key(m, true)?;
        }
        self.key(main, true)?;
        self.key(main, false)?;
        for &m in mods.iter().rev() {
            self.key(m, false)?;
        }
        Ok(())
    }

    pub fn destroy(&mut self) {
        // SAFETY: fd belongs to self.file and is still open.
        unsafe {
            ioctl(self.file.as_raw_fd(), ui_dev_destroy());
        }
    }
}

impl Drop for VirtualKeyboard {
    fn drop(&mut self) {
        self.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_abi() {
        // These are ABI; a silent change would break every injection.
        assert_eq!(ui_dev_create(), 0x5501);
        assert_eq!(ui_dev_destroy(), 0x5502);
        assert_eq!(ui_dev_setup(), 0x405c_5503);
        assert_eq!(ui_abs_setup(), 0x401c_5504);
        assert_eq!(ui_set_evbit(), 0x4004_5564);
        assert_eq!(ui_set_keybit(), 0x4004_5565);
        assert_eq!(ui_set_relbit(), 0x4004_5566);
        assert_eq!(ui_set_absbit(), 0x4004_5567);
        assert_eq!(ui_set_propbit(), 0x4004_556e);
    }

    #[test]
    fn struct_sizes_match_the_kernel_abi() {
        if cfg!(target_pointer_width = "64") {
            assert_eq!(std::mem::size_of::<InputEvent>(), 24);
            assert_eq!(std::mem::size_of::<InputAbsinfo>(), 24);
            assert_eq!(std::mem::size_of::<UinputAbsSetup>(), 28);
            assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        }
    }

    #[test]
    fn button_parsing_accepts_names_and_numbers() {
        assert_eq!(Button::parse("left").unwrap(), Button::Left);
        assert_eq!(Button::parse("1").unwrap(), Button::Left);
        assert_eq!(Button::parse("middle").unwrap(), Button::Middle);
        assert_eq!(Button::parse("right").unwrap(), Button::Right);
        assert_eq!(Button::parse("3").unwrap(), Button::Right);
        assert!(Button::parse("sideways").is_err());
    }

    #[test]
    fn ascii_maps_to_evdev_keycodes() {
        assert_eq!(char_to_keycode('a'), Some((30, false)));
        assert_eq!(char_to_keycode('A'), Some((30, true)));
        assert_eq!(char_to_keycode('1'), Some((2, false)));
        assert_eq!(char_to_keycode(' '), Some((57, false)));
        assert_eq!(char_to_keycode('\n'), Some((28, false)));
        assert_eq!(char_to_keycode('中'), None);
    }

    #[test]
    fn availability_probe_never_creates_a_device() {
        let a = probe();
        assert!(!a.reason.is_empty());
        // The reason must always name a stage, so a caller can act on it.
        if !a.usable {
            assert!(
                a.reason.starts_with("uinput-"),
                "reason should be stage-tagged, got {:?}",
                a.reason
            );
        }
    }

    #[test]
    fn process_groups_are_read_without_panicking() {
        // Must never panic regardless of the environment.
        let g = process_groups();
        let _ = g.contains(&0);
    }

    #[test]
    fn input_group_lookup_is_total() {
        let _ = input_gid();
        let _ = user_listed_in_input_group();
    }
}
