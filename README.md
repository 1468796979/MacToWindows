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

## 本地 macOS 构建与权限复用

辅助功能、输入监控和注入事件权限绑定在 Bundle ID、entitlements 和代码签名身份的组合上。当前应用标识固定为 `com.mactowindows.desktop`，本地验证时必须重复使用同一个 Apple Development、Developer ID 或固定的本地代码签名身份；`codesign --sign -` 是临时 adhoc 签名，每次构建都会产生新的 CDHash，macOS 会把它当作新的客户端并再次要求授权。

先用 Tauri 构建 arm64 App，再用固定身份签名：

```bash
cargo tauri build --target aarch64-apple-darwin --bundles app
CODESIGN_IDENTITY="你的固定签名身份" scripts/sign-local-macos-app.sh
```

脚本会拒绝在没有稳定身份时继续签名，并在签名后验证 Bundle ID、Team ID 和 CDHash。这样只要 Bundle ID、entitlements 和签名身份不变，后续重新构建通常可以直接验证功能，不需要重复授予 TCC 权限。发布包仍应使用 Apple Developer 证书完成正式签名和 notarization。
