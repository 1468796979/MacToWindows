const state = {
  enabled: false,
  runtimeStatus: "Stopped",
  rules: [
    { name: "左 Option → Windows", detail: "修饰键 · 物理位置", key: "⌥", enabled: true },
    { name: "左 Command → Alt", detail: "修饰键 · 物理位置", key: "⌘", enabled: true },
    { name: "Ctrl + A / C / V / X / Z", detail: "常用编辑快捷键", key: "⌃ A", enabled: true },
    { name: "Ctrl + F5", detail: "网页强制刷新", key: "⌃ F5", enabled: true },
    { name: "Ctrl + Shift", detail: "切换输入法", key: "⌃ ⇧", enabled: true },
    { name: "Caps Lock", detail: "大小写锁定（原生）", key: "⇪", enabled: true },
    { name: "Shift（Apple Pinyin）", detail: "中英文输入切换", key: "⇧", enabled: true },
    { name: "自然滚动", detail: "垂直滚轮方向", key: "↕", enabled: false }
  ],
  diagnostics: { seen: 0, failures: 0, tap: "未启动", inputSource: "未知", latency: "—" }
  , permission: "Unknown", listenAccess: false, postAccess: false, permissionMessage: ""
};
const $ = (id) => document.getElementById(id);
const backendInvoke = (command, args) => {
  const invoke = window.__TAURI__?.core?.invoke || window.__TAURI_INTERNALS__?.invoke;
  return invoke ? invoke(command, args) : Promise.reject(new Error("Tauri bridge unavailable"));
};
function renderRules() {
  const query = $("search").value.trim().toLowerCase();
  const visible = state.rules.filter((rule) => (rule.name + " " + rule.detail).toLowerCase().includes(query));
  $("rules").innerHTML = visible.map((rule) => {
    const index = state.rules.indexOf(rule);
    return '<div class="rule"><div><div class="rule-name">' + rule.name + '</div><div class="rule-detail">' + rule.detail + '</div></div><span class="keycap">' + rule.key + '</span><button class="switch ' + (rule.enabled ? "on" : "") + '" data-index="' + index + '" aria-label="切换规则"></button></div>';
  }).join("") || '<p class="muted">没有匹配的规则。</p>';
  document.querySelectorAll(".switch").forEach((button) => button.addEventListener("click", () => {
    state.rules[Number(button.dataset.index)].enabled = !state.rules[Number(button.dataset.index)].enabled;
    renderRules();
  }));
}
function renderStatus() {
  const degraded = state.runtimeStatus === "Degraded";
  $("toggle-mode").classList.toggle("is-on", state.enabled);
  $("toggle-mode").setAttribute("aria-pressed", String(state.enabled));
  $("toggle-label").textContent = state.enabled ? "已启用" : "已暂停";
  $("health-pill").textContent = degraded ? "降级透传" : (state.enabled ? "运行中" : "已暂停");
  $("health-pill").className = "pill " + (degraded || state.enabled ? "accent-pill" : "muted-pill");
  $("status-title").textContent = degraded
    ? "Windows 模式已降级"
    : (state.enabled ? "Windows 模式已启用" : "Windows 模式未启用");
  const statusCopy = state.permission === "Denied"
    ? "权限未就绪：监听输入 " + (state.listenAccess ? "已授权" : "未授权") + "，注入事件 " + (state.postAccess ? "已授权" : "未授权") + "。请在系统设置中重新添加本应用后再次检查。"
    : degraded
      ? "输入源或事件注入异常，已进入降级模式，原始键鼠输入继续透传。"
    : state.enabled
      ? "全局键鼠事件正在按当前规则转换。"
      : "原生键鼠输入正常透传。启用后会按当前预设处理全局事件。";
  $("status-copy").textContent = statusCopy;
  $("permission-feedback").textContent = state.permissionMessage;
  $("tap-state").textContent = state.diagnostics.latency;
  $("input-source").textContent = state.diagnostics.inputSource;
}
function renderDiagnostics() {
  $("events-seen").textContent = state.diagnostics.seen;
  $("injection-failures").textContent = state.diagnostics.failures;
  $("latency").textContent = state.diagnostics.latency;
}
$("toggle-mode").addEventListener("click", async () => {
  state.permissionMessage = "";
  try { state.enabled = await backendInvoke("toggle_mode"); }
  catch (error) {
    if (window.__TAURI__ || window.__TAURI_INTERNALS__) {
      $("status-copy").textContent = "启用失败：" + (error?.message || error);
      await syncRuntime().catch(() => {});
    }
    else state.enabled = !state.enabled;
  }
  renderStatus();
});
$("search").addEventListener("input", renderRules);
$("profile-select").addEventListener("change", async (event) => {
  try { await backendInvoke("set_profile", { profile: event.target.value }); }
  catch (_) { if (event.target.value === "custom") alert("自定义配置将在保存后启用。"); }
});
$("permission-button").addEventListener("click", async () => {
  try {
    await backendInvoke("reload_permissions");
    await syncRuntime();
    state.permissionMessage = "权限与运行状态已刷新。";
    renderStatus();
  }
  catch (error) {
    state.permissionMessage = "权限检查失败：" + (error?.message || error) + "。请检查辅助功能和输入监控权限。";
    renderStatus();
  }
});
$("open-settings-button").addEventListener("click", async () => {
  try {
    await backendInvoke("open_privacy_settings");
    state.permissionMessage = "已打开系统设置的隐私与安全页面，请完成授权后再次点击检查权限。";
  } catch (error) {
    state.permissionMessage = "无法打开系统设置：" + (error?.message || error);
  }
  renderStatus();
});
$("panic-test").addEventListener("click", async () => {
  try { await backendInvoke("emergency_disable"); } catch (_) {}
  state.enabled = false; renderStatus(); alert("映射已停止，逻辑按键已释放。");
});
$("refresh-diagnostics").addEventListener("click", async () => {
  try { await syncRuntime(); } catch (_) { state.diagnostics.seen += 1; state.diagnostics.latency = "本地预览"; renderDiagnostics(); }
});
$("add-rule").addEventListener("click", () => alert("录制按键后即可创建一条自定义规则。"));
$("export-config").addEventListener("click", async () => {
  let contents;
  try { contents = await backendInvoke("export_config"); }
  catch (_) { contents = "schema_version = 1\nactive_profile = \"windows\"\n"; }
  const blob = new Blob([contents], { type: "application/toml" });
  const link = document.createElement("a"); link.href = URL.createObjectURL(blob); link.download = "mactowindows-config.toml"; link.click(); URL.revokeObjectURL(link.href);
});
$("import-config").addEventListener("click", () => $("config-file").click());
$("config-file").addEventListener("change", async (event) => {
  const file = event.target.files[0];
  if (!file) return;
  try { await backendInvoke("import_config", { contents: await file.text() }); await syncRuntime(); alert("配置已导入并编译。"); }
  catch (error) { alert("配置导入失败：" + error); }
});
async function syncRuntime() {
  const view = await backendInvoke("runtime_status");
  state.runtimeStatus = view.status || "Stopped";
  state.enabled = view.status === "Running";
  state.diagnostics.seen = view.events_seen;
  state.diagnostics.failures = view.injection_failures;
  state.diagnostics.latency = view.tap_state;
  state.diagnostics.inputSource = view.input_source || "未知";
  state.permission = view.permission || "Unknown";
  state.listenAccess = Boolean(view.listen_access);
  state.postAccess = Boolean(view.post_access);
  renderStatus(); renderDiagnostics();
}
renderRules();
renderStatus();
renderDiagnostics();
syncRuntime().catch((error) => {
  $("status-copy").textContent = "后端未连接：" + (error?.message || error);
});
