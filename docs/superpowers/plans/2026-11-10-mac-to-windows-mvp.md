# MacToWindows MVP Implementation Plan

> For agentic workers: REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax for tracking.

Goal: Build a testable Rust workspace implementing the MacToWindows MVP core, configuration, safe macOS adapter boundary, diagnostics, and a static settings UI shell.

Architecture: Keep platform-independent event and rule logic in focused crates. Use immutable compiled rules plus an event-owned state machine, and isolate macOS CoreGraphics behind a trait with a safe non-macOS implementation. Expose runtime command/status types from src-tauri and provide a dependency-free UI shell that can be bundled later by Tauri 2.

Tech Stack: Rust 2021, serde, toml, thiserror, tempfile (tests), HTML/CSS/JavaScript static UI, conditional CoreGraphics FFI boundary.

Spec: docs/superpowers/specs/2026-11-10-mac-to-windows-design.md

## Global Constraints

- macOS 13+; Apple Silicon and Intel targets are represented by conditional adapter code.
- Unknown or failed event handling always passes the original event through.
- Rule output is limited to 16 events and one mapping depth.
- Press/release mapping is frozen at press time and release_all clears logical state.
- Config schema contains schema_version, active_profile, keyboard_rules, mouse_rules, global_hotkeys, app_rules, device_rules.
- Hot path does not perform blocking I/O or persist raw text/input content.

## Review Focus

- Unknown keycodes, event kinds, and malformed output remain pass-through or validation errors.
- A hot rule replacement between key down/up still releases the original logical key.
- Shared logical modifiers and mouse buttons do not release early.
- Config corruption or failed atomic replacement preserves the last valid config.
- Injection failure returns the original event and marks runtime degraded.

### Task 1: Workspace and event model

Files: Cargo.toml; crates/event-model/Cargo.toml; crates/event-model/src/lib.rs; tests in lib.rs.

Produces NormalizedEvent, EventKind, EventPhase, PhysicalCode, ModifierSet, OutputEvent, Decision, EventOrigin, and default physical key helpers.

- [ ] Write tests for key identity, modifiers, mouse and decision serialization.
- [ ] Run cargo test -p event-model and verify it fails because types are absent.
- [ ] Implement the event model and Windows physical modifier preset.
- [ ] Run cargo test -p event-model and verify it passes.
- [ ] Run cargo fmt --check.

### Task 2: Rule engine and state machine

Files: crates/rule-engine/Cargo.toml; crates/rule-engine/src/lib.rs; tests in lib.rs.

Produces Rule, Trigger, Action, RuleScope, CompiledRules, RuleEngine, EngineError, RuntimeState::release_all, and handle.

- [ ] Write failing tests for single key replacement, pass-through, priority, hot-update press/release freeze, shared targets, scroll inversion, conflict and loop rejection.
- [ ] Run cargo test -p rule-engine and verify expected failures.
- [ ] Implement rule compilation, matcher, state machine and safe fallback.
- [ ] Run cargo test -p rule-engine and verify all pass.
- [ ] Run cargo fmt --check.

### Task 3: Config schema and atomic storage

Files: crates/config/Cargo.toml; crates/config/src/lib.rs; tests in lib.rs.

Produces AppConfig, Profile, ConfigError, load, save_atomic, export_toml, import_toml, and default_config.

- [ ] Write failing tests for default config, TOML round-trip, schema validation and backup-preserving atomic save.
- [ ] Run cargo test -p mac-to-windows-config and verify failures.
- [ ] Implement serde schema, validation, import/export and atomic replacement with backup.
- [ ] Run cargo test -p mac-to-windows-config and verify pass.
- [ ] Run cargo fmt --check.

### Task 4: Diagnostics and macOS adapter boundary

Files: crates/diagnostics/Cargo.toml; crates/diagnostics/src/lib.rs; crates/macos-adapter/Cargo.toml; crates/macos-adapter/src/lib.rs; tests in both libs.

Produces Diagnostics, HealthSnapshot, PermissionState, TapState, EventAdapter, SafeAdapter, InjectionError, and marker helpers.

- [ ] Write failing tests for atomic counters, permission gating, marker loop filtering and injection failure pass-through.
- [ ] Run package tests and verify failures.
- [ ] Implement diagnostics and conditional adapter traits/stub.
- [ ] Run package tests and verify pass.
- [ ] Run cargo fmt --check.

### Task 5: Runtime shell, UI, and end-to-end verification

Files: src-tauri/Cargo.toml; src-tauri/src/main.rs; src-tauri/tauri.conf.json; ui/index.html; ui/app.js; ui/styles.css; workspace Cargo.toml.

Produces RuntimeCommand, RuntimeStatus, RuntimeController, and a CLI smoke path checkable on any platform.

- [ ] Write failing tests for enable/disable/release/shutdown command transitions.
- [ ] Run cargo test -p mac-to-windows and verify failures.
- [ ] Implement runtime controller and static UI shell with status, preset, import/export and diagnostics actions.
- [ ] Run cargo test --workspace and cargo check --workspace and verify pass.
- [ ] Validate UI files and run final formatting.
