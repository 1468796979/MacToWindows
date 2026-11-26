# MacToWindows

MacToWindows 是一个面向 Windows 用户的 macOS 全局键鼠语义层。工程按设计文档拆成纯 Rust 核心和 macOS 适配边界，核心可在 CI 环境独立测试。

## 当前工程

- crates/event-model：物理 keycode、左右修饰键、鼠标、滚轮与决策模型。
- crates/rule-engine：优先级/作用域匹配、组合键、按下释放冻结、共享目标引用计数、滚轮反转和故障安全。
- crates/config：版本化 TOML、默认 Windows profile、规则编译、原子写入和备份。
- crates/macos-adapter：权限/注入抽象、marker 回环防护、非 macOS 安全实现和 macOS CoreGraphics FFI 边界。
- crates/diagnostics：无原始文本内容的原子计数与健康快照。
- src-tauri：Tauri 2 应用、运行时生命周期、配置持久化和剪贴板服务。
- ui：读取真实后端配置的设置页和剪贴板面板，无需前端构建工具。

## 配置与规则

配置位于 `~/Library/Application Support/MacToWindows/config.toml`，上一份内容备份为 `config.toml.bak`。设置修改后自动保存。替换主文件前发生错误时，原配置继续生效。主文件已替换但目录同步失败时，新配置生效，设置页显示同步警告。每次启动保持暂停，需要手动启用 Windows 模式。

应用会锁定用户数据目录，防止并行实例互相覆盖配置和剪贴板历史。升级前请退出旧版应用。启动时无法恢复的配置会尽量保存在同目录下的 `config.manual-recovery`，恢复提示会显示保存位置。无法保全原文件时，应用会暂停配置写入。

当前配置格式为 schema v2，规则使用稳定 UUID；v1 文件在加载或导入时迁移，启动迁移会保留原文件备份。各预设保存完整、独立的规则集合，自定义预设初始复制默认规则，之后新增或关闭规则不会改变其他预设。旧版本不能直接读取 v2；回退前应保留迁移前的备份。

触发键示例：`key:11`、`chord:left_control|right_shift+key:11`。多个修饰键之间使用 `|`，修饰键和主键之间使用 `+`。释放时识别的 `modifier_chord:left_control|left_shift` 只支持完整的一次性 `shortcut:` 动作，不接受 `pass`、`suppress` 或没有完整释放生命周期的按键输出。当前桌面应用明确拒绝启用应用限定及设备限定规则。

启用 Windows 模式后，内置预设会把 Ctrl+S 转成 Command+S，适用于 VS Code 等应用。左 Ctrl、右 Ctrl 和两侧同时按住均可触发。保存规则使用 `exact_chord:`（如 `exact_chord:left_control+key:1`），只有修饰键完全匹配才会转换。Ctrl+Shift+S 等其他组合沿用原有行为。升级时会为采用内置快捷键且没有自定义覆盖的旧预设补齐此规则。首次写回时，`config.toml.bak` 保留升级前的配置。后续保存会轮换备份，计划回退到不支持 `exact_chord:` 的版本时请另存一份升级前的配置。

内置预设中的左 Ctrl+Shift+Z 和左 Ctrl+Shift+V 分别输出 Command+Shift+Z 和 Command+Shift+V。可用 `modified:left_command|left_shift+key:6` 配置带修饰键的输出。

启用 Windows 模式后，内置预设会把 Ctrl+Y 转成 Command+Shift+Z，在 VS Code 中重做上一步撤销的编辑。左 Ctrl、右 Ctrl 和两侧同时按住均可触发，额外按下 Shift、Alt 等修饰键时不会触发。旧版配置升级时，仍使用内置撤销规则的预设会自动补齐缺少的重做规则，原有的 Ctrl+Y 自定义规则会保留。

设置页显示规则优先级以及相同触发条件下的覆盖关系。可以在添加规则时指定优先级，也可以从规则列表修改已保存的优先级。

鼠标映射支持按钮、释放和拖动；滚轮反转保留原事件精度及惯性信息。`shortcut:show_desktop` 使用设置页中配置的系统输出快捷键，默认 `key:103`（F11），需与 macOS 的“显示桌面”绑定一致。锁屏和关闭窗口分别使用 Control+Command+Q、Command+W。

设置页的“滚轮方向”可选跟随系统、自然和非自然，在 Windows 模式启用时生效。逐轴滚轮规则优先。旧配置升级后默认跟随系统。使用 TOML 显式配置时，在 `[mouse]` 中同时设置 `scroll_mode_initialized = true` 和对应的 `scroll_mode` 值（`system`、`natural` 或 `non_natural`）。

剪贴板保存失败时，当前内存记录仍可查看，服务会自动重试保存。关闭面板或成功粘贴不会清除尚未恢复的保存错误。清空历史和删除条目会同步更新可自动恢复的 `.bak` 备份。若更新中断，应用会优先恢复已提交的 `.removal` 记录，再同步主文件和备份。主文件损坏或缺失时，应用会恢复带新版恢复标记的有效备份。缺少该标记的旧备份会保存在 `clipboard-history.manual-recovery` 中，面板会提示该位置。确认备份内容后，可退出应用并在保留当前文件的前提下，将需要恢复的备份复制为 `clipboard-history.json`。

## 软件输入透传

设置页提供“软件输入透传”开关，顶层配置项为 `software_input_passthrough`，默认 `true`；旧配置缺少该字段时也默认开启。修改后自动保存，切换预设保留该全局选择，修改此开关不会自动启用 Windows 模式。

Windows 模式启用时，物理键盘和可识别的 Deskflow 始终使用完整项目映射。对其他可识别软件输入，行为如下：

| 输入 | 透传开启（默认） | 透传关闭 |
| --- | --- | --- |
| 普通单键，例如自定义 B→N | 保留原始单键 | 应用项目映射 |
| 明确的 Windows 组合快捷键，例如 Ctrl+C、Alt+Tab | 保留项目转换 | 保留项目转换 |
| 剪贴板、Shift 输入法切换等项目功能 | 保留，需有可识别的按键事件 | 保留，需有可识别的按键事件 |
| 被识别为已生成文字的事件 | 保留原始文字 | 保留原始文字 |

Windows 模式暂停时不进行上述映射和快捷转换，Option+V 保留现有独立剪贴板功能。无法与物理键盘区分的软件或虚拟 HID 输入沿用原有映射，来源缓存只是兼容提示，不是软件身份认证。

部分软件事件无法区分最终文字与 A 按键，本轮优先保留内容，因此相应的 A / Ctrl+A 也可能保持原样。此限制同样可能影响 RustDesk 的 Legacy、Translate、Map / rdev 路径，不能承诺这些同形快捷键转换为 Command+A；物理键盘和已识别的 Deskflow 保持原映射。具体文字识别边界、自动回归与真实会话验收状态见[输入兼容性说明](docs/rustdesk-input-compatibility.md)。

## 验证

    cargo test --workspace
    cargo check --workspace
    cargo build -p mac-to-windows
    cargo fmt --all -- --check
    node --check ui/app.js
    node --check ui/clipboard.js
    node --test ui/*.test.js

已具备 macOS 输入权限时，可显式验证 Event Tap 创建、启动、停止和线程回收（不发送替换按键）：

    cargo test -p mac-to-windows --lib tests::event_runtime_starts_a_real_core_graphics_tap -- --ignored --exact

真实 macOS 权限矩阵、Event Tap 硬件回归、Universal Binary、签名和 notarization 需要在 macOS 13+ 真机阶段执行。

## 本地 macOS 构建与权限复用

辅助功能、输入监控和注入事件权限绑定在 Bundle ID、entitlements 和代码签名身份的组合上。当前应用标识固定为 `com.mactowindows.desktop`，本地验证时必须重复使用同一个 Apple Development、Developer ID 或固定的本地代码签名身份；`codesign --sign -` 是临时 adhoc 签名，每次构建都会产生新的 CDHash，macOS 会把它当作新的客户端并再次要求授权。

先用 Tauri 构建 arm64 App，再用固定身份签名：

```bash
cargo tauri build --target aarch64-apple-darwin --bundles app
CODESIGN_IDENTITY="你的固定签名身份" scripts/sign-local-macos-app.sh
```

脚本会拒绝在没有稳定身份时继续签名，并在签名后验证 Bundle ID、Team ID 和 CDHash。这样只要 Bundle ID、entitlements 和签名身份不变，后续重新构建通常可以直接验证功能，不需要重复授予 TCC 权限。发布包仍应使用 Apple Developer 证书完成正式签名和 notarization。
