const core = window.clipboardCore;
const state = { entries: [], selectedId: null, error: "", selecting: null, panelOpen: false, panelSession: 0, revision: 0 };
let refreshInFlight = null;
const $ = (id) => document.getElementById(id);
const backendInvoke = (command, args) => {
  const invoke = window.__TAURI__?.core?.invoke || window.__TAURI_INTERNALS__?.invoke;
  return invoke ? invoke(command, args) : Promise.reject(new Error("Tauri bridge unavailable"));
};
const listen = window.__TAURI__?.event?.listen || window.__TAURI_INTERNALS__?.listen;
const currentWindow = window.__TAURI__?.window?.getCurrentWindow?.();
const dragRegion = $("clipboard-drag-region");
if (dragRegion) {
  dragRegion.addEventListener("mousedown", (event) => {
    if (event.button !== 0) return;
    const target = event.target instanceof Element ? event.target : null;
    if (target?.closest("button, a, input, select, textarea, [role=button]")) return;
    event.preventDefault();
    const dragPromise = currentWindow?.startDragging
      ? currentWindow.startDragging()
      : Promise.reject(new Error("Tauri window API unavailable"));
    dragPromise.catch((error) => {
      state.error = error?.message || String(error);
      render();
    });
  });
}

function formatTime(timestamp) {
  if (!timestamp) return "刚刚";
  return new Date(timestamp).toLocaleString([], { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

function applySnapshot(snapshot) {
  const normalized = core.normalizeSnapshot(snapshot);
  const { panelSession, revision } = normalized;
  if (revision < state.revision || panelSession < state.panelSession) return;
  if (panelSession !== state.panelSession) {
    state.selecting = null;
    state.selectedId = null;
  }
  state.revision = revision;
  state.panelSession = panelSession;
  state.entries = normalized.entries;
  state.error = normalized.lastError || "";
  state.panelOpen = normalized.panelOpen;
  if (!state.panelOpen) state.selectedId = null;
  if (!state.entries.some((entry) => entry.id === state.selectedId)) state.selectedId = null;
  render();
}

async function refreshSnapshot() {
  if (!refreshInFlight) {
    refreshInFlight = backendInvoke("clipboard_history")
      .then(applySnapshot)
      .finally(() => { refreshInFlight = null; });
  }
  return refreshInFlight;
}

function render() {
  const list = $("clipboard-list");
  list.replaceChildren();
  $("clipboard-close").disabled = false;
  $("clipboard-clear").disabled = Boolean(state.selecting);
  $("clipboard-empty").hidden = state.entries.length > 0;
  $("clipboard-error").textContent = state.error;
  state.entries.forEach((entry) => {
    const item = document.createElement("article");
    const selected = entry.id === state.selectedId;
    item.className = "clipboard-item" + (selected ? " selected" : "");
    item.setAttribute("role", "option");
    item.setAttribute("aria-selected", String(selected));
    item.addEventListener("click", () => {
      if (state.selecting) return;
      state.selectedId = entry.id;
      selectEntry(entry.id);
    });

    const content = document.createElement("div");
    const text = document.createElement("div");
    text.className = "clipboard-text";
    text.textContent = core.previewText(entry.text);
    const time = document.createElement("div");
    time.className = "clipboard-time";
    time.textContent = formatTime(entry.copied_at_unix_ms);
    content.append(time, text);

    const remove = document.createElement("button");
    remove.className = "clipboard-delete";
    remove.type = "button";
    remove.textContent = "删除";
    remove.disabled = Boolean(state.selecting);
    remove.addEventListener("click", async (event) => {
      event.stopPropagation();
      try {
        await backendInvoke("delete_clipboard_item", { id: entry.id });
        await refreshSnapshot();
      }
      catch (error) { state.error = error?.message || String(error); render(); }
    });
    item.append(content, remove);
    list.append(item);
  });
}

async function selectEntry(id) {
  if (state.selecting || !state.panelOpen) return;
  const request = { session: state.panelSession };
  state.selecting = request;
  render();
  try {
    await backendInvoke("select_clipboard_item", { id, session: request.session });
    if (state.selecting !== request) return;
    state.selecting = null;
    state.panelOpen = false;
    state.selectedId = null;
    render();
  } catch (error) {
    if (state.selecting !== request) return;
    state.selecting = null;
    state.error = error?.message || String(error);
    render();
  }
}

async function closePanel() {
  const session = state.panelSession;
  state.selecting = null;
  state.panelOpen = false;
  state.selectedId = null;
  render();
  try {
    await backendInvoke("close_clipboard_panel", { session });
  }
  catch (error) {
    if (state.panelSession !== session) return;
    state.panelOpen = true;
    state.error = error?.message || String(error);
    render();
  }
}

$("clipboard-close").addEventListener("click", closePanel);
$("clipboard-clear").addEventListener("click", async () => {
  if (state.selecting) return;
  try {
    await backendInvoke("clear_clipboard_history");
    await refreshSnapshot();
  }
  catch (error) { state.error = error?.message || String(error); render(); }
});
document.addEventListener("keydown", (event) => {
  if (event.defaultPrevented || event.isComposing) return;
  if (event.key === "Escape") { event.preventDefault(); closePanel(); return; }
  const target = event.target instanceof Element ? event.target : null;
  if (target?.closest("button, a, input, select, textarea, [role=button], [contenteditable]:not([contenteditable=false])")) return;
  if ((event.key === "ArrowDown" || event.key === "ArrowUp") && state.entries.length) {
    event.preventDefault();
    const currentIndex = state.entries.findIndex((entry) => entry.id === state.selectedId);
    const nextIndex = core.moveSelection(currentIndex, event.key === "ArrowDown" ? 1 : -1, state.entries.length);
    state.selectedId = state.entries[nextIndex].id;
    render();
  }
  if (event.key === "Enter" && state.selectedId !== null) {
    event.preventDefault(); selectEntry(state.selectedId);
  }
});
document.addEventListener("click", (event) => {
  if (event.target === document.body || event.target === $("clipboard-panel")) closePanel();
});

if (listen) {
  Promise.resolve(listen("clipboard-show", (event) => {
    applySnapshot(event.payload);
    $("clipboard-list").scrollTop = 0;
    window.focus();
    refreshSnapshot().catch(() => {});
  }))
    .catch((error) => { state.error = error?.message || String(error); render(); });
  Promise.resolve(listen("clipboard-updated", (event) => {
    applySnapshot(event.payload);
  }))
    .catch((error) => { state.error = error?.message || String(error); render(); });
}
backendInvoke("clipboard_history").then(applySnapshot).catch((error) => { state.error = error?.message || String(error); render(); });
render();
const refreshVisiblePanel = () => {
  refreshSnapshot().catch(() => {});
};
window.addEventListener("focus", refreshVisiblePanel);
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) refreshVisiblePanel();
});
