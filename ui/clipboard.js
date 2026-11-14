const core = window.clipboardCore;
const state = { entries: [], selectedIndex: -1, error: "", selecting: false, panelOpen: false };
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
  state.entries = normalized.entries;
  state.error = normalized.lastError || "";
  const staleClosedSnapshot = state.panelOpen && !normalized.panelOpen;
  if (!staleClosedSnapshot) state.panelOpen = normalized.panelOpen;
  if (!normalized.panelOpen && !staleClosedSnapshot) {
    state.selecting = false;
    state.selectedIndex = -1;
  } else if (normalized.lastError) {
    state.selecting = false;
  }
  if (state.entries.length === 0) state.selectedIndex = -1;
  else if (state.selectedIndex >= 0) {
    state.selectedIndex = Math.min(state.selectedIndex, state.entries.length - 1);
  }
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
  $("clipboard-clear").disabled = state.selecting;
  $("clipboard-empty").hidden = state.entries.length > 0;
  $("clipboard-error").textContent = state.error;
  state.entries.forEach((entry, index) => {
    const item = document.createElement("article");
    item.className = "clipboard-item" + (index === state.selectedIndex ? " selected" : "");
    item.setAttribute("role", "option");
    item.setAttribute("aria-selected", String(index === state.selectedIndex));
    item.addEventListener("click", () => {
      if (state.selecting) return;
      state.selectedIndex = index;
      render();
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
    remove.disabled = state.selecting;
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
  if (state.selecting) return;
  state.selecting = true;
  render();
  try {
    await backendInvoke("select_clipboard_item", { id });
    state.selecting = false;
    state.panelOpen = false;
    state.selectedIndex = core.resetSelection();
    render();
  } catch (error) {
    state.selecting = false;
    state.error = error?.message || String(error);
    render();
  }
}

async function closePanel() {
  state.selecting = false;
  state.panelOpen = false;
  state.selectedIndex = core.resetSelection();
  render();
  try {
    await backendInvoke("close_clipboard_panel");
  }
  catch (error) { state.panelOpen = true; state.error = error?.message || String(error); render(); }
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
  if (event.key === "Escape") { event.preventDefault(); closePanel(); return; }
  if (event.key === "ArrowDown" && state.entries.length) {
    event.preventDefault(); state.selectedIndex = core.moveSelection(state.selectedIndex, 1, state.entries.length); render();
  }
  if (event.key === "ArrowUp" && state.entries.length) {
    event.preventDefault(); state.selectedIndex = core.moveSelection(state.selectedIndex, -1, state.entries.length); render();
  }
  if (event.key === "Enter" && state.selectedIndex >= 0) {
    event.preventDefault(); selectEntry(state.entries[state.selectedIndex].id);
  }
});
document.addEventListener("click", (event) => {
  if (event.target === document.body || event.target === $("clipboard-panel")) closePanel();
});

if (listen) {
  Promise.resolve(listen("clipboard-show", (event) => {
    state.selecting = false;
    state.panelOpen = true;
    state.selectedIndex = core.resetSelection();
    applySnapshot(event.payload);
    $("clipboard-list").scrollTop = 0;
    window.focus();
    refreshSnapshot().catch(() => {});
  }))
    .catch((error) => { state.error = error?.message || String(error); render(); });
  Promise.resolve(listen("clipboard-updated", (event) => {
    const snapshot = core.normalizeSnapshot(event.payload);
    if (!snapshot.panelOpen || snapshot.lastError) state.selecting = false;
    applySnapshot(event.payload);
  }))
    .catch((error) => { state.error = error?.message || String(error); render(); });
}
backendInvoke("clipboard_history").then(applySnapshot).catch((error) => { state.error = error?.message || String(error); render(); });
render();
const refreshVisiblePanel = () => {
  state.panelOpen = true;
  refreshSnapshot().catch(() => {});
};
window.addEventListener("focus", refreshVisiblePanel);
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) refreshVisiblePanel();
});
