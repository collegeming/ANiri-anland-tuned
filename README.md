# ANiri anland tuned

面向 **Droidspaces + Arch Linux + niri/ANiri** 场景优化的 Wayland 合成器。

本项目让 Droidspaces 容器中的 Arch Linux 桌面通过 anland 渲染到 Android 管理的 GPU 缓冲区，并在 ANiri 侧维护低唤醒轮询、重连和剪贴板清空语义。它是完整方案中的 **Linux 合成器与输入终点**；RDP、认证、H.264 编码以及 Android Surface 分发由配套仓库负责。

> 当前验证边界仅到已有的编译检查。本文描述的是批准的组件边界和目标架构，不表示三个仓库已经在任意 Android 设备上完成端到端运行验证、兼容性验证或功耗验证。

## 与上游仓库的关系

| 仓库 | 关系 |
|---|---|
| [`niri-wm/niri`](https://github.com/niri-wm/niri) | niri 原始项目，提供滚动平铺 Wayland 合成器的主体实现 |
| [`Celvra/ANiri`](https://github.com/Celvra/ANiri) | 本仓库的直接代码来源，在 niri 基础上加入 anland 后端和 Android 运行支持 |
| [`superturtlee/anland`](https://github.com/superturtlee/anland) | ANiri 使用的 Android/Linux GPU 缓冲区共享协议与配套实现来源 |
| `collegeming/ANiri-anland-tuned` | 在 ANiri 基础上，针对 Droidspaces、Arch、低唤醒运行和远程访问继续维护的场景化分支 |

由于远程仓库曾删除后重新创建，GitHub 当前不会显示 `Forked from` / `parent` 元数据；这不改变上述实际代码继承关系。本仓库会尽量保留上游 niri/ANiri 的结构、许可证和开发习惯，场景无关的功能应优先回到对应上游讨论。

## 运行环境与术语

Droidspaces 在这里指 **共享 Android 内核的特权 Linux namespace 容器**。Arch 提供的是隔离的 Linux 用户空间；它没有独立客体内核，也不是通过 PRoot 做用户态路径转换的环境。因此本文使用“Android 宿主侧”和“Droidspaces 容器侧”，不使用“Android/PRoot loopback”一类表述。

网络行为取决于 Droidspaces 的网络模式：

- **host network**：容器和 Android 宿主共享 network namespace，因此也共享 loopback；两侧访问的 `127.0.0.1` 是同一个回环接口。
- **NAT network**：容器拥有独立 network namespace，不与 Android 宿主共享 loopback；Android 的 `127.0.0.1` 不是容器的 `127.0.0.1`。要从设备外访问容器内 RDP 监听端口，需由 Droidspaces 或宿主显式把外部 RDP 端口映射到容器的 3389；映射语法取决于实际部署工具。

目标环境由以下部分组成：

1. Android 设备上的 Droidspaces 特权容器和 Arch Linux / Arch Linux ARM 用户空间；
2. 容器内带 anland 后端的 niri/ANiri；
3. Android 侧一个 anland consumer，以及本地 Surface 和远程 MediaCodec input Surface 的按模式分发；
4. 容器内 `lamco-anland-bridge` 提供的外部 RDP 服务。

## 组件职责与三条独立通道

三条通道用途不同，不能把它们统称为同一个“本地桥”或用 TCP/loopback 替代 UDS：

1. **anland 显示与输入通道**
   - Android 侧 anland display daemon 的 Unix domain socket（UDS）通过 bind mount 暴露给容器，ANiri 默认连接 `/run/display.sock`。
   - UDS 用于会话建立、控制和通过 `SCM_RIGHTS` 交接描述符；逐帧热路径使用共享的 **dmabuf、memfd、eventfd 和 fence FD**，而不是经 RDP TCP 回传像素。
   - ANiri 用 EGL 向 consumer 选定的 dmabuf 渲染，并通过 eventfd/fence 与 consumer 同步；anland data socket 承载送入 ANiri 的键盘、指针和剪贴板事件。

2. **Android bridge 与 Lamco 的私有通道**
   - Android 私有目录 `/data/local/tmp/anland-rdp` bind mount 为容器内 `/run/anland-rdp`。
   - Android bridge 和 `lamco-anland-bridge` 只通过该目录中的私有 UDS 交换远程帧、输入、剪贴板和控制消息。
   - 这条通道不是 anland display daemon socket，也不是对外监听的 RDP TCP 端口。

3. **外部 RDP 通道**
   - Windows `mstsc` 连接 `lamco-anland-bridge` 暴露的、带认证的 RDP TCP 服务；RDP/TLS、认证、EGFX AVC420 和 CLIPRDR 位于这一层。
   - host network 下可直接使用设备可达地址上的 TCP 3389；NAT 下需由宿主或容器网络配置把外部 RDP 端口映射到容器 3389。
   - 这是唯一面向外部 PC 的通道。前两条 UDS 不应暴露为网络服务。

## 批准的架构

```text
Windows mstsc
    │
    │ ③ 外部、带认证的 RDP/TLS TCP 3389
    │    host network：设备地址直达
    │    NAT：宿主/容器网络显式映射到容器 3389
    ▼
lamco-anland-bridge（Droidspaces / Arch 容器）
    │
    │ ② 私有目录 UDS
    │    /run/anland-rdp
    │        ⇅ bind mount
    │    /data/local/tmp/anland-rdp（Android）
    ▼
Android anland-bridge / RDP 适配层
    │
    │ ① bind-mounted anland display daemon UDS
    │    容器端默认 /run/display.sock
    │    会话/控制 + SCM_RIGHTS FD 交接
    │    dmabuf / memfd / eventfd / fence FD 热路径
    ▼
ANiri / Smithay（单输出、单个 anland consumer 连接）
    │ EGL：每帧只渲染一次到 Android consumer 选定的 dmabuf
    ▼
Android anland consumer（一个）
    ├─ local
    │    └─ 本地 Android Surface ──► SurfaceFlinger ──► 设备屏幕
    ├─ remote
    │    └─ 远程 MediaCodec input Surface ──► H.264 AVC420 ──► ② ──► ③
    └─ both
         └─ Android GPU 有界 fan-out（每个输出帧两次 GPU draw）
              ├─► 本地 Android Surface ──► SurfaceFlinger ──► 设备屏幕
              └─► 远程 MediaCodec input Surface ──► H.264 AVC420 ──► ② ──► ③
```

图中的 **本地 Android Surface** 是交给 SurfaceFlinger 合成并显示到设备屏幕的目标；**远程 MediaCodec input Surface** 是交给硬件编码器的输入目标。二者职责和生命周期不同，不能画成或实现成同一个 Surface。

RDP 输入和 CLIPRDR 沿图中 ③ → ② 返回 Android bridge，再由 anland data socket 进入 ANiri；ANiri 桌面剪贴板的反向更新按相反方向返回。RDP 层不捕获整个 Android 屏幕。

## Android 运行模式

| 模式 | Android 侧行为 | ANiri/anland 不变量 |
|---|---|---|
| `local` | 有本地 Surface 时直出 SurfaceFlinger；远程 bridge service 停止并拒绝 stream | 一个 anland consumer，ANiri 每帧渲染一次 |
| `remote` | 有 stream 时直出 MediaCodec；无 stream 时有本地 Surface则本地直出，否则停止 display consumer/MediaCodec | 一个 anland consumer，ANiri 每帧渲染一次 |
| `both` | 有 stream 和本地 Surface 时，同一个 consumer 经 Android GPU fan-out 到本地 Surface 与 MediaCodec Surface；每帧执行两次有界 GPU draw（本地 Surface buffer→ring texture、ring texture→MediaCodec input Surface），不做 CPU readback。无本地 Surface 的活动 stream 使用 remote-direct；空闲且无本地 Surface 时停止图形链 | 一个 anland consumer，ANiri 每帧渲染一次 |

这三个模式 **不要求修改 anland daemon 为两个 consumer**，也不让 ANiri 为本地和远程各渲染一次。ANiri 的 anland 实现仍保持单 consumer；`both` 的第二目标由 Android 侧 GPU fan-out 完成。

## 本分支的 ANiri 修改

### 1. 面向刷新率的自适应轮询

ANiri 的 anland 实现仍使用一个 consumer，并保留当前自适应定时轮询策略：

- 活跃连接按显示刷新率的半帧周期轮询；
- 活跃间隔限制在 **2～8 ms**；
- consumer 断开或进入 fallback 时退避到 **500 ms**；
- 重连后恢复活跃间隔。

`get_buffer_ready_fd()` 返回的是 **C context 拥有的借用 raw fd**，Rust/ANiri 不拥有它。fallback 或重连会释放旧会话资源并替换该 fd；如果直接把它注册进 calloop：

- calloop source 可能继续观察已经关闭、甚至被系统复用为其他对象的 fd；
- 把借用 fd 错当成 `OwnedFd` 会引入重复关闭或所有权错误；
- 即使先 `dup()`，副本也只引用旧 eventfd，重连后不会自动切换到新会话的 eventfd。

在没有把“注销旧 source、取得新 fd、注册新 source”做成可靠会话生命周期协议之前，当前实现不会直接注册这个借用 fd。定时器每次通过 C API 取得当前 fd，并以非阻塞方式检查 buffer-ready，同时轮询 data socket；这保持了现有 **2～8 ms / 500 ms** 行为。

该策略减少轮询唤醒是设计目标；实际待机功耗、温度和逐帧延迟仍需在目标设备上测量，本文不把它们声明为已验证结果。

### 2. 可靠读取变长输入消息

anland 的剪贴板和文本输入使用“固定事件头 + 变长 UTF-8 负载”。本分支源码路径确保：

- 完整读取每个变长负载，避免剩余字节破坏后续消息边界；
- 只有 `poll_input_event_extend_data()` 明确返回完整成功时才使用数据；
- 超时或读取失败不会把零填充缓冲区误当成有效剪贴板；
- 非法 UTF-8 不会写入 compositor clipboard。

### 3. 空剪贴板清空

`clipboard.size == 0` 在 ANiri 中表示清空 compositor clipboard，而不是忽略事件。这为文本剪贴板双向同步保留了“空值即清空”的语义。

### 4. 构建配置清理

为 anland 可选音频/相机编译路径声明自定义 cfg，避免新版 Rust 的 `unexpected_cfgs` 噪声，同时不强制引入这些可选组件。这不表示远程音频已经实现；当前 RDP 架构没有 RDPSND。

## 构建与启动

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

设置 `ANLAND` 即选择 anland 后端：

```bash
ANLAND=1 ./target/release/niri --session
```

默认连接 bind-mounted display daemon socket `/run/display.sock`，也可显式指定：

```bash
ANLAND=1 \
ANLAND_SOCKET=/run/display.sock \
./target/release/niri --session
```

可通过 `ANLAND_DRM_DEVICE` 指定 EGL/DRM 渲染设备。实际 Droidspaces namespace、bind mount、KGSL 权限和设备节点配置取决于目标 Android 内核与设备。

## 配套仓库

建议使用相互匹配的三个分支：

| 组件 | 仓库 | 分支 |
|---|---|---|
| ANiri 合成器 | 本仓库 | `anland-poll-optimize` |
| Android/anland bridge | [`collegeming/anland-bridge`](https://github.com/collegeming/anland-bridge) | `bridge-service-toggle` |
| RDP 服务端 | [`collegeming/lamco-anland-bridge`](https://github.com/collegeming/lamco-anland-bridge) | `anland-bridge` |

## RDP 功能边界

当前批准范围是有意收窄的，不应在文档或测试报告中扩展为完整 RDP 功能集：

- **输出**：单个固定尺寸输出；不承诺动态桌面 resize、多显示器或运行时切换分辨率。
- **视频**：只使用 H.264 **AVC420** 路径；不声明 AVC444、软件 OpenH264 fallback 或其他 codec。
- **剪贴板**：只支持 `CF_UNICODETEXT` 纯文本，包括 CJK、emoji 和空文本清空；不支持图片、文件、HTML、RTF 或其他剪贴板格式。
- **音频**：没有 **RDPSND**，不传输远程桌面音频。
- **键盘**：物理键/evdev 路径保留 Win/Super 等按键；非 ASCII 字符的直接 Unicode key event 仍有限制。CJK、emoji 等文本应走 `CF_UNICODETEXT` 剪贴板，不能据此宣称任意 Unicode 直接键入可用。
- **协议职责**：RDP/TLS、认证、EGFX、CLIPRDR 和对外 TCP 监听属于 `lamco-anland-bridge`；ANiri 不实现这些协议。

## 设备验证门槛

已有编译校验不能替代真机验证。至少通过以下门槛后，才能声明对应设备上的实现或运行成功：

- Droidspaces 特权 namespace、Android 内核共享、bind mount、文件权限和 KGSL/DRM 设备访问正确；
- `/run/display.sock` 以及 `/data/local/tmp/anland-rdp -> /run/anland-rdp` 两组 UDS 映射、权限、断开和重连行为正确；
- dmabuf/memfd/eventfd/fence FD 的 `SCM_RIGHTS` 交接、ownership、同步和缓冲区重分配正确；
- 本地 Android Surface 到 SurfaceFlinger 的呈现，以及独立 MediaCodec input Surface 的导入、fence 同步和 AVC420 编码正确；
- `both` 每个输出帧只增加两次有界 Android GPU draw，没有第二个 anland consumer、第二次 ANiri render 或 CPU readback；
- host network 的共享 loopback 与 NAT 的独立 loopback 行为符合预期，NAT 端口映射和外部认证 RDP 访问正确；
- `mstsc` 的 AVC420、固定输出、输入、`CF_UNICODETEXT`（含 CJK、emoji、空清空）兼容性正确；
- Android 版本相关的隐藏 `ANativeWindow` API、MediaCodec 行为、锁屏/后台、Surface 重建和 consumer 重连正确；
- 长时间运行的帧序、延迟、内存/FD 生命周期、功耗、温度和稳定性达到目标。

在这些验证完成并记录前，本仓库只声明代码与编译层面的状态，不声明完整方案已经端到端可用。

## 上游资料

- [niri 官方文档](https://niri-wm.github.io/niri/)
- [Celvra/ANiri](https://github.com/Celvra/ANiri)
- [superturtlee/anland](https://github.com/superturtlee/anland)
- [Droidspaces-OSS](https://github.com/ravindu644/Droidspaces-OSS)

## 许可证

本仓库保留 niri/ANiri 上游许可证及各组件原有版权声明，详见 [`LICENSE`](LICENSE) 与源码文件头。修改和再分发时请同时遵守对应上游项目的许可条款。
