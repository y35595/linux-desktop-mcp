# linux-desktop-mcp

一个快速、轻依赖的 [Model Context Protocol](https://modelcontextprotocol.io) 服务，让 AI agent 在 **Linux X11 桌面**上获得眼睛和手：截图、鼠标、键盘、窗口控制。

不链接 C 库，不需要系统开发头文件，每次操作都不起子进程。单个静态二进制。

[English](README.md) | 简体中文

```
操作                                延迟
──────────────────────────────────────────────────
XTEST 指针移动                      0.011 ms
XTEST 按键                          0.077 ms
全屏截图 → JPEG → base64            43–55 ms   (3200×2000)
区域截图 400×300                    2.5 ms
```

## 为什么还要再造一个

各家第一方的 computer use 产品都跳过了 Linux：Claude Code 的 computer use 仅 macOS，ChatGPT/Codex 的 Linux 预览版明确不含桌面控制。而现有能力较强的开源方案每次点击要花 550–880 ms 在 agent 光标动画上，可 XTEST 本身只需要约 0.05 ms——那段时间是动画，不是能力。

本服务把这些开销去掉了：一条常驻的 X11 连接、进程内截图与编码、不产生子进程。

它还修掉了一个在其他方案里会静默破坏坐标准确性的问题：**缩放后的截图，其像素坐标与屏幕坐标并不相同。** 每次截图都会报告精确的缩放比例与原点，并提供 `to_screen` 工具把图像像素换算成屏幕坐标。

## 环境要求

- Linux，且为 **X11** 会话（`echo $XDG_SESSION_TYPE` → `x11`）
- 构建需要 Rust 1.75+（不需要 `libX11-dev` / `libXtst-dev`）

**不支持 Wayland**：它从设计上就禁止普通客户端注入合成输入。详见[已知限制](#已知限制)。

## 构建

```sh
git clone https://github.com/y35595/linux-desktop-mcp
cd linux-desktop-mcp
cargo build --release
# 二进制：./target/release/linux-desktop-mcp
```

## 接入客户端

服务通过 stdio 讲 MCP 协议，用你手头的任意客户端注册即可。

### DSH（DeepSeek Harness）

向 `~/.dsh/profiles/<profile>/cordis.patch.yml` 追加：

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

重启 DSH，然后确认客户端已拉起服务进程：

```sh
ps -o pid,etime,cmd -C linux-desktop-mcp
```

### Claude Code

```sh
claude mcp add linux-desktop /absolute/path/to/linux-desktop-mcp
```

### Codex / Cursor / 任意 JSON 配置客户端

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

## 工具列表

| 工具 | 用途 |
|---|---|
| `screenshot` | 截取全屏或指定区域，以图像形式返回 |
| `screenshot_file` | 把 PNG 存到磁盘，不消耗上下文 token |
| `screen_info` | 屏幕分辨率、会话类型、当前活动窗口 |
| `list_windows` | 通过 EWMH `_NET_CLIENT_LIST` 列出顶层窗口 |
| `active_window` | 当前获得焦点的窗口 |
| `focus_window` | 按 id 提升并聚焦某个窗口 |
| `mouse_move` | 把指针移动到绝对屏幕坐标 |
| `click` | 点击，可先移动并支持多击 |
| `mouse_down` / `mouse_up` | 手动按下/抬起，用于自定义拖拽 |
| `drag` | 按下、插值移动、抬起 |
| `scroll` | 在指针处或指定坐标滚动滚轮 |
| `type` | 输入字面文本 |
| `hotkey` | 组合键，例如 `ctrl+alt+t` |
| `to_screen` | 把截图像素换算成屏幕坐标 |
| `desktop_stats` | 累计实测延迟统计 |

所有工具入参都会校验；失败时返回 `isError: true` 并附说明，不会静默什么都不做。

## 坐标：点击之前请先读这段

为节省 token，`screenshot` 默认会缩放到 `max_width=1280`。**此时图像像素不是屏幕像素。** 每次截图的 caption 都会明确写出换算关系：

```
image 1280x800 JPEG 78KB capture 50.7ms
SCALED: screen is 3200x2000 but this image is 1280x800 (scale 0.4000).
Image pixel (u,v) belongs to screen coordinate (0,0) — multiply by 2.5000
before calling click/mouse_move, or pass max_width=3200 to get 1:1 pixels.
```

保持坐标准确的三种方式：

1. **直接要 1:1** —— `screenshot` 传 `max_width: 0`，保持原生像素，此时图像像素即屏幕像素。
2. **换算** —— 调用 `to_screen`，传入 `u`、`v`、`image_width`、`source_width`，返回 `screen_x` / `screen_y`。
3. **相乘** —— 用 caption 里给出的倍率。

截取区域比全分辨率全屏截图更省，同时也能拿到 1:1 的像素：

```json
{ "name": "screenshot", "arguments": { "x": 400, "y": 300, "width": 800, "height": 600 } }
```

## 已知限制

- **仅支持 X11。** 在原生 Wayland 会话下，XTEST 无法触达原生客户端，请改用该合成器对应的方案（`ydotool`、`wlrctl`）。
- **没有无障碍树。** 点击基于坐标。「点击名为 Save 的按钮」这类需求需要 AT-SPI 层，本服务选择不做，而不是半成品地做。
- **没有权限确认。** 服务以当前登录用户身份执行。请只在你有意让 agent 操作的桌面上使用。
- **截图不做脱敏。** 屏幕上有什么就截到什么；避免在屏幕上有敏感信息时截图。
- **`scroll` 使用 XTEST 的按钮 4/5/6/7。** 在多数 GTK/Qt 应用中有效；部分自绘界面会忽略它。
- **默认单屏。** 跨 RandR 输出的多显示器坐标处理未经测试。

## 实现说明

| 文件 | 职责 |
|---|---|
| `src/main.rs` | MCP 握手、工具 schema、消息分发循环 |
| `src/x11.rs` | 截图、XTEST 输入合成、EWMH 窗口查询 |
| `src/tools.rs` | 工具实现、参数校验、延迟统计 |
| `src/protocol.rs` | 基于 stdio 的 JSON-RPC 2.0 分帧 |

全链路纯 Rust：[`x11rb`](https://github.com/psychon/x11rb) 直接讲 X11 线协议，[`image`](https://github.com/image-rs/image) 在进程内完成编码。没有任何部分链接 libX11，因此即使机器上没装 X11 开发头文件也能构建。

设置 `LDMCP_DEBUG_PIXELS=1` 可让服务把实际采样到的像素打到 stderr——当截图与外部截图工具结果不一致时很有用。

## 许可证

MIT，详见 [LICENSE](LICENSE)。
