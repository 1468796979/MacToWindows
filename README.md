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

配置位于 `~/Library/Application Support/MacToWindows/config.toml`，上一次有效内容备份为 `config.toml.bak`。规则、名称、开关和预设修改成功后自动保存；保存失败时原配置继续生效。每次启动保持暂停，需要手动启用 Windows 模式。

当前配置格式为 schema v2，规则使用稳定 UUID；v1 文件在加载或导入时迁移，启动迁移会保留原文件备份。各预设保存完整、独立的规则集合，自定义预设初始复制默认规则，之后新增或关闭规则不会改变其他预设。旧版本不能直接读取 v2；回退前应保留迁移前的备份。

触发键示例：`key:11`、`chord:left_control|right_shift+key:11`。多个修饰键之间使用 `|`，修饰键和主键之间使用 `+`。释放时识别的 `modifier_chord:left_control|left_shift` 只支持完整的一次性 `shortcut:` 动作，不接受 `pass`、`suppress` 或没有完整释放生命周期的按键输出。当前桌面应用明确拒绝启用应用限定及设备限定规则。

鼠标映射支持按钮、释放和拖动；滚轮反转保留原事件精度及惯性信息。`shortcut:show_desktop` 使用设置页中配置的系统输出快捷键，默认 `key:103`（F11），需与 macOS 的“显示桌面”绑定一致。锁屏和关闭窗口分别使用 Control+Command+Q、Command+W。

剪贴板保存失败时，当前内存记录仍可查看，服务会自动重试保存。关闭面板或成功粘贴不会清除尚未恢复的保存错误。“清空历史”沿用现有备份策略，不删除 `.bak` 文件。

RustDesk 等软件键盘输入原样透传，常规物理键盘继续映射；来源识别边界、诊断计数及待执行验收见[软件键盘输入兼容性说明](docs/rustdesk-input-compatibility.md)。

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
