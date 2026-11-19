# linux-desktop-mcp

A fast, dependency-light [Model Context Protocol](https://modelcontextprotocol.io) server that gives AI agents eyes and hands on a **Linux desktop — both X11 and Wayland**: screenshots, mouse, keyboard, and window control.

No C bindings, no system dev headers, one static binary. Serves X11 directly and Wayland through `xdg-desktop-portal` (capture) plus `/dev/uinput` (input).

English | [简体中文](README.md)

```
operation                              latency
──────────────────────────────────────────────────────────
XTEST pointer move (X11)               0.011 ms
uinput pointer move (Wayland)          ~0.01 ms after setup
full-screen capture, X11 → JPEG        43–55 ms   (3200×2000)
full-screen capture, portal → PNG      ~520–780 ms (3200×2000)
```

## Why another one

The first-party computer-use products skip Linux: Claude Code's computer use is macOS-only, and the ChatGPT/Codex Linux preview explicitly excludes desktop control.

This server fixes the thing that silently breaks coordinate accuracy elsewhere: **a downscaled screenshot has different pixel coordinates than the screen, and on Wayland the capture resolution is not the same space as the input resolution.** Every capture reports its exact scale and origin, every input tool converts automatically, and `to_screen` converts image pixels to screen coordinates.

## Requirements

- Linux with an X11 **or** Wayland session
- Rust 1.75+ to build (no `libX11-dev` / `libXtst-dev` needed)
- On Wayland: `xdg-desktop-portal` (usually already running) for capture, and the `/dev/uinput` permission step below for input

## Wayland support

Wayland deliberately blocks synthetic input for ordinary clients, so this server uses **two different mechanisms** depending on the task, and reports which one served every call through `screen_info`.

| Task | X11 session | Wayland session |
|---|---|---|
| Screen capture | `GetImage` on the root window | XDG portal `Screenshot` (native resolution) |
| Pointer (absolute) | XTEST | uinput absolute pointer |
| Keyboard | XTEST | uinput keyboard |
| Window list | EWMH `_NET_CLIENT_LIST` | Xwayland clients only (see below) |

**How the backends are chosen**

```
capture:  grim (wlroots)  →  X11 GetImage (real X11 only)  →  xdg-desktop-portal
input:    uinput (absolute pointer + keyboard)  →  X11 XTEST  →  mutter keyboard-only
```

`grim` is used on sway/Hyprland/river; GNOME's mutter and KDE's KWin do not implement `wlr-screencopy`, so they fall through to the portal. X11 `GetImage` is used **only** when a real X server owns the screen — under Xwayland it returns `BadMatch` (the root window has no backing store), which is the exact bug that made an earlier version of this project fail on Wayland.

### One-time setup: `/dev/uinput`

Pointer control on Wayland needs `/dev/uinput`. It ships as `root:root 0600`, so an agent running as your user cannot open it:

```sh
# 1. allow the `input` group to use uinput
sudo tee /etc/udev/rules.d/60-uinput.rules >/dev/null <<'EOF'
KERNEL=="uinput", SUBSYSTEM=="misc", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"
EOF

# 2. join the group (then LOG OUT and back in — a new shell is not enough)
sudo usermod -aG input "$USER"
sudo modprobe uinput
```

Verify after re-login:

```sh
ls -l /dev/uinput                                  # crw-rw---- root input
./linux-desktop-mcp   # stderr should say input='uinput (absolute pointer + keyboard)'
```

Until this is done the server still runs: capture works, and keyboard input falls back to mutter. `click`/`mouse_move` return a clear error naming this step rather than pretending to work.

**If `/dev/uinput` still shows `crw------- root root 0600` after installing the rule**, the node was created before the rule took effect. Apply it to the existing node without rebooting:

```sh
sudo udevadm control --reload-rules
sudo udevadm trigger --name-match=uinput
```

The startup diagnostic distinguishes the stages, so you can tell which one you are in:

| stderr tag | meaning | next step |
|---|---|---|
| `uinput-missing` | `/dev/uinput` absent | `sudo modprobe uinput` |
| `uinput-stale-node` | rule installed, node still `0600` | `udevadm trigger` above |
| `uinput-relogin` | you are in the `input` group in `/etc/group`, but this login has not inherited it | log out and back in |
| `uinput-permission` | not in the `input` group | `usermod -aG input`, then re-login |

> **Security note.** Membership of `input` also grants read access to every `/dev/input/event*` device, which is keylogging-capable. On a shared machine prefer a dedicated group that owns only `/dev/uinput`.

## Build

```sh
git clone https://github.com/y35595/linux-desktop-mcp
cd linux-desktop-mcp
cargo build --release
# binary: ./target/release/linux-desktop-mcp
```

## Install into a client

The server speaks MCP over stdio. Register it with whichever client you use.

### DSH (DeepSeek Harness)

Append to `~/.dsh/profiles/<profile>/cordis.patch.yml`:

```yaml
- insert:
    - id: mcp-linux-desktop
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        serverName: linux-desktop
        transport: stdio
        command: /absolute/path/to/linux-desktop-mcp
        args: []
        toolCallTimeoutMs: 30000
        failOnStartupError: false
        reconnectEnabled: true
        reconnectMaxAttempts: 3
```

On Wayland, **do not** pin `DISPLAY`/`WAYLAND_DISPLAY` in `env` unless you need to override them; the server detects the session itself and a stale `DISPLAY` only confuses detection.

Restart DSH, then confirm the client spawned the server:

```sh
ps -o pid,etime,cmd -C linux-desktop-mcp
```

### Claude Code

```sh
claude mcp add linux-desktop /absolute/path/to/linux-desktop-mcp
```

### Codex / Cursor / any JSON-config client

```json
{
  "mcpServers": {
    "linux-desktop": {
      "command": "/absolute/path/to/linux-desktop-mcp",
      "args": []
    }
  }
}
```

## Tools

| Tool | Purpose |
|---|---|
| `screenshot` | Capture the screen or a region, return it as an image |
| `screenshot_file` | Save a PNG to disk without spending context tokens |
| `screen_info` | Session, compositor, backends, coordinates, scale, lock state |
| `list_windows` | Top-level windows (X11 / Xwayland clients only on Wayland) |
| `active_window` | The currently focused X11 window |
| `focus_window` | Raise and focus a window by id |
| `mouse_move` | Move the pointer to absolute screen coordinates |
| `click` | Click, with optional move and multi-click count |
| `mouse_down` / `mouse_up` | Manual press/release for custom drags |
| `drag` | Press, interpolate, release |
| `scroll` | Wheel at the pointer or given coordinates |
| `type` | Type literal text |
| `hotkey` | Key combinations such as `ctrl+alt+t` |
| `to_screen` | Convert screenshot pixels to screen coordinates |
| `desktop_stats` | Cumulative measured latency and active backends |

Every tool input is validated; failures return `isError: true` with a message rather than silently doing nothing.

Start with `screen_info`: it names the active capture and input backends, so you can tell before clicking whether the pointer will actually move.

## Coordinates: read this before clicking

There is exactly **one** coordinate space for all input tools: **native capture pixels**. If you read pixel `(u, v)` off a native-resolution screenshot and pass it to `click`, it lands on that pixel. On a HiDPI Wayland session the native resolution differs from the logical input space (e.g. 3200×2000 physical vs 1832×1145 logical at scale 1.7467); the server converts per backend and does it for you.

`screenshot` downscales to `max_width=1280` by default for token efficiency. **Image pixels are then not screen pixels.** Each capture's caption states the mapping explicitly:

```
image 1280x800 JPEG 78KB capture 50.7ms via xdg-desktop-portal Screenshot
coordinates: this image and all input tools share one space — a pixel read
here (plus the origin) is the screen coordinate to pass to click/mouse_move.
SCALED for display only: the native screen is 3200x2000 while this image is
1280x800 (scale 0.4000). Multiply image pixels by 2.5000 to get screen
coordinates, or pass max_width=0 for 1:1 pixels.
HiDPI: native pixels are 1.747x the logical input space (1.747). This is
handled automatically.
```

Three ways to stay accurate:

1. **Ask for 1:1** — `screenshot` with `max_width: 0` keeps native pixels, so image pixel = screen pixel.
2. **Convert** — call `to_screen` with `u`, `v`, `image_width`, and `source_width`; it returns `screen_x`/`screen_y`.
3. **Multiply** — use the factor printed in the caption.

### Locked session

A locked screen cannot be captured: the compositor will not paint the desktop into a portal screenshot, so you get a black frame. The server detects this and appends an explicit warning naming `org.gnome.ScreenSaver GetActive=true`, rather than handing back an unexplained black image.

## Scope and limits

- **Window enumeration on Wayland is Xwayland-only.** Native Wayland windows are intentionally invisible to X11, so `list_windows` returns X11 clients only and says so. Coordinate-based clicks work regardless.
- **No accessibility tree.** Clicks are coordinate-based. "Click the button labelled Save" needs an AT-SPI layer, which this server deliberately omits rather than half-implementing.
- **No permission prompts.** The server acts as the logged-in user. Point it at a desktop you intend an agent to drive.
- **No screenshot redaction.** Whatever is on screen gets captured; avoid capturing with secrets visible.
- **mutter's `RemoteDesktop` interface is not used for the pointer.** On GNOME 48 `NotifyPointerMotionRelative` returns success while moving nothing, and `NotifyPointerMotionAbsolute` needs an attached PipeWire screencast. Its keyboard methods do work, so they back the keyboard fallback. See `src/mutter.rs` for the measurements.
- **`type` on the uinput backend covers the US layout.** Non-Latin text returns a precise error naming the character instead of typing the wrong thing.
- **One screen assumed.** Multi-monitor coordinate handling across outputs is untested.

### Verified configuration

The Wayland path is exercised on GNOME 48 / mutter 48.1 with fractional scaling at 1.7467 (3200×2000 native, 1832×1145 logical):

- capture returns real desktop content, and a region capture is pixel-identical to the same crop of a full capture
- the compositor scale is read from `DisplayConfig` (1.7467248439788818), not guessed
- a synthetic **Super** keypress through the uinput keyboard opens the GNOME overview
- a click at `y=1870` places the text caret at `y=1866` — 4 px on a 2000 px-tall screen
- `click` and `drag` both act on the widget under the requested coordinate

`grim` and KDE are not exercised on that host (no wlroots or Plasma session was available).

> **Testing note.** `XQueryPointer` is not a valid probe for the uinput backend on Wayland: the compositor owns the pointer and Xwayland's mirror does not follow injected events, so it reports a stale position even while clicks land correctly. Verify with an observable UI effect instead.

## How it works

| File | Role |
|---|---|
| `src/main.rs` | MCP handshake, tool schemas, dispatch loop |
| `src/platform.rs` | Session/compositor detection, the coordinate model |
| `src/capture.rs` | Capture backends: grim, X11 `GetImage`, portal |
| `src/input.rs` | Input backends and automatic coordinate conversion |
| `src/uinput.rs` | Virtual absolute pointer + keyboard via `/dev/uinput` |
| `src/dbus.rs` | Portal screenshot, GNOME DisplayConfig geometry, lock state |
| `src/mutter.rs` | Keyboard-only fallback via mutter `RemoteDesktop` |
| `src/x11.rs` | XTEST input and EWMH window queries |
| `src/tools.rs` | Tool implementations, validation, latency accounting |
| `src/protocol.rs` | JSON-RPC 2.0 framing over stdio |

Pure Rust end to end: [`x11rb`](https://github.com/psychon/x11rb) speaks the X11 wire protocol, [`zbus`](https://github.com/dbus2/zbus) speaks D-Bus, and [`image`](https://github.com/image-rs/image) encodes in-process. Nothing links against libX11 or libei, so the crate builds on machines without development headers.

Set `LDMCP_DEBUG_PIXELS=1` to have the server print what it sampled to stderr — useful when a screenshot disagrees with an external capture tool.

## License

MIT. See [LICENSE](LICENSE).
