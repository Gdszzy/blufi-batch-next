# BLUFI Batch

Rust 原生桌面批量初始化工具。使用 **egui + eframe / OpenGL** 渲染界面，**btleplug + Tokio** 访问系统蓝牙；不是 WebView，也不依赖原 Web 工具的 WASM、浏览器、Qt、Slint 或 protoc。

Linux 使用 BlueZ / D-Bus，Windows 使用 WinRT。旧工具的初始化操作保留，并接入 `blufi-web` 中的全部五类设备协议。新工具不包含 Web 管理端的文件浏览、业务配置编辑等非批量初始化功能。

## 设备与操作

设备类型筛选提供 **自动选择（默认，扫描全部设备）**、**BLUFI** 和 **Smartcard**：BLUFI 包含新旧 WiFi 与 4G BLUFI，Smartcard 包含 SA01A2 / SA02A1。连接后仍以实际 GATT 服务识别协议；Smartcard 服务优先于 BLUFI 服务。BLUFI 内部沿用 Web 工具的名称规则：`SMCE` 为 4G，其他 `SM` 名称长度为 10 时为新 WiFi，否则为旧 WiFi。

| 设备 | 信息 / 版本 / 容量 / 电量 | 校时、恢复出厂、格式化、清 WiFi | WiFi 信号检查 / 配网 | 专属功能 |
|---|---|---|---|---|
| 旧 WiFi BLUFI | 旧 JSON，兼容 `device_id` / `sn` | 支持 | 支持 | 激活 |
| 新 WiFi BLUFI | protobuf | 支持 | 支持 | 已激活，无需旧激活命令 |
| SMCE 4G BLUFI | protobuf + Modem | 支持 | 支持 | Modem 检查、SN 分配、ICCID 上报、升级触发 |
| SA01A2 | BLE RPC / protobuf | 支持 | 支持 | 按实际服务与特征属性选择通信方向 |
| SA02A1 | BLE RPC / protobuf + LTE 信息 | 支持 | 支持 | Modem 版本检查 |

协议连接 / 握手完成后默认等待 5 秒再读取设备信息，配置中可设为 0–60 秒（0 表示不等待）。支持 Modem 的 SMCE 4G BLUFI 和 SA02A1 必须成功返回非空 Modem 版本与 ICCID；缺失、空值或读取失败都会中止该设备流程。

扫描按项目服务 UUID 筛选。BLUFI 还要求 `SMCD` / `SMCE` 名称前缀，避免把其他产品的通用 BLUFI 服务当作本项目设备；SA01A2 / SA02A1 使用各自专用服务 UUID。进入队列前再次核对当前机型筛选，无关设备和其他已排除机型不会显示或计入异常。

设备服务：

- BLUFI：`0000ffff-0000-1000-8000-00805f9b34fb`
- SA01A2：`99eeb53e-ec41-4472-ac6c-20cc08390000`
- SA02A1：`32b4f7d5-20ce-4c14-9b73-d83e26061800`

混合批次中，激活、自动 SN 分配、升级等**型号专属操作**对不适用设备会记录说明并跳过，不会发送未知命令。Modem 校验仅应用于两类蜂窝设备。工厂固件白名单默认是 `4.1.0`、`4.1.1`，在配置窗口中可按每行一个版本修改并持久化；匹配采用完整版本或点分组件前缀（如白名单 `4.1.0` 可匹配设备版本 `4.1.0.15`，但不匹配 `4.1.01.15`）。仅白名单内的 SMCE 4G 固件可执行已启用的出厂测试校验、SN/ICCID 绑定及升级，其他固件只执行正常初始化步骤。通用步骤失败则整台设备标记失败，不继续后续步骤。

## 构建与运行

使用 Rust stable；本次 Linux 构建使用 `rustc 1.98.1`。提交的 `Cargo.lock` 固定依赖，构建请保留 `--locked`。

### Linux

Fedora：

```sh
sudo dnf install gcc gcc-c++ dbus-devel pkgconf-pkg-config libxkbcommon-devel wayland-devel mesa-libEGL-devel mesa-libGL-devel google-noto-sans-cjk-vf-fonts
```

Ubuntu / Debian：

```sh
sudo apt-get install build-essential pkg-config libdbus-1-dev libxkbcommon-dev libwayland-dev libegl1-mesa-dev libgl1-mesa-dev fonts-noto-cjk
```

```sh
cargo build --locked --release
./target/release/blufi-batch-next
```

要求可用的图形会话、OpenGL 驱动、BlueZ 服务与 BLE 适配器。请以当前桌面用户运行，不要通过 `sudo` 启动 GUI。蓝牙关闭、被 rfkill 屏蔽或 D-Bus 权限不足时，应先修复系统蓝牙状态。

只检查蓝牙，不初始化设备：

```sh
./target/release/blufi-batch-next --diagnose
```

该命令枚举适配器、扫描三秒并停止；只统计本项目支持的设备，不展示周围其他蓝牙设备。

界面固定采用亮色主题，支持 X11 / Wayland。运行时优先加载常规字重的系统中文字体（Noto Sans CJK / Droid Sans Fallback，Windows 使用微软雅黑），避免可变字体被渲染成过细字重；缺少字体时会提示安装。

### Windows

在 Windows 10 / 11 x64 上安装：

1. Rust stable 的 MSVC 工具链。
2. Visual Studio Build Tools 的“使用 C++ 的桌面开发”工作负载与 Windows SDK。
3. 支持 BLE 的蓝牙适配器及正常工作的 Windows 蓝牙驱动。

在 PowerShell 中：

```powershell
cargo build --locked --release
.\target\release\blufi-batch-next.exe
```

Windows 代码使用 btleplug 的真实 WinRT 后端，不是空实现或 Linux 命令替代。界面采用 Windows 原生窗口和系统字体；不需要 BlueZ 或 D-Bus。当前尚未在 Windows 编译或实机测试；`.github/workflows/build.yml` 配置了 Linux / Windows 原生构建与协议测试，但尚无 CI 执行结果。Windows 首次测试应先关闭所有写入操作，确认发现、连接和读取，再逐项开启初始化操作。

## 批次操作

日常操作：**打开程序 → 点击「开始」**。程序自动恢复上次配置并持续处理新设备；点击同一位置的「停止」结束批次。

- 主界面只有工具栏和「处理中 / 成功 / 异常」三列。设备使用约 26px 的单行紧凑列表，同时显示名称、阶段 / SN 和耗时；长内容悬停查看，点击整行打开详情。设备详情按顺序列出该设备已进入的执行步骤，失败或停止时可结合当前状态确认最后一步。默认 1120×720 窗口已用单列 20 条临时队列验证可完整显示。
- 仅需调整流程时打开「配置」；各项按段落完整展示、不折叠，必要时在配置窗口内滚动查看。WiFi 配网关闭时仍显示禁用的输入项；WiFi 信号检查固定显示五行，填写 SSID 的行才生效，无需点击添加。点击「完成」或关闭配置窗口会自动记住设置，不需要另点保存。
- 「更多」中仅保留「运行日志」。失败设备行可右键立即重试；「异常」标题旁有「重试全部」按钮，「成功」标题旁有「清理成功」按钮。
- 配置只在停止时可改。面向专业操作员，保存配置、开始和重试均直接执行，不弹高风险确认或重复警告；真实失败仍明确显示。
- 默认并发 **2**，可设 **1–8**。混合型号固件不同时，关闭统一版本检查或按 BLUFI / Smartcard 分类运行；BLUFI 类别内仍可能包含不同固件版本。
- 配置、详情和日志使用统一的居中弹窗；完成操作在右侧，关闭按钮位于右上角。配置中按 Esc 或点击取消会放弃编辑，点击完成或右上角关闭会记住有效配置。
- 最低电量勾选后才生效，滑块与右侧百分比输入联动，范围为 0–100%；未勾选时保持清晰的禁用显示且不执行电量检查。

容量使用与旧工具相同的 **1024 换算、两位小数**，例如 `7.28G`、`8.00G`；不是 `8GB`，也不是原始字节数。设备详情同时显示格式化容量和原始字节数。最低电量和 WiFi RSSI 阈值均包含边界值；同名 AP 使用最强信号。

默认不启用清空、SN 分配、升级或配网，不内置生产 WiFi 密码；默认开启校时及最低 20% 电量检查。首次连接测试可先关闭校时，只读取与校验信息。

### 去重、重试与停止

- 按系统蓝牙设备标识去重，而不是按可能重复的广播名称去重。
- 同一批次中，失败设备不会因重复广播自动反复执行破坏性操作。
- 成功记录在**当前程序会话**内保留去重；重启程序不保留去重状态。
- 「异常」列表中的失败 / 已停止设备可右键直接重试；会解除该设备的去重，并在下一次扫描到时重新入队。「重试全部」使用上一轮冻结的配置，重跑失败 / 已停止设备的完整流程，并恢复持续发现；不会使用尚未开始的新配置。
- 修改配置后「开始」会允许失败 / 已停止 / 之前跳过的设备重新处理；成功设备仍去重。
- 「重试全部」和「清理成功」在批次运行期间也可用。「清理成功」运行中会先清除界面记录，同时保留当前扫描的去重，避免立即重复执行；下次手动开始批次时再解除这些成功设备的去重。停止后清理则立即解除去重。
- WiFi 尝试次数为明确配置值。每次失败后断开旧会话再重新连接，不重放已成功的清空、SN 等前序步骤。
- 停止会取消排队与执行任务并断开连接；**已经发送到设备的写入不可回滚**。未得到成功确认的操作不会算成功。正常关闭窗口也会等待有界的后台清理。
- 升级等待期间若检测到设备断开，放入「成功」列并标记为「待检查」，显示“设备已断开，检查灯光是否为绿色”；该状态不会触发失败重试。设备明确拒绝、仍连接但超时、CRC / 分片错误仍标记失败。升级触发确认不替代升级后固件版本验收。

### 初始化动作与企业接口

恢复出厂会删除配置与本地文件，格式化会删除存储数据，清 WiFi 会删除保存的网络。配置由专业操作员直接控制，保存和开始时不另行确认。

SMCE 使用企业接口：

- 启用「校验出厂测试」且固件命中白名单时，在修改 SN 前用设备原始 SN 调用 `GET https://erp.gdszzy.com/api/dual-mode/testLog/pass?sn={sn}`。响应必须是布尔值 `true` 才继续；`false` 表示测试失败或没有记录，响应格式错误或请求失败也会中止设备流程。
- 仅对合法的 `4S1000EC` 出厂占位 SN，调用 `GET https://erp.gdszzy.com/api/dual-mode/getSn/{sn}` 分配正式 SN。写入后读回确认，不一致即失败。
- 对 SMCE 启用 Modem 版本检查时，检查通过后调用 `POST https://erp.gdszzy.com/api/dual-mode/setInfo`，JSON 为 `{"sn":"…","ccid":"…"}`，同时更新服务端绑定。
- SA02A1 的 Modem 校验不调用上述企业绑定接口；不推断新机型的企业身份业务规则。
- HTTPS 请求有连接 / 总时限且不跟随重定向。服务器错误、无效 SN、缺失 ICCID 均明确失败，不自动重试身份分配。

## 配置与报告

上次配置会自动记住，位于当前用户的应用配置目录：Linux 默认 `~/.config/blufi-batch-next/settings.json`，尊重 `XDG_CONFIG_HOME`；Windows 位于当前用户 AppData 应用目录。

- WiFi 密码自动记入**系统密码库**：Linux Secret Service（GNOME Keyring / KDE Wallet），Windows Credential Manager。JSON 文件仍不含明文密码，重启后无需重填。
- Linux 需要当前桌面的 Secret Service 可用且已解锁。密码库不可用会明确报错，不降级为明文文件；恢复密码失败时会阻止使用空密码误启动，需在配置中恢复密码。
- 旧版程序没有保存过 WiFi 密码，因此升级后需要在配置中补填一次。旧配置其余字段可直接读取。
- 配置通过同目录临时文件、同步、替换保存，避免先删除旧配置。
- 日志上限 2000 条；界面隐藏本会话已知 WiFi 凭据。显示密码后截图仍可能泄露凭据。

## 协议实现与验证边界

- `src/blufi.rs`：原生 DH1024 / MD5 / AES-CFB128、标准安全控制帧 `0x04`、序号、明文 CRC、分片、旧固件明文响应、旧 JSON / ACK 与新 protobuf。
- `src/rpc.rs`：SA01A2 / SA02A1 严格分片重组、protobuf、设备错误码、15 秒普通请求 / 45 秒配网请求。响应没有请求 ID，超时后会话失效，防止迟到响应用于下一条命令。
- `src/backend.rs`：真实 GATT 发现、特征属性兼容、持续扫描、有界并发、每设备状态、HTTP 业务与取消清理。
- `src/app.rs`：原生 GUI；`src/model.rs` / `src/config.rs`：共享状态、校验与持久化。
- 写入采用 **20 字节安全 ATT 载荷**，不假定平台已协商较大 MTU。初始化数据量小，优先兼容性而非吞吐量。
- BLUFI v1 使用存量设备要求的旧加密协议，DH 交换不认证设备身份；请在可信工厂近场环境使用。该兼容实现不代表现代安全配对协议。

```sh
cargo test --locked
```

此前已通过 23 项协议与配置回归测试；本次新增设备范围过滤后，针对后端运行的 5 项测试全部通过，覆盖无关广播排除、当前机型准入和原有信息检查。真实 Linux BlueZ 项目范围扫描识别到 17 台项目设备，未执行设备初始化。系统密码库跨进程恢复、JSON 不含明文密码及保存配置后直接开始也已验证。

亮色界面以临时视觉验收程序检查了长名称 / 错误信息截断、单列 20 条队列、三类弹窗居中和 900×560 小窗口下的按钮可达性；还验证了动作配置直接保存、切换机型后无关记录隐藏。最低电量滑块在生产程序的隔离配置中验证了 0%、100% 及禁用值的保存。临时数据不代表真实设备初始化结果，验收程序不随工具交付。

**未验证**：五类嵌入式设备各自的完整真机初始化、真实 WiFi 联网、企业 SN / ICCID 接口、固件升级结果及 Windows 平台运行。自动测试与蓝牙扫描不替代这些产线验收。验证过程未执行设备清空、身份分配或升级。

协议来源是相邻 `blufi-web` 的 `src/types/`、连接 composable、`src/proto/*.proto` 与 `docs/ble_rpc_*.md`，初始化业务来源是相邻 `blufi-batch-win/src/main.cpp`。BLUFI 传输参考 [Espressif BLUFI 文档](https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-guides/ble/blufi.html)。
