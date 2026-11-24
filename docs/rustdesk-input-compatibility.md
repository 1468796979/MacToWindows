# RustDesk 与软件键盘输入兼容性

本次修复为软件注入的键盘事件增加原样透传路径。常规物理键盘继续使用 Windows 键位映射；RustDesk 等软件产生的键盘事件不再进入物理按键状态机，也不由 MacToWindows 重建为新的按键事件。

本轮只修改源码并进行自动化验证，不生成安装或发布包，不安装、替换本机 App，也不执行签名、公证或发布。下文分别记录自动回归与人工验收结果。自动测试通过不能代替真实 RustDesk 远程验收。

## 原因与 RustDesk 模式边界

已核对本机 `/Applications/RustDesk.app` 的公开版本元数据：RustDesk 1.5.3，build 71。没有读取 RustDesk 个人配置、连接凭据或远程会话内容。用户使用的键盘模式尚未确认，不能假定所有远程输入都走同一条路径。

官方 1.5.3 标签对应提交 `95cca3f661f635b3cc85c04935fe8d96d8f432ef`。该版本的 macOS 文本注入会按 Unicode 字素创建虚拟键码为 0、只包含 Down 的事件，并在原生事件上附带文本；没有配对 Up。Enigo 发送前将 `EVENT_SOURCE_USER_DATA` 设为 100。实现见 [`libs/enigo/src/macos/macos_impl.rs` 的 `key_sequence()`、`post_event()`](https://github.com/rustdesk/rustdesk/blob/95cca3f661f635b3cc85c04935fe8d96d8f432ef/libs/enigo/src/macos/macos_impl.rs)。

原有物理按键状态机在第一次 Down 后登记键码绑定，将没有 Up 的同键码后续 Down 视为重复按下而抑制。因此这条文本输入路径会把不同字符当成同一个物理键。若进入按键重建路径，原始 Unicode 载荷也没有得到保留。这解释了软件文本输入为何会与真实物理键盘表现不同。

| RustDesk 1.5.3 接收路径 | macOS 注入方式 | 与本问题的关系 |
| --- | --- | --- |
| Legacy：不含热键修饰键的普通 `Chr` | `process_chr()` → Enigo `key_sequence()` | 使用上述文本事件 |
| Legacy：`Unicode`、`Seq` | `process_unicode()` / `process_seq()` → `key_sequence()` | 使用上述文本事件 |
| Legacy：热键字符及控制键 | Enigo 布局键或控制键的 Down/Up | 不应与普通文本事件混为一谈 |
| Translate：`Seq` | Enigo `key_sequence()` | 使用上述文本事件 |
| Translate：`Chr` | rdev 原始 macOS 虚拟键码 Down/Up | 与文本事件不同 |
| Map | rdev 原始 macOS 虚拟键码 Down/Up | 不能据文本路径直接认定 Map 也有相同故障 |

分派见固定提交的 [`src/server/input_service.rs`](https://github.com/rustdesk/rustdesk/blob/95cca3f661f635b3cc85c04935fe8d96d8f432ef/src/server/input_service.rs) 中 `legacy_keyboard_mode()`、`translate_keyboard_mode()`、`map_keyboard_mode()`。Translate 的 `Unicode` 分支在该版本未处理，不能把它描述成另一条 Enigo 文本入口。

RustDesk 的 [`Cargo.lock`](https://github.com/rustdesk/rustdesk/blob/95cca3f661f635b3cc85c04935fe8d96d8f432ef/Cargo.lock) 将 rdev 锁定为 `0904f23ce496e255461a98848b85c11fa5a62eab`。该版本 [`src/macos/simulate.rs` 的 `VirtualInput::simulate()`](https://github.com/rustdesk-org/rdev/blob/0904f23ce496e255461a98848b85c11fa5a62eab/src/macos/simulate.rs) 直接发送虚拟键事件，未显式设置 Enigo 的 marker 100。因此不能只用 `user_data == 100` 识别全部 RustDesk 按键，也不能仅根据某个计数证明当前远程键盘模式。

## 透传边界与原生元数据约定

键盘来源在进入修饰键跟踪、剪贴板热键及规则引擎之前判定：

- 本程序带自身 marker 的回放沿用已有回环防护，不重新执行映射。
- 符合常规物理来源约定的事件继续走原有映射、按下/释放配对与快捷键处理。
- 外部软件键盘事件直接返回原生事件，保留其 Unicode 载荷、flags、Down/Up 形式和其他原生属性；不更新本项目的物理按键绑定或修饰键状态，也不触发本项目的键位映射及热键。远程快捷键由 RustDesk 和 macOS 按原事件处理。

原生接口 `EventTapEvent::keyboard_route(own_marker)` 对 keyDown、keyUp、flagsChanged（事件类型 10、11、12）返回以下路由；其他事件返回 `None`，继续既有分支。顺序以自身 marker 的判断优先：

| 路由 | 原生元数据约定 |
| --- | --- |
| `OwnInjected` | 非零 user data 与本程序 marker 相同 |
| `Hardware` | user data 为 0、来源 PID 为 0，且来源状态为 1（HIDSystemState） |
| `External` | 其他键盘来源，包含无法按上述约定确认的来源 |

读取的 CoreGraphics 字段分别为 `kCGEventSourceUserData`（42）、`kCGEventSourceUnixProcessID`（41）与 `kCGEventSourceStateID`（45）。路由不读取实际字符，也不以虚拟键码 0 作为软件来源的识别条件。

这是对软件键盘来源的统一兼容策略，不是只给 RustDesk、marker 100 或中文字符设置例外。鼠标与滚轮不属于本次键盘透传策略的变更范围。

来源判定使用 CoreGraphics 提供的事件来源元数据，包括 user data、自带的来源进程信息及来源状态约定。它们是工程上的识别约定，不是经过认证的安全身份，不能用于信任判断或权限控制。软件可以生成或影响这些元数据；经虚拟 HID 驱动提交、表现与普通硬件一致的事件，不能仅靠这些字段可靠地区分。本次修复不承诺识别所有虚拟 HID 或所有远程输入实现。

外部键盘旁路不会等待运行时互斥锁。回调通过非阻塞取锁尝试取消本地待识别手势；运行时忙碌时以原子标记保留该请求，在下次本机事件进入引擎前处理。此过程不清除本机仍按住的映射键，自身回放也不会取消本机手势。

## 诊断与隐私

新增 `external_keyboard_events_passed` 计数，表示命中外部软件键盘透传分支的事件数量。键盘 Down、Up 或修饰键事件按实际到达事件计数，不代表字符数，也不确认目标应用已经接收文本。

`Diagnostics::record_external_keyboard_passed()` 只递增这一专用计数；调用方分别记录 `events_seen` 和 `events_passed`，避免重复累计。计数通过健康快照和运行状态查询暴露。

诊断只保留计数与运行状态，不记录实际字符、Unicode 载荷、完整按键序列、来源 PID 或远程凭据。来源判定需要读取原生元数据，不等于将它们写入日志或配置。

## 自动回归清单

以下自动回归已通过。原生测试只构造隔离事件对象，未向系统发送键盘事件。

| 用例 | 预期结果 | 状态 |
| --- | --- | --- |
| 本程序自身 marker 的回放 | 保持回环防护，不进入外部软件键盘计数 | 通过 |
| 常规物理来源的 Down/Up 与左右修饰键 | 保持原有分类与映射行为 | 通过 |
| 外部软件来源，包含非零 marker 或软件来源元数据 | 原始键盘事件透传，不更新物理状态 | 通过 |
| 外部软件来源未使用 Enigo marker 100 | 仍按统一来源策略透传，不依赖 RustDesk 特例 | 通过 |
| 连续 keycode 0、仅 Down、带不同文本的事件 | 每个原生事件均透传，无绑定残留或后续吞键 | 通过 |
| 带 Unicode 载荷的原生事件 | 不重建按键，载荷及原生属性保留 | 通过 |
| 外部软件修饰键、Ctrl+Shift、Shift、Option+V | 不进入本项目规则及热键，不污染后续物理输入 | 通过 |
| 软件输入后接常规物理键盘输入 | 物理按键、修饰键配对和映射继续正常 | 通过 |
| 鼠标、滚轮事件 | 沿用既有处理分支 | 通过 |
| 运行时锁忙碌或中毒 | 外部文字立即原样透传；忙碌时下次本机事件仍取消旧手势 | 通过 |
| 单独记录外部键盘计数 | 仅专用计数增加，seen/passed 等其他计数不变 | 通过 |
| 调用方同时记录 seen、passed 和专用计数 | 三个计数各增加一次，无重复累计 | 通过 |
| 工作区测试、静态检查及前端测试 | 全部通过；失败项如实记录 | 通过 |

验证命令与结果：

| 命令 | 结果 |
| --- | --- |
| `cargo test --workspace` | 182 项通过，0 失败，3 项原有权限/独占 Event Tap 测试忽略 |
| `cargo check --workspace` | 通过 |
| `cargo build -p mac-to-windows` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| `node --check ui/app.js`、`node --check ui/clipboard.js` | 通过 |
| `node --test ui/*.test.js` | 32 项通过 |
| `git diff --check` | 通过 |

## 人工验收表

本表全部待执行。验收需记录双方系统与 RustDesk 版本、实际键盘模式、是否启用 MacToWindows，以及每项通过或失败的结果；不收集用户输入的实际文本、来源 PID、连接 ID 或凭据。

| 场景 | 操作与验收点 | 状态 |
| --- | --- | --- |
| 本地物理键盘，Windows 模式启用 | 检查英文、拼音组字、左右修饰键及既有 Windows 快捷键 | 待执行 |
| 本地物理键盘，Windows 模式暂停 | 检查原生中英文输入与快捷键 | 待执行 |
| RustDesk Legacy，Windows 模式启用 | 连续输入英文、中文及标点，确认首字后仍可继续输入；检查删除、回车与快捷键 | 待执行 |
| RustDesk Translate，Windows 模式启用 | 分别检查文本序列和普通按键；记录实际经过的客户端模式，不据单次表现推断全部入口 | 待执行 |
| RustDesk Map，Windows 模式启用 | 检查英文及远端输入法组字，按下/释放和快捷键；独立记录结果 | 待执行 |
| RustDesk 各模式，Windows 模式暂停 | 对照原生远程输入，区分本项目行为与 RustDesk 自身限制 | 待执行 |
| 远程软件输入与本地物理输入交替 | 检查无卡键、残留修饰键或本地映射失效 | 待执行 |
| 软件来源快捷键 | 确认不被本项目重映射或触发本项目剪贴板热键；常规物理 Option+V 仍能打开历史 | 待执行 |
| 诊断计数与隐私 | 软件键盘透传计数增加，日志不包含实际输入字符、来源 PID 或凭据 | 待执行 |

本轮没有执行上述真实远程或前台输入验收，也没有安装修复后的 App。保留当前已授权应用，不把源码测试结果描述为已部署修复。
