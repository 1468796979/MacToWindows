# MacToWindows MVP 实现设计

## 意图

将现有需求与设计基线落成一个从 Rust 核心到 macOS 适配、配置和菜单栏壳的可运行工程。核心逻辑必须可以在非 macOS 环境独立编译和测试，平台特有能力通过接口隔离，并在缺少权限或注入失败时安全透传原始事件。

## 目标

- 建立 Cargo workspace，包含 event-model、rule-engine、config、macos-adapter、diagnostics 和 src-tauri。
- 实现基于物理 keycode、左右修饰键和鼠标物理按钮的事件模型。
- 实现 Pass/Suppress/Emit 决策、规则优先级、scope、冲突与循环校验。
- 实现按下时冻结映射、按键/鼠标按钮状态和 release_all。
- 实现 Windows 物理键位默认预设、滚轮反转和未匹配透传。
- 实现 TOML 配置校验、导入导出、原子保存和备份回滚。
- 提供 macOS Event Tap 的条件编译接口、权限状态和 marker 回环防护；非 macOS 提供安全 stub。
- 提供诊断计数器、运行时命令/状态模型和可运行的 UI 静态壳。

## 非目标

- 在当前环境模拟真实 CoreGraphics 权限、Event Tap 和硬件兼容性。
- 首版实现触控板手势、复杂宏、按应用/设备规则运行时匹配或文本字符重排。
- 引入需要系统 WebKit 或签名环境才能运行的完整 Tauri 构建链。

## 约束

- Rust 2021 edition；macOS 13+ 适配使用 CoreGraphics/Core Foundation FFI。
- 热路径不能执行阻塞 I/O；规则快照不可变，状态由事件线程独占。
- 输出事件最多 16 个，映射深度为 1；注入失败必须回放原始事件。
- 配置包含 schema_version、active_profile、keyboard_rules、mouse_rules、global_hotkeys、app_rules、device_rules。
- 配置保存采用临时文件 + fsync + 原子 rename，并保留备份。
- 默认不记录字符或完整按键序列。

## 方案与取舍

设计文档已经确定采用 CoreGraphics CGEventTap + Rust FFI，并用 Tauri 2 作为 UI 容器。实现中保留 macOS adapter 的真实接口和条件编译边界，同时让核心 crates 在 Linux/CI 可测试；rdev/enigo 仅适合原型，外部虚拟 HID 会增加安装和系统扩展依赖，因此不纳入首版。

## 实现边界

- event-model：平台无关的数据类型、修饰键和默认物理映射。
- rule-engine：规则编译、匹配、组合键、滚轮、状态机和故障安全。
- config：TOML schema、默认配置、校验、迁移和原子存储。
- diagnostics：无原始输入内容的原子计数和健康快照。
- macos-adapter：权限抽象、事件 Tap 生命周期、注入接口和非 macOS 安全实现。
- src-tauri：组合核心模块、运行时命令/状态、CLI 级 smoke 入口。
- ui：无需构建工具即可打开的设置页静态壳，覆盖状态、规则和诊断入口。

## 验收

以 cargo test --workspace、cargo check --workspace、UI 静态文件检查和规则/配置/状态测试为证据；真实 macOS 权限、Event Tap、Universal Binary 和 8 小时稳定性留给 macOS 真机阶段。
