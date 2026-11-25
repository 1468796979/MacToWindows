# RustDesk 文本通道与 Deskflow 映射兼容性

本轮修正保留普通成对按键流的 Windows 映射，覆盖本地键盘、Deskflow，以及 RustDesk 的 Map / Translate `Chr` 路径；对 RustDesk 1.5.3 已确认的 Enigo marker 100 键盘通道原样透传。来源 PID、来源状态或“由软件生成”不再作为禁止映射的门槛。

用户已确认当前安装的旧版 MacToWindows 能配合 Deskflow 正常执行 Windows 映射，这是本次修正需要保持的行为。本轮只修改源码并进行自动化验证，不生成安装或发布包，不安装、替换本机 App，也不执行签名、公证或发布。下文记录本轮自动回归结果；修正后的人工验收仍待执行，不能用隔离测试结果代替真实会话验收。

## Deskflow：保留 HID 与 CoreGraphics 回退路径

已核对本机 Deskflow 的公开版本元数据为 1.27.0.0。官方 v1.27.0 的 [`src/lib/platform/OSXKeyState.cpp`](https://github.com/deskflow/deskflow/blob/v1.27.0/src/lib/platform/OSXKeyState.cpp#L599-L648) 显示：`postHIDVirtualKey()` 使用 `IOHIDPostEvent`；`postKeyboardKey()` 使用 `CGEventCreateKeyboardEvent` 和事件发送；`fakeKey()` 在 HID 发送失败时回退到 CoreGraphics。

两条路径均需继续按原有按下/释放语义处理，不能只保留 HID 路径的映射。自动回归应覆盖 `user_data = 0`、来源 PID 为正、来源状态为 0（CombinedSessionState）的 CoreGraphics 成对事件；这些是兼容性测试条件，不代表已抓取或确认真实 Deskflow 会话中每个事件都带有相同元数据。

## RustDesk 1.5.3 的已确认通道

已核对本机 `/Applications/RustDesk.app` 的公开版本元数据：RustDesk 1.5.3，build 71。没有读取个人配置、连接凭据或远程会话内容。用户的实际键盘模式尚未确认。

官方 1.5.3 标签对应提交 `95cca3f661f635b3cc85c04935fe8d96d8f432ef`。该版本 Enigo 的 `key_sequence()` 按 Unicode 字素创建虚拟键码 0、仅 Down 的原生事件并附带文本，不发送配对 Up；`post_event()` 为 Enigo 键盘通道设置 `EVENT_SOURCE_USER_DATA = 100`。实现见 [`libs/enigo/src/macos/macos_impl.rs`](https://github.com/rustdesk/rustdesk/blob/95cca3f661f635b3cc85c04935fe8d96d8f432ef/libs/enigo/src/macos/macos_impl.rs)。

原有按键状态机会在第一次 Down 后登记键码绑定，并抑制没有 Up 的后续同键码 Down。文本通道因此可能把不同字符当成一个始终按住的物理键；重建按键还会丢失原始 Unicode 载荷。此次透传完整 marker 100 键盘通道，保留原生文本和事件顺序，避免在同一通道中只旁路部分按键。

| RustDesk 1.5.3 接收路径 | macOS 注入方式 | 本轮处理 |
| --- | --- | --- |
| Legacy：普通 `Chr`，不含热键修饰键 | Enigo `key_sequence()` | marker 100 键盘通道原样透传 |
| Legacy：`Unicode`、`Seq` | Enigo `key_sequence()` | marker 100 键盘通道原样透传 |
| Legacy：热键字符及控制键 | Enigo 布局键或控制键的 Down/Up | 同属 marker 100 通道，也原样透传 |
| Translate：`Seq` | Enigo `key_sequence()` | marker 100 键盘通道原样透传 |
| Translate：`Chr` | rdev 原始 macOS 虚拟键码 Down/Up | 继续原有 Windows 映射 |
| Map | rdev 原始 macOS 虚拟键码 Down/Up | 继续原有 Windows 映射 |

分派见固定提交的 [`src/server/input_service.rs`](https://github.com/rustdesk/rustdesk/blob/95cca3f661f635b3cc85c04935fe8d96d8f432ef/src/server/input_service.rs) 中 `legacy_keyboard_mode()`、`translate_keyboard_mode()`、`map_keyboard_mode()`。该版本 Translate 的 `Unicode` 分支未处理，本轮不改变 RustDesk 自身的这一行为。

RustDesk 的 [`Cargo.lock`](https://github.com/rustdesk/rustdesk/blob/95cca3f661f635b3cc85c04935fe8d96d8f432ef/Cargo.lock) 锁定 rdev 提交 `0904f23ce496e255461a98848b85c11fa5a62eab`。其 [`src/macos/simulate.rs` 的 `VirtualInput::simulate()`](https://github.com/rustdesk-org/rdev/blob/0904f23ce496e255461a98848b85c11fa5a62eab/src/macos/simulate.rs) 没有显式设置 Enigo marker 100。不能将所有 RustDesk 按键都归入文本旁路，也不能把当前策略描述为隔离全部远程输入。

## 路由契约与限制

`EventTapEvent::keyboard_route(own_marker)` 对 keyDown、keyUp、flagsChanged（原生类型 10、11、12）判定路由；其他事件返回 `None`，继续既有分支。自身 marker 的判断优先。

| 路由 | 条件 | 处理 |
| --- | --- | --- |
| `OwnInjected` | 非零 user data 与本程序 marker 相同，包括自身 marker 恰为 100 的情况 | 沿用回环防护，不再次映射 |
| `TextInjection` | 自身 marker 判断之后，user data 为 100 的键盘事件 | 原始事件透传，不进入归一化状态跟踪、剪贴板热键或规则引擎 |
| `Mappable` | 其他可读取的键盘事件，无 PID 或来源状态门槛 | 继续原有归一化、热键和 Windows 映射 |
| `Unknown` | 无法读取原生键盘事件，或不支持该原生读取的环境 | 安全透传 |

`TextInjection` 是本项目的通道路由名称，不表示该通道每个事件都携带 Unicode 文本。路由只依赖已确认的 user data 约定，不读取实际字符，不以虚拟键码 0、PID、来源状态或按键是否成对进行猜测。普通软件按键流即使来源 PID 为正、来源状态为 CombinedSessionState，也保持可映射。

marker 100 是工程上的兼容约定，不是认证身份，也不唯一属于 RustDesk 或某个进程。其他采用相同 marker 的工具同样会旁路本项目键盘映射。经虚拟 HID 提交、表现与普通硬件一致的事件也不会因“软件来源”而被隔离。

本轮只解决已确认的 marker 100 通道兼容问题。未知软件若以其他 marker 发送不配对的 Down，仍可能遇到原有按键状态风险；新版或修改版 RustDesk 改变注入约定后也需要重新验证。本项目不据此承诺修复所有远控或所有文本注入实现。

`TextInjection` / `Unknown` 旁路保留原生 Unicode 载荷、flags 和 Down/Up 形式，不重建键盘事件。旁路不等待运行时互斥锁：回调尝试非阻塞取消本地待识别手势，锁忙碌时以原子标记保留请求，在下次可映射事件进入引擎前处理。不清除仍按住的映射键；自身回放不取消本地手势。普通 Deskflow 和 RustDesk Map 按键继续走原有映射与手势处理。鼠标和滚轮保持既有分支。

## 诊断与隐私

保留字段名 `external_keyboard_events_passed`，其含义收窄为实际命中 `TextInjection` 或 `Unknown` 键盘旁路的事件数，不再统计所有软件来源。Deskflow、RustDesk Map 等 `Mappable` 事件不因来源 PID 或状态而增加这一计数。它不是字符数，也不能确认目标应用已经收到文本，或证明 RustDesk 当前使用的键盘模式。

`Diagnostics::record_external_keyboard_passed()` 只递增专用计数；调用方分别记录 `events_seen` 和 `events_passed`，避免重复累计。诊断只保留计数与运行状态，不记录实际字符、Unicode 载荷、完整按键序列、来源 PID 或远程凭据。

## 自动回归清单

以下隔离自动回归已通过。原生对象测试不向系统发送键盘事件；Deskflow 和 RustDesk 的真实安装后验收另列下表。

| 用例 | 预期结果 | 状态 |
| --- | --- | --- |
| 本程序自身 marker，含自身 marker 为 100 | 自身回放优先，不误归为文本注入、不取消手势 | 通过 |
| 本地物理键盘 Down/Up 与左右修饰键 | 继续原有 Windows 映射及释放配对 | 通过 |
| Deskflow HID 形态的成对按键 | 继续映射，无残留按键绑定 | 通过 |
| Deskflow CG 回退形态：marker 0、正 PID、state 0 | 继续映射，不因 PID / state 进入旁路 | 通过 |
| 其他非 100 marker 的成对软件按键 | 继续映射，不设置全软件来源门槛 | 通过 |
| RustDesk Map / Translate `Chr` 形态的成对按键 | 继续映射，不因远程软件来源被隔离 | 通过 |
| marker 100、keycode 0、仅 Down、附不同文本 | 每个原生事件原样透传，无绑定残留或后续吞键 | 通过 |
| marker 100 的修饰键与热键事件 | 完整通道透传，不混入本项目快捷键和物理状态 | 通过 |
| 其他工具使用 marker 100 | 同样归入文本通道旁路，符合非唯一身份约定 | 通过 |
| 不可读取的原生键盘事件 | `Unknown` 安全透传 | 通过 |
| 文本旁路后接物理或 Deskflow 成对输入 | 后续映射、修饰键和手势状态正常 | 通过 |
| 运行时锁忙碌或中毒 | 旁路立即返回；忙碌时保留待取消手势请求 | 通过 |
| 已按住本地映射键时经过文本旁路 | 不清除原按键绑定，之后 Up 正常释放 | 通过 |
| 非键盘事件 | 沿用鼠标和滚轮既有分支 | 通过 |
| 旁路计数 | 只统计 `TextInjection` / `Unknown`；seen、passed 不重复累计 | 通过 |

本轮重新执行的验证结果：

| 命令 | 状态 |
| --- | --- |
| `cargo test --workspace` | 193 项通过，0 失败，3 项原有权限/独占测试忽略 |
| `cargo check --workspace` | 通过 |
| `cargo build -p mac-to-windows` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| `node --check ui/app.js`、`node --check ui/clipboard.js` | 通过 |
| `node --test ui/*.test.js` | 32 项通过 |
| `git diff --check` | 通过 |

## 人工验收表

修正后的真实远程、Deskflow 和前台输入验收均未执行。需记录双方系统及工具版本、实际 RustDesk 键盘模式、MacToWindows 状态和每项结果；不收集实际输入文本、PID、连接 ID 或凭据。用户确认旧安装版可用属于保留行为的基线，不代表新源码已经安装验收。

| 场景 | 操作与验收点 | 状态 |
| --- | --- | --- |
| Deskflow 1.27.0.0，Windows 模式启用 | 对照旧安装版，检查中英文、左右修饰键、Windows 快捷键与按下/释放；继续保持原有映射 | 待执行 |
| Deskflow HID / CG 回退路径 | 在能够确认所经路径的测试环境分别验收；不为强制回退更改用户权限或安装 | 待执行 |
| Deskflow，Windows 模式暂停 | 原生输入及释放正常 | 待执行 |
| 本地物理键盘，Windows 模式启用与暂停 | 检查英文、拼音组字、左右修饰键和快捷键，无既有功能退化 | 待执行 |
| RustDesk Legacy，Windows 模式启用 | 连续输入英文、中文、标点，确认首字后仍可输入；检查同属 Enigo 通道的热键原样处理 | 待执行 |
| RustDesk Translate，Windows 模式启用 | 分别检查文本序列与成对 `Chr`；前者透传，后者仍映射，记录实际模式 | 待执行 |
| RustDesk Map，Windows 模式启用 | 检查英文、远端输入法组字、Windows 快捷键及按下/释放，保持原有映射 | 待执行 |
| RustDesk 各模式，Windows 模式暂停 | 对照原生远程输入，区分本项目与 RustDesk 自身限制 | 待执行 |
| 物理、Deskflow、RustDesk 输入交替 | 检查无卡键、手势误触发、残留修饰键或后续映射失效 | 待执行 |
| 诊断计数与隐私 | marker 100 文本旁路计数增加；Deskflow 普通映射不计旁路；日志无字符、PID 或凭据 | 待执行 |

本轮不打包、不安装、不发布；保留当前已授权应用，不把源码与隔离测试结果描述为已部署修复。
