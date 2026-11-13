const core = window.clipboardCore;
const state = { entries: [], selectedIndex: -1, error: "" };
const $ = (id) => document.getElementById(id);
const backendInvoke = (command, args) => {
  const invoke = window.__TAURI__?.core?.invoke || window.__TAURI_INTERNALS__?.invoke;
  return invoke ? invoke(command, args) : Promise.reject(new Error("Tauri bridge unavailable"));
};
const listen = window.__TAURI__?.event?.listen || window.__TAURI_INTERNALS__?.listen;

function formatTime(timestamp) {
  if (!timestamp) return "刚刚";
  return new Date(timestamp).toLocaleString([], { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

function applySnapshot(snapshot) {
  const normalized = core.normalizeSnapshot(snapshot);
  state.entries = normalized.entries;
  state.error = normalized.lastError || "";
  if (state.entries.length === 0) state.selectedIndex = -1;
  else state.selectedIndex = Math.max(0, Math.min(state.selectedIndex, state.entries.length - 1));
  render();
}

async function refreshSnapshot() {
  applySnapshot(await backendInvoke("clipboard_history"));
}

function render() {
  const list = $("clipboard-list");
  list.replaceChildren();
  $("clipboard-empty").hidden = state.entries.length > 0;
  $("clipboard-error").textContent = state.error;
  state.entries.forEach((entry, index) => {
    const item = document.createElement("article");
    item.className = "clipboard-item" + (index === state.selectedIndex ? " selected" : "");
    item.setAttribute("role", "option");
    item.setAttribute("aria-selected", String(index === state.selectedIndex));
    item.addEventListener("click", () => {
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
    content.append(text, time);

    const remove = document.createElement("button");
    remove.className = "clipboard-delete";
    remove.type = "button";
    remove.textContent = "删除";
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
  try {
    await backendInvoke("select_clipboard_item", { id });
    await refreshSnapshot();
  } catch (error) {
    state.error = error?.message || String(error);
    render();
  }
}

async function closePanel() {
  try { await backendInvoke("close_clipboard_panel"); }
  catch (error) { state.error = error?.message || String(error); render(); }
}

$("clipboard-close").addEventListener("click", closePanel);
$("clipboard-clear").addEventListener("click", async () => {
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
  if (event.target === document.body) closePanel();
});

if (listen) {
  Promise.resolve(listen("clipboard-show", (event) => {
    state.selectedIndex = core.resetSelection();
    applySnapshot(event.payload);
    window.focus();
  }))
    .catch((error) => { state.error = error?.message || String(error); render(); });
  Promise.resolve(listen("clipboard-updated", (event) => applySnapshot(event.payload)))
    .catch((error) => { state.error = error?.message || String(error); render(); });
}
backendInvoke("clipboard_history").then(applySnapshot).catch((error) => { state.error = error?.message || String(error); render(); });
render();
