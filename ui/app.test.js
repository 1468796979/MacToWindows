const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const settle = () => new Promise(setImmediate);
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };

function snapshot(revision = 0, changes = {}) {
  return {
    config_revision: revision, active_profile: "custom", profiles: ["custom", "windows_physical", "Imported"],
    show_desktop_shortcut: "key:103", config_error: null,
    rules: [{ id: "rule-id", name: "Imported rule", category: "keyboard_rules", trigger: "key:11", action: "key:12", enabled: true, priority: 100, app_bundle_id: null, editable: true }],
    runtime: { status: "Disabled", permission: "Granted", listen_access: true, post_access: true, tap_state: "Running", events_seen: 1, injection_failures: 0, input_source: "ABC", clipboard_status: "正常", last_error: null, pending_releases: 0 },
    ...changes,
  };
}

async function appUi({ initial = snapshot(), invoke, connected = true } = {}) {
  class Element {
    constructor(tagName = "div") {
      this.tagName = tagName; this.children = []; this.listeners = new Map(); this.dataset = {}; this.attributes = {};
      this.value = ""; this.textContent = ""; this.disabled = false; this.hidden = true; this.className = ""; this.files = [];
      this.classList = { toggle() {} };
    }
    set innerHTML(_) { throw new Error("Unsafe innerHTML rendering"); }
    setAttribute(name, value) { this.attributes[name] = value; }
    append(...children) { this.children.push(...children); }
    replaceChildren(...children) { this.children = children; }
    addEventListener(type, listener) { if (!this.listeners.has(type)) this.listeners.set(type, []); this.listeners.get(type).push(listener); }
    async fire(type, extra = {}) {
      const event = { target: this, preventDefault() { this.defaultPrevented = true; }, ...extra };
      await Promise.all((this.listeners.get(type) || []).map((handler) => handler(event)));
      return event;
    }
    click() { return this.fire("click"); }
    reset() {}
    focus() {}
  }
  const nodes = new Map(); const windowListeners = new Map(); const backendListeners = new Map();
  const calls = []; const blobs = []; const downloads = [];
  const document = {
    getElementById(id) { if (!nodes.has(id)) nodes.set(id, new Element()); return nodes.get(id); },
    createElement(tag) {
      const element = new Element(tag);
      if (tag === "a") element.click = () => downloads.push(element);
      return element;
    },
    querySelector() { return this.getElementById("dialog-backdrop"); },
  };
  const window = { ruleEditorCore: require("./rule-editor-core.js"), addEventListener(type, handler) {
    if (!windowListeners.has(type)) windowListeners.set(type, []); windowListeners.get(type).push(handler);
  } };
  if (connected) window.__TAURI__ = {
    core: { invoke(command, args) {
      calls.push({ command, args });
      if (invoke) return Promise.resolve().then(() => invoke(command, args, calls));
      if (command === "settings_snapshot") return Promise.resolve(initial);
      return Promise.reject(new Error("Unexpected command " + command));
    } },
    event: { listen(name, handler) { backendListeners.set(name, handler); return Promise.resolve(() => {}); } },
  };
  const context = vm.createContext({ window, document,
    Blob: class { constructor(parts) { blobs.push(parts); } }, URL: { createObjectURL() { return "blob:result"; }, revokeObjectURL() {} },
  });
  vm.runInContext(fs.readFileSync(require.resolve("./app.js"), "utf8"), context);
  await settle();
  return {
    nodes, calls, blobs, downloads, get: (id) => document.getElementById(id),
    current() { return JSON.parse(vm.runInContext("JSON.stringify(state.snapshot)", context)); },
    async event(type, properties = {}) {
      const event = { preventDefault() {}, ...properties };
      await Promise.all((windowListeners.get(type) || []).map((handler) => handler(event)));
      await settle();
    },
    async runtimeChanged() { await backendListeners.get("runtime-changed")?.({}); await settle(); },
    async idle() { await vm.runInContext("writeQueue", context); await settle(); },
  };
}

const toggleFor = (ui, index = 0) => ui.get("rules").children[index].children[2];

test("the actual settings snapshot supplies profiles, rules and safe text", async () => {
  const malicious = '<img src=x onerror="bad()">';
  const data = snapshot(4);
  data.rules[0].name = malicious;
  data.rules.push({ ...data.rules[0], id: "readonly", name: "Native", editable: false });
  const ui = await appUi({ initial: data });
  assert.equal(ui.get("profile-select").value, "custom");
  assert.equal(ui.get("profile-select").children.length, 3);
  assert.equal(ui.get("rules").children.length, 2);
  assert.equal(ui.get("rules").children[0].children[0].children[0].textContent, malicious);
  assert.equal(toggleFor(ui, 1).textContent, "只读");
  assert.equal(ui.get("show-desktop-shortcut").value, "key:103");
});

test("rule toggle updates the backend using its stable ID and revision", async () => {
  const before = snapshot(4);
  const after = snapshot(5); after.rules[0].enabled = false;
  const ui = await appUi({ invoke(command, args) {
    if (command === "settings_snapshot") return before;
    assert.equal(command, "set_rule_enabled");
    assert.deepEqual(JSON.parse(JSON.stringify(args)), { profileId: "custom", ruleId: "rule-id", enabled: false, expectedRevision: 4 });
    return after;
  } });
  await toggleFor(ui).click();
  await ui.idle();
  assert.equal(ui.current().rules[0].enabled, false);
  assert.equal(toggleFor(ui).attributes["aria-pressed"], "false");
});

test("failed rule writes keep the confirmed state and show their error", async () => {
  const ui = await appUi({ invoke(command) {
    if (command === "settings_snapshot") return snapshot();
    throw new Error("disk full");
  } });
  await toggleFor(ui).click();
  await ui.idle();
  assert.equal(ui.current().rules[0].enabled, true);
  assert.equal(toggleFor(ui).attributes["aria-pressed"], "true");
  assert.match(ui.get("settings-feedback").textContent, /disk full/);
});

test("configuration writes are serialized and use the latest confirmed revision", async () => {
  const first = deferred(); let mutationCount = 0;
  const ui = await appUi({ invoke(command, args) {
    if (command === "settings_snapshot") return snapshot();
    mutationCount += 1;
    if (command === "set_rule_enabled") return first.promise;
    assert.equal(command, "set_show_desktop_shortcut");
    assert.equal(args.expectedRevision, 1);
    assert.equal(args.trigger, "chord:left_command+key:103");
    return snapshot(2, { show_desktop_shortcut: args.trigger });
  } });
  const toggle = toggleFor(ui).click();
  await settle();
  ui.get("show-desktop-shortcut").value = "chord:left_command+key:103";
  await ui.get("show-desktop-shortcut").fire("input");
  const save = ui.get("show-desktop-save").click();
  await settle();
  assert.equal(mutationCount, 1);
  first.resolve(snapshot(1));
  await Promise.all([toggle, save]);
  await ui.idle();
  assert.equal(mutationCount, 2);
  assert.equal(ui.current().config_revision, 2);
});

test("successful enable refreshes Degraded status and rejects an older delayed response", async () => {
  const old = deferred(); let reads = 0;
  const disabled = snapshot(); disabled.runtime.status = "Degraded";
  const running = snapshot(); running.runtime.status = "Running";
  const ui = await appUi({ invoke(command) {
    if (command === "toggle_mode") return true;
    if (++reads === 1) return disabled;
    if (reads === 2) return old.promise;
    return running;
  } });
  const focus = ui.event("focus");
  await settle();
  await ui.get("toggle-mode").click();
  await ui.idle();
  assert.equal(ui.get("status-title").textContent, "Windows 模式已启用");
  old.resolve(disabled);
  await focus;
  assert.equal(ui.get("status-title").textContent, "Windows 模式已启用");
});

test("profile changes, imported rules and new rule names come from mutation snapshots", async () => {
  let current = snapshot();
  const ui = await appUi({ invoke(command, args) {
    if (command === "settings_snapshot") return current;
    if (command === "set_profile") current = snapshot(1, { active_profile: args.profile, rules: [] });
    else if (command === "import_config") current = snapshot(2, { active_profile: "FromFile", profiles: ["FromFile"], rules: [] });
    else if (command === "add_rule") {
      assert.equal(args.name, "My named rule");
      assert.equal(args.expectedRevision, 2);
      current = snapshot(3, { active_profile: "FromFile", profiles: ["FromFile"], rules: [{ ...snapshot().rules[0], name: args.name }] });
    } else throw new Error(command);
    return current;
  } });
  ui.get("profile-select").value = "Imported";
  await ui.get("profile-select").fire("change"); await ui.idle();
  assert.equal(ui.get("rules").children[0].textContent, "没有匹配的规则。");
  ui.get("config-file").files = [{ text: async () => "valid toml" }];
  await ui.get("config-file").fire("change"); await ui.idle();
  assert.equal(ui.get("profile-select").value, "FromFile");
  ui.get("rule-name").value = "My named rule";
  ui.get("rule-trigger").value = "key:11";
  ui.get("rule-action").value = "key:12";
  await ui.get("rule-form").fire("submit"); await ui.idle();
  assert.equal(ui.get("rules").children[0].children[0].children[0].textContent, "My named rule");
});

test("tray runtime events refresh the settings without fabricating diagnostic values", async () => {
  let current = snapshot();
  const ui = await appUi({ invoke: () => current });
  current = snapshot(); current.runtime.status = "Running"; current.runtime.events_seen = 99;
  await ui.runtimeChanged();
  assert.equal(ui.get("status-title").textContent, "Windows 模式已启用");
  assert.equal(ui.get("events-seen").textContent, 99);
});

test("failed exports never download a fabricated configuration", async () => {
  const ui = await appUi({ invoke(command) {
    if (command === "settings_snapshot") return snapshot();
    throw new Error("export unavailable");
  } });
  await ui.get("export-config").click();
  assert.equal(ui.blobs.length, 0);
  assert.equal(ui.downloads.length, 0);
  assert.match(ui.get("settings-feedback").textContent, /导出失败/);
});

test("recording through actual key handlers preserves sides and resets on blur", async () => {
  const ui = await appUi();
  await ui.get("record-trigger").click();
  await ui.event("keydown", { key: "Control", code: "ControlRight", ctrlKey: true });
  await ui.event("keydown", { key: "Shift", code: "ShiftRight", ctrlKey: true, shiftKey: true });
  await ui.event("keydown", { key: "B", code: "KeyB", ctrlKey: true, shiftKey: true });
  assert.equal(ui.get("rule-trigger").value, "chord:right_control|right_shift+key:11");
  await ui.event("blur");
  await ui.get("record-trigger").click();
  await ui.event("keydown", { key: "C", code: "KeyC", ctrlKey: false });
  assert.equal(ui.get("rule-trigger").value, "key:8");
  await ui.get("record-trigger").click();
  await ui.event("keydown", { key: "Control", code: "ControlRight", ctrlKey: true });
  await ui.event("keydown", { key: "Shift", code: "ShiftLeft", ctrlKey: true, shiftKey: true });
  await ui.event("keyup", { key: "Shift", code: "ShiftLeft", ctrlKey: true });
  await ui.event("keyup", { key: "Control", code: "ControlRight", ctrlKey: false });
  assert.equal(ui.get("rule-trigger").value, "modifier_chord:right_control|left_shift");
});

test("an unavailable backend leaves controls paused without fake data", async () => {
  const ui = await appUi({ connected: false });
  assert.equal(ui.current(), null);
  assert.equal(ui.get("toggle-mode").disabled, true);
  await ui.get("toggle-mode").click();
  await ui.idle();
  assert.equal(ui.get("status-title").textContent, "Windows 模式未启用");
  assert.equal(ui.get("rules").children.length, 1);
  assert.match(ui.get("settings-feedback").textContent, /后端未连接/);
});

test("a stale configuration read cannot undo a confirmed profile mutation", async () => {
  const old = deferred(); let reads = 0;
  const ui = await appUi({ invoke(command, args) {
    if (command === "set_profile") return snapshot(1, { active_profile: args.profile, rules: [] });
    if (++reads === 1) return snapshot();
    return old.promise;
  } });
  const focus = ui.event("focus");
  await settle();
  ui.get("profile-select").value = "Imported";
  await ui.get("profile-select").fire("change"); await ui.idle();
  old.resolve(snapshot());
  await focus;
  assert.equal(ui.get("profile-select").value, "Imported");
  assert.equal(ui.current().config_revision, 1);
});

test("an older failed refresh does not replace the feedback for a newer success", async () => {
  const old = deferred(); let reads = 0;
  const running = snapshot(); running.runtime.status = "Running";
  const ui = await appUi({ invoke(command) {
    if (command === "toggle_mode") return true;
    if (++reads === 1) return snapshot();
    if (reads === 2) return old.promise;
    return running;
  } });
  const focus = ui.event("focus");
  await settle();
  await ui.get("toggle-mode").click(); await ui.idle();
  old.reject(new Error("obsolete refresh failure"));
  await focus;
  assert.equal(ui.get("status-title").textContent, "Windows 模式已启用");
  assert.doesNotMatch(ui.get("settings-feedback").textContent, /obsolete/);
});

test("emergency disable bypasses an unfinished configuration write", async () => {
  const write = deferred();
  const ui = await appUi({ invoke(command) {
    if (command === "settings_snapshot") return snapshot();
    if (command === "set_rule_enabled") return write.promise;
    if (command === "emergency_disable") return undefined;
    throw new Error(command);
  } });
  const pendingWrite = toggleFor(ui).click();
  await settle();
  assert.equal(ui.get("profile-select").disabled, true);
  assert.equal(ui.get("panic-test").disabled, false);
  await ui.get("panic-test").click();
  assert(ui.calls.some((call) => call.command === "emergency_disable"));
  assert.equal(ui.current().config_revision, 0);
  write.resolve(snapshot(1));
  await pendingWrite; await ui.idle();
});
