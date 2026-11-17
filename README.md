# linux-desktop-mcp

A fast, dependency-light [Model Context Protocol](https://modelcontextprotocol.io) server that gives AI agents eyes and hands on a **Linux X11 desktop**: screenshots, mouse, keyboard, and window control.

No C bindings, no system dev headers, no subprocess per action. One static binary.

```
operation                          latency
──────────────────────────────────────────────────
XTEST pointer move                 0.011 ms
XTEST key press                    0.077 ms
full-screen capture → JPEG → b64   43–55 ms   (3200×2000)
region capture 400×300             2.5 ms
```

## Why another one

The first-party computer-use products skip Linux: Claude Code's computer use is macOS-only, and the ChatGPT/Codex Linux preview explicitly excludes desktop control. The capable open-source option spends 550–880 ms per click on an agent-cursor glide, while XTEST itself needs ~0.05 ms — that time is animation, not capability.

This server removes the overhead: one persistent X11 connection, in-process capture and encoding, and no subprocess spawns.

It also fixes the thing that silently breaks coordinate accuracy elsewhere: **a downscaled screenshot has different pixel coordinates than the screen.** Every capture reports its exact scale and origin, and a `to_screen` tool converts image pixels to screen coordinates.

## Requirements

- Linux with an **X11** session (`echo $XDG_SESSION_TYPE` → `x11`)
- Rust 1.75+ to build (no `libX11-dev` / `libXtst-dev` needed)

Wayland is **not** supported: it blocks synthetic input for ordinary clients by design. See [Limitations](#limitations).

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
        env:
          DISPLAY: ':0'
```

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
      "args": [],
      "env": { "DISPLAY": ":0" }
    }
  }
}
```

## Tools

| Tool | Purpose |
|---|---|
| `screenshot` | Capture the screen or a region, return it as an image |
| `screenshot_file` | Save a PNG to disk without spending context tokens |
| `screen_info` | Screen geometry, session type, active window |
| `list_windows` | Top-level windows via EWMH `_NET_CLIENT_LIST` |
| `active_window` | The currently focused window |
| `focus_window` | Raise and focus a window by id |
| `mouse_move` | Move the pointer to absolute screen coordinates |
| `click` | Click, with optional move and multi-click count |
| `mouse_down` / `mouse_up` | Manual press/release for custom drags |
| `drag` | Press, interpolate, release |
| `scroll` | Wheel at the pointer or given coordinates |
| `type` | Type literal text |
| `hotkey` | Key combinations such as `ctrl+alt+t` |
| `to_screen` | Convert screenshot pixels to screen coordinates |
| `desktop_stats` | Cumulative measured latency |

Every tool input is validated; failures return `isError: true` with a message rather than silently doing nothing.

## Coordinates: read this before clicking

`screenshot` downscales to `max_width=1280` by default for token efficiency. **Image pixels are then not screen pixels.** Each capture's caption states the mapping explicitly:

```
image 1280x800 JPEG 78KB capture 50.7ms
SCALED: screen is 3200x2000 but this image is 1280x800 (scale 0.4000).
Image pixel (u,v) belongs to screen coordinate (0,0) — multiply by 2.5000
before calling click/mouse_move, or pass max_width=3200 to get 1:1 pixels.
```

Three ways to stay accurate:

1. **Ask for 1:1** — `screenshot` with `max_width: 0` keeps native pixels, so image pixel = screen pixel.
2. **Convert** — call `to_screen` with `u`, `v`, `image_width`, and `source_width`; it returns `screen_x`/`screen_y`.
3. **Multiply** — use the factor printed in the caption.

Capture a region to get 1:1 pixels at a lower cost than a full-resolution screen grab:

```json
{ "name": "screenshot", "arguments": { "x": 400, "y": 300, "width": 800, "height": 600 } }
```

## Scope and limits

- **X11 only.** Under a native Wayland session, XTEST cannot reach native clients. Use a compositor-specific route (`ydotool`, `wlrctl`) there.
- **No accessibility tree.** Clicks are coordinate-based. "Click the button labelled Save" needs an AT-SPI layer, which this server deliberately omits rather than half-implementing.
- **No permission prompts.** The server acts as the logged-in user. Point it at a desktop you intend an agent to drive.
- **No screenshot redaction.** Whatever is on screen gets captured; avoid capturing with secrets visible.
- **`scroll` uses XTEST button 4/5/6/7.** It works in most GTK/Qt applications; some self-rendering surfaces ignore it.
- **One screen assumed.** Multi-monitor coordinate handling across RandR outputs is untested.

## How it works

| File | Role |
|---|---|
| `src/main.rs` | MCP handshake, tool schemas, dispatch loop |
| `src/x11.rs` | Capture, XTEST input synthesis, EWMH window queries |
| `src/tools.rs` | Tool implementations, argument validation, latency accounting |
| `src/protocol.rs` | JSON-RPC 2.0 framing over stdio |

Pure Rust end to end: [`x11rb`](https://github.com/psychon/x11rb) speaks the X11 wire protocol directly and [`image`](https://github.com/image-rs/image) encodes in-process. Nothing links against libX11, so the crate builds on machines without X11 development headers.

Set `LDMCP_DEBUG_PIXELS=1` to have the server print what it sampled to stderr — useful when a screenshot disagrees with an external capture tool.

## License

MIT. See [LICENSE](LICENSE).
