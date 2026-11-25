const state = {
  snapshot: null, feedback: "", permissionMessage: "", writesPending: 0,
  desktopDraftDirty: false, lastAppliedRequest: 0,
};
const $ = (id) => document.getElementById(id);
const ruleEditor = window.ruleEditorCore;
const recorder = ruleEditor.createRecorder();
const permissionSettingsPath = "请前往：系统设置 → 隐私与安全性 → 辅助功能和输入监控。";
const backendInvoke = (command, args) => {
  const invoke = window.__TAURI__?.core?.invoke || window.__TAURI_INTERNALS__?.invoke;
  return invoke ? invoke(command, args) : Promise.reject(new Error("后端未连接"));
};
let requestSequence = 0;
let writeQueue = Promise.resolve();
const message = (error) => error?.message || String(error);
const textElement = (tag, className, text) => {
  const element = document.createElement(tag);
  element.className = className;
  element.textContent = text;
  return element;
};

function renderRules() {
  const query = $("search").value.trim().toLowerCase();
  const list = $("rules");
  list.replaceChildren();
  const rules = (state.snapshot?.rules || []).filter((rule) =>
    [rule.name, rule.trigger, rule.action, rule.category, rule.app_bundle_id].join(" ").toLowerCase().includes(query));
  for (const rule of rules) {
    const profileId = state.snapshot.active_profile;
    const row = textElement("div", "rule", "");
    const description = document.createElement("div");
    description.append(
      textElement("div", "rule-name", rule.name || `${rule.trigger} → ${rule.action}`),
      textElement("div", "rule-detail", `${rule.trigger} → ${rule.action}${rule.app_bundle_id ? ` · ${rule.app_bundle_id}` : ""}`),
    );
    const categoryLabel = { keyboard_rules: "键盘", mouse_rules: "鼠标", global_hotkeys: "快捷键", app_rules: "按应用", device_rules: "按设备" }[rule.category] || "规则";
    row.append(description, textElement("span", "keycap", categoryLabel));
    if (rule.editable) {
      const toggle = document.createElement("button");
      toggle.type = "button";
      toggle.className = "switch" + (rule.enabled ? " on" : "");
      toggle.disabled = state.writesPending > 0;
      toggle.setAttribute("aria-label", `切换规则：${rule.name || rule.trigger}`);
      toggle.setAttribute("aria-pressed", String(rule.enabled));
      toggle.dataset.ruleId = rule.id;
      toggle.addEventListener("click", () => mutateSettings("set_rule_enabled", () => ({
        profileId, ruleId: rule.id, enabled: !rule.enabled,
      })).catch(() => {}));
      row.append(toggle);
    } else {
      row.append(textElement("span", "muted", "只读"));
    }
    list.append(row);
  }
  if (!rules.length) list.append(textElement("p", "muted", state.snapshot ? "没有匹配的规则。" : "正在读取配置…"));
}

function renderStatus() {
  const runtime = state.snapshot?.runtime;
  const enabled = runtime?.status === "Running";
  const degraded = runtime?.status === "Degraded";
  $("toggle-mode").classList.toggle("is-on", enabled);
  $("toggle-mode").setAttribute("aria-pressed", String(enabled));
  $("toggle-label").textContent = enabled ? "已启用" : "已暂停";
  $("health-pill").textContent = degraded ? "降级透传" : (enabled ? "运行中" : "已暂停");
  $("health-pill").className = "pill " + (degraded || enabled ? "accent-pill" : "muted-pill");
  $("status-title").textContent = degraded ? "Windows 模式已降级" : (enabled ? "Windows 模式已启用" : "Windows 模式未启用");
  $("status-copy").textContent = runtime?.permission === "Denied"
    ? "输入权限未就绪。" + permissionSettingsPath
    : degraded ? "输入源或事件注入异常，已暂停映射；请处理错误后重新启用。"
      : enabled ? "全局键鼠事件正在按当前规则转换。" : "原生键鼠输入正常透传。启用后会按当前预设处理全局事件。";
  $("permission-feedback").textContent = state.permissionMessage;
  const errors = [state.feedback, state.snapshot?.config_error, runtime?.last_error,
    runtime?.pending_releases ? `仍有 ${runtime.pending_releases} 个逻辑按键等待安全释放。` : ""].filter(Boolean);
  $("settings-feedback").textContent = errors.join("\n");
  $("events-seen").textContent = runtime?.events_seen ?? 0;
  $("injection-failures").textContent = runtime?.injection_failures ?? 0;
  $("latency").textContent = runtime?.tap_state || "未连接";
  $("tap-state").textContent = runtime?.tap_state || "未连接";
  $("input-source").textContent = runtime?.input_source || "未知";
  $("clipboard-status").textContent = "剪贴板服务：" + (runtime?.clipboard_status || "未知");
}

function renderSettings() {
  const busy = state.writesPending > 0 || !state.snapshot;
  for (const id of ["toggle-mode", "profile-select", "add-rule", "import-config", "export-config", "permission-button", "show-desktop-save", "rule-submit", "software-input-passthrough"]) {
    $(id).disabled = busy;
  }
  const select = $("profile-select");
  select.replaceChildren();
  const labels = { windows_physical: "Windows 物理键位", shortcut: "快捷键习惯", custom: "自定义" };
  for (const profile of state.snapshot?.profiles || []) {
    const option = document.createElement("option");
    option.value = profile;
    option.textContent = labels[profile] || profile;
    select.append(option);
  }
  select.value = state.snapshot?.active_profile || "";
  if (!state.desktopDraftDirty) $("show-desktop-shortcut").value = state.snapshot?.show_desktop_shortcut || "";
  const passthrough = state.snapshot?.software_input_passthrough !== false;
  $("software-input-passthrough").className = "switch" + (passthrough ? " on" : "");
  $("software-input-passthrough").setAttribute("aria-checked", String(passthrough));
  $("software-input-passthrough-state").textContent = !state.snapshot ? "正在读取配置…"
    : passthrough ? "已开启 · 软件普通输入透传，保留快捷键"
      : "已关闭 · 软件普通按键使用项目映射，文字仍受保护";
  renderRules();
  renderStatus();
}

function applySnapshot(snapshot, request) {
  if (!snapshot || !Array.isArray(snapshot.rules) || !Array.isArray(snapshot.profiles)) throw new Error("后端返回的配置无效");
  const oldRevision = state.snapshot?.config_revision ?? -1;
  if (snapshot.config_revision < oldRevision || (snapshot.config_revision === oldRevision && request < state.lastAppliedRequest)) return false;
  state.snapshot = snapshot;
  state.lastAppliedRequest = Math.max(state.lastAppliedRequest, request);
  renderSettings();
  return true;
}

async function syncSettings() {
  const request = ++requestSequence;
  try {
    const snapshot = await backendInvoke("settings_snapshot");
    applySnapshot(snapshot, request);
    return snapshot;
  } catch (error) {
    if (request < state.lastAppliedRequest) return state.snapshot;
    throw error;
  }
}

function enqueueWrite(operation) {
  state.writesPending += 1;
  renderSettings();
  const result = writeQueue.then(async () => {
    state.feedback = "";
    try {
      if (!state.snapshot) await syncSettings();
      return await operation();
    } catch (error) {
      state.feedback = message(error);
      await syncSettings().catch(() => {});
      throw error;
    } finally {
      state.writesPending -= 1;
      renderSettings();
    }
  });
  writeQueue = result.catch(() => {});
  return result;
}

function mutateSettings(command, argumentsForSnapshot) {
  return enqueueWrite(async () => {
    const request = ++requestSequence;
    const snapshot = await backendInvoke(command, {
      ...argumentsForSnapshot(), expectedRevision: state.snapshot.config_revision,
    });
    applySnapshot(snapshot, request);
    return snapshot;
  });
}

function runtimeMutation(command) {
  return enqueueWrite(async () => {
    await backendInvoke(command);
    await syncSettings();
  });
}

$("search").addEventListener("input", renderRules);
$("toggle-mode").addEventListener("click", () => runtimeMutation("toggle_mode").catch(() => {}));
$("profile-select").addEventListener("change", (event) => {
  const profile = event.target.value;
  mutateSettings("set_profile", () => ({ profile })).catch(() => {});
});
$("software-input-passthrough").addEventListener("click", () => {
  if (!state.snapshot || state.writesPending > 0) return;
  mutateSettings("set_software_input_passthrough", () => ({
    enabled: state.snapshot.software_input_passthrough === false,
  })).catch(() => {});
});
$("permission-button").addEventListener("click", async () => {
  try {
    await runtimeMutation("reload_permissions");
    const view = state.snapshot.runtime;
    state.permissionMessage = view.permission === "Denied" || !view.listen_access || !view.post_access
      ? "权限检查未通过。" + permissionSettingsPath : "权限与运行状态已刷新。";
  } catch (error) { state.permissionMessage = "权限检查失败：" + message(error); }
  renderStatus();
});
$("open-settings-button").addEventListener("click", async () => {
  try {
    await backendInvoke("open_privacy_settings");
    state.permissionMessage = "已打开隐私与安全页面，授权后请再次检查权限。";
  } catch (error) { state.permissionMessage = "无法打开系统设置：" + message(error); }
  renderStatus();
});
// Emergency stop must remain independent of slow configuration writes.
$("panic-test").addEventListener("click", async () => {
  try {
    await backendInvoke("emergency_disable");
    await syncSettings();
  } catch (error) {
    state.feedback = "紧急停用失败：" + message(error);
    await syncSettings().catch(() => {});
    renderStatus();
  }
});
$("refresh-diagnostics").addEventListener("click", () => syncSettings().catch(showSyncError));
$("show-desktop-shortcut").addEventListener("input", () => { state.desktopDraftDirty = true; });
$("show-desktop-save").addEventListener("click", async () => {
  const trigger = $("show-desktop-shortcut").value.trim();
  try {
    await mutateSettings("set_show_desktop_shortcut", () => ({ trigger }));
    state.desktopDraftDirty = false;
    renderSettings();
  } catch (_) {}
});

let recordingRuleTrigger = false;
function stopRecording(label = "") {
  recordingRuleTrigger = false;
  recorder.cancel();
  $("recording-state").textContent = label;
}
function closeRuleDialog() {
  stopRecording();
  $("rule-dialog").hidden = true;
}
function openRuleDialog() {
  $("rule-form").reset();
  $("rule-feedback").textContent = "";
  stopRecording();
  $("rule-dialog").hidden = false;
  $("rule-name").focus();
}
$("add-rule").addEventListener("click", openRuleDialog);
$("rule-dialog-close").addEventListener("click", closeRuleDialog);
$("rule-cancel").addEventListener("click", closeRuleDialog);
document.querySelector("[data-close-rule-dialog]").addEventListener("click", closeRuleDialog);
$("record-trigger").addEventListener("click", () => {
  recorder.start();
  recordingRuleTrigger = true;
  $("recording-state").textContent = "请按下组合键；纯修饰键在全部松开后完成（Esc 取消）。";
  $("rule-trigger").focus();
});
function acceptRecordedTrigger(trigger) {
  if (!trigger) return;
  $("rule-trigger").value = trigger;
  stopRecording("已录制");
}
window.addEventListener("keydown", (event) => {
  if (event.isComposing) return;
  if (recordingRuleTrigger && event.key === "Escape") {
    event.preventDefault();
    stopRecording("已取消录制");
    return;
  }
  if (recordingRuleTrigger) event.preventDefault();
  try { acceptRecordedTrigger(recorder.keyDown(event)); }
  catch (error) { stopRecording(message(error)); }
  if (!recordingRuleTrigger && event.key === "Escape" && !$("rule-dialog").hidden) closeRuleDialog();
});
window.addEventListener("keyup", (event) => {
  if (event.isComposing) return;
  if (recordingRuleTrigger) event.preventDefault();
  try { acceptRecordedTrigger(recorder.keyUp(event)); }
  catch (error) { stopRecording(message(error)); }
});
window.addEventListener("blur", () => {
  recorder.reset();
  if (recordingRuleTrigger) stopRecording("窗口失去焦点，已取消录制");
});
$("rule-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (state.writesPending) return;
  $("rule-feedback").textContent = "";
  try {
    const rule = ruleEditor.normalizeRule({ name: $("rule-name").value, trigger: $("rule-trigger").value, action: $("rule-action").value, enabled: true });
    await mutateSettings("add_rule", () => ({ ...rule, priority: 100 }));
    closeRuleDialog();
  } catch (error) { $("rule-feedback").textContent = message(error); }
});
$("export-config").addEventListener("click", async () => {
  try {
    await writeQueue;
    const contents = await backendInvoke("export_config");
    const blob = new Blob([contents], { type: "application/toml" });
    const link = document.createElement("a");
    const url = URL.createObjectURL(blob);
    link.href = url;
    link.download = "mactowindows-config.toml";
    link.click();
    URL.revokeObjectURL(url);
  } catch (error) { state.feedback = "导出失败：" + message(error); renderStatus(); }
});
$("import-config").addEventListener("click", () => $("config-file").click());
$("config-file").addEventListener("change", async (event) => {
  const file = event.target.files[0];
  if (!file) return;
  try {
    const contents = await file.text();
    await mutateSettings("import_config", () => ({ contents }));
  } catch (error) { state.feedback = "配置导入失败：" + message(error); renderStatus(); }
  finally { event.target.value = ""; }
});

function showSyncError(error) {
  state.feedback = "刷新失败：" + message(error);
  renderStatus();
}
const listen = window.__TAURI__?.event?.listen || window.__TAURI_INTERNALS__?.listen;
if (listen) Promise.resolve(listen("runtime-changed", () => syncSettings().catch(showSyncError))).catch(showSyncError);
window.addEventListener("focus", () => syncSettings().catch(showSyncError));
renderSettings();
syncSettings().catch(showSyncError);
