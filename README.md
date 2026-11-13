# MacToWindows

MacToWindows 是一个面向 Windows 用户的 macOS 全局键鼠语义层。工程按设计文档拆成纯 Rust 核心和 macOS 适配边界，核心可在 CI 环境独立测试。

## 当前工程

- crates/event-model：物理 keycode、左右修饰键、鼠标、滚轮与决策模型。
- crates/rule-engine：优先级/作用域匹配、组合键、按下释放冻结、共享目标引用计数、滚轮反转和故障安全。
- crates/config：版本化 TOML、默认 Windows profile、规则编译、原子写入和备份。
- crates/macos-adapter：权限/注入抽象、marker 回环防护、非 macOS 安全实现和 macOS CoreGraphics FFI 边界。
- crates/diagnostics：无原始文本内容的原子计数与健康快照。
- src-tauri：运行时命令/状态控制器和后续 Tauri 2 集成入口。
- ui：无需构建工具即可打开的设置页静态壳。

## 验证

    cargo test --workspace
    cargo check --workspace
    node --check ui/app.js

真实 macOS 权限矩阵、Event Tap 硬件回归、Universal Binary、签名和 notarization 需要在 macOS 13+ 真机阶段执行。
