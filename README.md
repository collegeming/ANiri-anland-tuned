# ANiri anland tuned

面向 **Droidspaces + Arch Linux + niri/ANiri** 场景优化的 Wayland 合成器。

本项目让运行在 Android Droidspaces 容器中的 Arch Linux 桌面可以通过 anland 直接渲染到 Android 提供的 GPU 缓冲区，并针对长期运行和远程 RDP 访问补充了低唤醒轮询、可靠重连与完整剪贴板清空语义。

> 本仓库不是通用 niri 的替代发行版，也不负责 RDP 编码。它是整套远程访问方案中的 **Linux 合成器与输入终点**。

## 与上游仓库的关系

| 仓库 | 关系 |
|---|---|
| [`niri-wm/niri`](https://github.com/niri-wm/niri) | niri 原始项目，提供滚动平铺 Wayland 合成器的主体实现 |
| [`Celvra/ANiri`](https://github.com/Celvra/ANiri) | 本仓库的直接代码来源，在 niri 基础上加入 anland 后端和 Android 运行支持 |
| [`superturtlee/anland`](https://github.com/superturtlee/anland) | ANiri 使用的 Android/Linux GPU 缓冲区共享协议与配套实现来源 |
| `collegeming/ANiri-anland-tuned` | 在 ANiri 基础上，针对 Droidspaces、Arch、低功耗运行和远程访问继续维护的场景化分支 |

由于远程仓库曾删除后重新创建，GitHub 当前不会显示 `Forked from` / `parent` 元数据；这不改变上述实际代码继承关系。本仓库会尽量保留上游 niri/ANiri 的结构、许可证和开发习惯，场景无关的功能应优先回到对应上游讨论。

## 改造背景

目标运行环境不是传统 Linux PC，而是：

1. Android 设备通过 **Droidspaces** 运行接近 LXC 形态的 Linux 容器；
2. 容器用户空间使用 **Arch Linux / Arch Linux ARM**；
3. 桌面使用带 anland 后端的 **niri/ANiri**；
4. ANiri 不直接驱动物理显示器，而是把桌面渲染到 Android consumer 提供的 dmabuf；
5. 用户希望在 Windows PC 上通过系统自带的 `mstsc` 远程访问这个 Linux 桌面。

这个场景对远程访问有几项特殊要求：

- 只传输 ANiri/anland 的 Linux 桌面，不能录制或发送整个 Android 屏幕；
- 视频由 Android `MediaCodec` 硬件 H.264 编码，不能在 Arch 容器里使用 OpenH264 软件编码；
- 键盘、鼠标、滚轮和剪贴板要直接进入 ANiri；
- PC 的 Win/Super 键必须保持原值，例如 `Win+E` 到达 niri 后仍是 `Mod+E`；
- 空剪贴板也必须能同步为“清空”，不能被当成无效事件丢弃；
- 本地无 consumer 或远程会话断开时，不能继续用 1 ms 定时器高频唤醒 CPU。

为完成整个链路，本项目与以下两个仓库配套：

- [`collegeming/anland-bridge`](https://github.com/collegeming/anland-bridge)：Android consumer、硬件编码和本地认证桥；
- [`collegeming/lamco-anland-bridge`](https://github.com/collegeming/lamco-anland-bridge)：向 `mstsc` 提供 RDP/TLS、EGFX AVC420、输入和 CLIPRDR。

## 本仓库负责什么

```text
Windows mstsc
    │ RDP 输入 / CLIPRDR
    ▼
lamco-anland-bridge（Arch 容器）
    │ 本地认证桥
    ▼
Android anland consumer
    │ anland data socket：真实 evdev 按键、指针、剪贴板
    ▼
ANiri / Smithay
    │ EGL 渲染
    ▼
Android 提供的 dmabuf / MediaCodec input Surface
```

ANiri 是最终的 Wayland 合成器、输入处理者和桌面剪贴板拥有者。RDP 协议、TLS 和 H.264 封装不在本仓库实现。

## 本次主要修改

### 1. 面向刷新率的自适应轮询

ANiri 原先固定每 1 ms 轮询 anland consumer。对于 60～120 Hz 显示链路，大部分唤醒都拿不到新缓冲区，只会增加容器 CPU 唤醒和手机发热。

当前策略：

- 活跃连接按显示刷新率的半帧周期轮询；
- 轮询间隔限制在 **2～8 ms**；
- consumer 断开或处于 fallback 时退避到 **500 ms**；
- 重连后自动恢复活跃间隔；
- 不接管 C 层拥有且会在重连时替换的借用文件描述符，避免把失效 fd 注册进 calloop。

典型情况下，60 Hz 从约 1000 次/秒降到约 125 次/秒，120 Hz 降到约 250 次/秒，同时仍保持每帧约两次采样。

### 2. 可靠的变长输入消息

anland 的剪贴板和文本输入使用“固定事件头 + 变长 UTF-8 负载”。本分支确保：

- 每个变长负载都被完整读取，避免剩余字节破坏后续消息边界；
- 只有 `poll_input_event_extend_data()` 明确返回完整成功时才使用数据；
- 超时或读取失败不会把零填充缓冲区误当成有效剪贴板；
- 非法 UTF-8 不会写入 compositor clipboard。

### 3. 空剪贴板清空

`clipboard.size == 0` 现在具有明确语义：清空 ANiri 的 compositor clipboard。

这使 `mstsc → Android → ANiri` 和反向链路都可以正确同步“清空剪贴板”，而不只是非空文本。

### 4. 构建配置清理

为 anland 可选音频/相机编译路径声明自定义 cfg，避免新版 Rust 的 `unexpected_cfgs` 噪声，同时不强制引入这些可选组件。

## 可实现的效果

与另外两个配套仓库一起使用时，可以实现：

- 在 Droidspaces 的 Arch 容器里运行完整 ANiri/niri 桌面；
- ANiri 继续直接渲染到 Android 管理的 GPU 缓冲区；
- Windows 使用标准 `mstsc` 访问，不需要自定义 PC 客户端；
- 只传输 Linux 桌面，不捕获 Android 主屏幕；
- PC 键盘、鼠标、滚轮直接进入 Smithay/anland 输入路径；
- Win/Super 键不做 Alt 映射，niri `Mod` 快捷键可以按原值触发；
- UTF-8 文本和空剪贴板双向同步；
- 本地 consumer 断开时降低轮询频率，减少无效唤醒和待机发热；
- consumer 重连、缓冲区重分配后重新导入 dmabuf 并请求重绘。

## 构建

首先按 niri 上游文档安装 Rust 与 Wayland 合成器构建依赖：

- [niri Getting Started](https://niri-wm.github.io/niri/Getting-Started.html)
- [niri Packaging](https://github.com/niri-wm/niri/wiki/Packaging-niri)

在 Arch 环境中构建：

```bash
git clone https://github.com/collegeming/ANiri-anland-tuned.git
cd ANiri-anland-tuned
git switch anland-poll-optimize
cargo build --release
```

开发检查：

```bash
cargo fmt --all -- --check
cargo check
```

## 启动 anland 后端

设置 `ANLAND` 即选择 anland 后端：

```bash
ANLAND=1 ./target/release/niri --session
```

默认 daemon socket 为 `/run/display.sock`，也可以显式指定：

```bash
ANLAND=1 \
ANLAND_SOCKET=/run/display.sock \
./target/release/niri --session
```

可通过 `ANLAND_DRM_DEVICE` 指定 EGL/DRM 渲染设备。实际 Droidspaces 启动脚本、KGSL 权限和 socket 映射取决于设备内核与容器配置。

## 配套仓库

建议使用相互匹配的三个分支：

| 组件 | 仓库 | 分支 |
|---|---|---|
| ANiri 合成器 | 本仓库 | `anland-poll-optimize` |
| Android/anland bridge | [`collegeming/anland-bridge`](https://github.com/collegeming/anland-bridge) | `bridge-service-toggle` |
| RDP 服务端 | [`collegeming/lamco-anland-bridge`](https://github.com/collegeming/lamco-anland-bridge) | `anland-bridge` |

## 当前边界

- 这是针对单个 anland 输出的场景化实现，不等同于 niri 上游支持矩阵；
- RDP 视频编码、TLS 和认证由配套仓库完成；
- 非 ASCII 的直接 Unicode 按键仍受当前 evdev wire path 限制，Unicode 文本可通过剪贴板完整传输；
- Android 隐藏 `ANativeWindow` API、MediaCodec Surface dmabuf 导入、Android/proot loopback、`mstsc` 兼容性和整机热表现必须在目标手机上验证；
- 同时进行本地显示与远程编码需要额外 EGL fan-out，本方案为降低功耗采用互斥的本地/远程 consumer。

## 上游资料

- [niri 官方文档](https://niri-wm.github.io/niri/)
- [Celvra/ANiri](https://github.com/Celvra/ANiri)
- [superturtlee/anland](https://github.com/superturtlee/anland)
- [Droidspaces-OSS](https://github.com/ravindu644/Droidspaces-OSS)

## 许可证

本仓库保留 niri/ANiri 上游许可证及各组件原有版权声明，详见 [`LICENSE`](LICENSE) 与源码文件头。修改和再分发时请同时遵守对应上游项目的许可条款。
