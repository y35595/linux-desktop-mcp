# linux-desktop-mcp

一个快速、轻依赖的 [Model Context Protocol](https://modelcontextprotocol.io) 服务，让 AI agent 在 **Linux 桌面（X11 与 Wayland 均支持）**上获得眼睛和手：截图、鼠标、键盘、窗口控制。

不链接 C 库，不需要系统开发头文件。X11 会话直接支持；Wayland 通过 `xdg-desktop-portal`（截图）加 `/dev/uinput`（输入）支持。单个静态二进制。

简体中文 | [English](README.en.md)

```
操作                                      延迟
────────────────────────────────────────────────────────
XTEST 指针移动（X11）                     0.011 ms
uinput 指针移动（Wayland，建立设备后）    ~0.01 ms
全屏截图 X11 → JPEG → base64              43–55 ms   (3200×2000)
全屏截图 portal → PNG                     约 520–780 ms (3200×2000)
```

## 为什么还要再造一个

各家第一方的 computer use 产品都跳过了 Linux：Claude Code 的 computer use 仅 macOS，ChatGPT/Codex 的 Linux 预览版明确不含桌面控制。而现有能力较强的开源方案每次点击要花 550–880 ms 在 agent 光标动画上，可 XTEST 本身只需要约 0.05 ms——那段时间是动画，不是能力。

本服务把这些开销去掉了：一条常驻连接、进程内截图与编码。

它还修掉了一个在其他方案里会静默破坏坐标准确性的问题：**缩放后的截图，其像素坐标与屏幕坐标并不相同。** 每次截图都会报告精确的缩放比例与原点，并提供 `to_screen` 工具把图像像素换算成屏幕坐标。

## 环境要求

- Linux，**X11 或 Wayland** 会话均可
- 构建需要 Rust 1.75+（不需要 `libX11-dev` / `libXtst-dev`）
- Wayland 下：截图需要 `xdg-desktop-portal`（通常已在运行），输入需要下面的 `/dev/uinput` 权限步骤

## Wayland 支持

Wayland 从设计上禁止普通客户端注入合成输入，因此本服务按任务采用**两套机制**，并通过 `screen_info` 如实报告每次调用由哪个后端提供服务。

| 任务 | X11 会话 | Wayland 会话 |
|---|---|---|
| 屏幕截图 | 根窗口 `GetImage` | XDG portal `Screenshot`（原生分辨率） |
| 指针（绝对定位） | XTEST | uinput 绝对指针设备 |
| 键盘 | XTEST | uinput 键盘 |
| 窗口列表 | EWMH `_NET_CLIENT_LIST` | 仅 Xwayland 客户端（见下） |

**后端选择顺序**

```
截图:  grim（wlroots）→  X11 GetImage（仅真 X11）→  xdg-desktop-portal
输入:  uinput（绝对指针 + 键盘）→  X11 XTEST  →  mutter 仅键盘
```

`grim` 用于 sway/Hyprland/river；GNOME 的 mutter 与 KDE 的 KWin 不实现 `wlr-screencopy`，因此回落到 portal。X11 `GetImage` **只在真 X 服务器**上使用——Xwayland 下根窗口没有后备存储，会返回 `BadMatch`，这正是本项目早期版本在 Wayland 上失效的根因。

### 一次性配置：`/dev/uinput`

Wayland 下控制指针需要 `/dev/uinput`，它的默认权限是 `root:root 0600`，以当前用户身份运行的服务无法打开：

```sh
# 1. 让 input 组可以使用 uinput
sudo tee /etc/udev/rules.d/60-uinput.rules >/dev/null <<'EOF'
KERNEL=="uinput", SUBSYSTEM=="misc", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"
EOF

# 2. 加入该组（之后必须注销并重新登录，仅开新 shell 无效）
sudo usermod -aG input "$USER"
sudo modprobe uinput
```

重新登录后验证：

```sh
ls -l /dev/uinput      # crw-rw---- root input
./linux-desktop-mcp    # stderr 应显示 input='uinput (absolute pointer + keyboard)'
```

**如果装好规则后 `/dev/uinput` 仍是 `crw------- root root 0600`**，说明节点是在规则生效前创建的。无需重启，直接对现有节点应用：

```sh
sudo udevadm control --reload-rules
sudo udevadm trigger --name-match=uinput
```

启动诊断会区分卡在哪一步：

| stderr 标签 | 含义 | 下一步 |
|---|---|---|
| `uinput-missing` | `/dev/uinput` 不存在 | `sudo modprobe uinput` |
| `uinput-stale-node` | 规则已装，但节点仍是 `0600` | 执行上面的 `udevadm trigger` |
| `uinput-relogin` | `/etc/group` 里已在 `input` 组，但当前登录没继承 | 注销并重新登录 |
| `uinput-permission` | 不在 `input` 组 | `usermod -aG input` 后重新登录 |

未完成该步骤时服务仍可运行：截图正常，键盘回落到 mutter；`click`/`mouse_move` 会返回明确指向该步骤的错误，而不是假装成功。

> **安全提示。** `input` 组成员同时能读所有 `/dev/input/event*`，具备记录键盘输入的能力。在共享机器上建议改用只拥有 `/dev/uinput` 的专用组。

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
```

Wayland 下**不要**在 `env` 里固定 `DISPLAY`/`WAYLAND_DISPLAY`（除非确实需要覆盖）：服务会自行探测会话，一个过期的 `DISPLAY` 只会干扰判断。

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

### 锁屏状态

锁屏时无法截图：合成器不会把桌面画进 portal 截图，只会得到一张全黑图。服务会检测该状态，并显式追加 `org.gnome.ScreenSaver GetActive=true` 的警告，而不是返回一张没有解释的黑图。

## 已知限制

- **Wayland 下窗口枚举仅限 Xwayland。** 原生 Wayland 窗口对 X11 故意不可见，因此 `list_windows` 只返回 X11 客户端并会明确提示。基于坐标的点击不受影响。
- **未使用 mutter 的 `RemoteDesktop` 接口控制指针。** GNOME 48 下 `NotifyPointerMotionRelative` 返回成功但指针不动，`NotifyPointerMotionAbsolute` 需要挂载 PipeWire 投屏流。其键盘方法确实有效，故作为键盘回落。实测记录见 `src/mutter.rs`。
- **`type` 在 uinput 后端覆盖 US 布局。** 非拉丁文本会返回精确指出该字符的错误，而不是打错字。
- **没有无障碍树。** 点击基于坐标。「点击名为 Save 的按钮」这类需求需要 AT-SPI 层，本服务选择不做，而不是半成品地做。
- **没有权限确认。** 服务以当前登录用户身份执行。请只在你有意让 agent 操作的桌面上使用。
- **截图不做脱敏。** 屏幕上有什么就截到什么；避免在屏幕上有敏感信息时截图。
- **`scroll` 在 X11 后端使用按钮 4/5/6/7，在 uinput 后端使用 `REL_WHEEL`/`REL_HWHEEL`。**
- **默认单屏。** 跨 RandR 输出的多显示器坐标处理未经测试。

### 已验证配置

Wayland 路径在 GNOME 48 / mutter 48.1、分数缩放 1.7467（原生 3200×2000，逻辑 1832×1145）下实际跑通：

- 截图返回真实桌面内容，且区域截图与全屏同位置裁剪**逐像素相同**
- 合成器缩放由 `DisplayConfig` 读取（1.7467248439788818），不是猜测
- 通过 uinput 键盘发送 **Super** 键可打开 GNOME 概览
- 点击 `y=1870` 时光标落在 `y=1866`——2000px 高的屏幕上误差 4px
- `click` 与 `drag` 都作用于请求坐标下的真实控件

该主机上 `grim` 与 KDE 未验证（没有 wlroots 或 Plasma 会话可用）。

> **测试提示。** 在 Wayland 下 `XQueryPointer` **不是** uinput 后端的有效探测手段：指针归合成器所有，Xwayland 的镜像不跟随注入事件，因此即使点击已正确落点，它仍会报告过期位置。请改用可观察的界面效果来验证。

## 实现说明

| 文件 | 职责 |
|---|---|
| `src/main.rs` | MCP 握手、工具 schema、消息分发循环 |
| `src/platform.rs` | 会话/合成器探测、坐标模型 |
| `src/capture.rs` | 截图后端：grim、X11 `GetImage`、portal |
| `src/input.rs` | 输入后端与自动坐标换算 |
| `src/uinput.rs` | 通过 `/dev/uinput` 的虚拟绝对指针与键盘 |
| `src/dbus.rs` | portal 截图、GNOME DisplayConfig 几何、锁屏状态 |
| `src/mutter.rs` | 基于 mutter `RemoteDesktop` 的仅键盘回落 |
| `src/x11.rs` | XTEST 输入与 EWMH 窗口查询 |
| `src/tools.rs` | 工具实现、参数校验、延迟统计 |
| `src/protocol.rs` | 基于 stdio 的 JSON-RPC 2.0 分帧 |

全链路纯 Rust：[`x11rb`](https://github.com/psychon/x11rb) 直接讲 X11 线协议，[`zbus`](https://github.com/dbus2/zbus) 讲 D-Bus，[`image`](https://github.com/image-rs/image) 在进程内完成编码。没有任何部分链接 libX11 或 libei，因此即使机器上没装开发头文件也能构建。

设置 `LDMCP_DEBUG_PIXELS=1` 可让服务把实际采样到的像素打到 stderr——当截图与外部截图工具结果不一致时很有用。

## 许可证

MIT，详见 [LICENSE](LICENSE)。
