const test = require("node:test");
const assert = require("node:assert/strict");
const { normalizeSnapshot, previewText, moveSelection } = require("./clipboard-core.js");

test("normalizeSnapshot supplies safe defaults for an empty history", () => {
  assert.deepEqual(normalizeSnapshot(), { entries: [], panelOpen: false, panelSession: 0, revision: 0, lastError: null, operationError: null });
});

test("previewText collapses whitespace and clips long content", () => {
  assert.equal(previewText("  alpha\n beta  ", 20), "alpha beta");
  assert.equal(previewText("1234567890", 5), "12345…");
});

test("moveSelection wraps around the history", () => {
  assert.equal(moveSelection(0, -1, 3), 2);
  assert.equal(moveSelection(2, 1, 3), 0);
  assert.equal(moveSelection(-1, -1, 3), 2);
  assert.equal(moveSelection(-1, 1, 3), 0);
  assert.equal(moveSelection(-1, 1, 0), -1);
});

// Exercise the shipped event handlers, including focus and the browser's
// unprevented Enter activation, instead of testing a separate shortcut helper.
const fs = require("node:fs");
const vm = require("node:vm");

async function clipboardUi({ selectPromise } = {}) {
  class Element {
    constructor(tagName = "div", parent = null) {
      this.tagName = tagName;
      this.parentElement = parent;
      this.children = [];
      this.listeners = new Map();
      this.disabled = false;
      this.contentEditable = "false";
    }
    addEventListener(type, listener) { this.listeners.set(type, listener); }
    replaceChildren() { this.children = []; }
    append(...children) {
      children.forEach((child) => { child.parentElement = this; });
      this.children.push(...children);
    }
    setAttribute() {}
    closest(selector) {
      if (["button", "a", "input", "select", "textarea"].some((tag) => this.tagName === tag && selector.includes(tag))) return this;
      if (this.contentEditable !== "false" && selector.includes("[contenteditable]")) return this;
      return this.parentElement?.closest(selector) || null;
    }
    async click() {
      await this.listeners.get("click")?.({ target: this, stopPropagation() {} });
    }
  }
  const nodes = new Map();
  const listeners = new Map();
  const document = {
    body: new Element("body"),
    hidden: false,
    getElementById(id) {
      if (!nodes.has(id)) nodes.set(id, new Element(["clipboard-close", "clipboard-clear"].includes(id) ? "button" : "div"));
      return nodes.get(id);
    },
    createElement: (tag) => new Element(tag),
    addEventListener: (type, listener) => listeners.set(type, listener),
  };
  const calls = [];
  const snapshot = { entries: [{ id: 7, text: "alpha", copied_at_unix_ms: 1 }], panel_open: true, last_error: null };
  const window = {
    clipboardCore: require("./clipboard-core.js"),
    __TAURI__: { core: { invoke: async (command, args) => {
      calls.push({ command, args });
      if (command === "select_clipboard_item" && selectPromise) return typeof selectPromise === "function" ? selectPromise(args) : selectPromise;
      return command === "clipboard_history" ? snapshot : undefined;
    } } },
    addEventListener() {},
  };
  const context = vm.createContext({ window, document, Element });
  vm.runInContext(fs.readFileSync(require.resolve("./clipboard.js"), "utf8"), context);
  await Promise.resolve();
  await Promise.resolve();
  calls.length = 0;
  return {
    calls, document, nodes, Element,
    snapshot(payload) {
      context.snapshotPayload = payload;
      vm.runInContext("applySnapshot(snapshotPayload)", context);
    },
    selecting() { return vm.runInContext("Boolean(state.selecting)", context); },
    async key(key, target = document.body, options = {}) {
      const event = { key, target, defaultPrevented: false, isComposing: false, ...options,
        preventDefault() { this.defaultPrevented = true; } };
      listeners.get("keydown")(event);
      if (key === "Enter" && !event.defaultPrevented && target.tagName === "button") await target.click();
      await Promise.resolve();
      await Promise.resolve();
      return event;
    },
  };
}

for (const [targetName, command] of [["clipboard-clear", "clear_clipboard_history"], ["clipboard-close", "close_clipboard_panel"], ["delete", "delete_clipboard_item"]]) {
  test(`Enter activates the focused ${targetName} button instead of pasting`, async () => {
    const ui = await clipboardUi();
    await ui.key("ArrowDown");
    const target = targetName === "delete"
      ? ui.nodes.get("clipboard-list").children[0].children[1]
      : ui.document.getElementById(targetName);
    const event = await ui.key("Enter", target);
    assert.equal(event.defaultPrevented, false);
    assert(ui.calls.some((call) => call.command === command));
    assert(!ui.calls.some((call) => call.command === "select_clipboard_item"));
  });
}

test("list navigation and Enter still select exactly one history item", async () => {
  const ui = await clipboardUi();
  assert.equal((await ui.key("ArrowDown")).defaultPrevented, true);
  assert.equal((await ui.key("Enter")).defaultPrevented, true);
  assert.equal(ui.calls.filter((call) => call.command === "select_clipboard_item").length, 1);
  assert.equal(ui.calls.find((call) => call.command === "select_clipboard_item").args.id, 7);
});

test("background reordering keeps the selected history entry", async () => {
  const ui = await clipboardUi();
  await ui.key("ArrowDown");
  ui.snapshot({ entries: [{ id: 8, text: "beta" }, { id: 7, text: "alpha" }], panel_open: true });
  assert.equal((await ui.key("Enter")).defaultPrevented, true);
  assert.equal(ui.calls.find(({ command }) => command === "select_clipboard_item").args.id, 7);
});

test("removing the selected entry leaves Enter idle until the user selects another", async () => {
  const ui = await clipboardUi();
  await ui.key("ArrowDown");
  ui.snapshot({ entries: [{ id: 8, text: "beta" }], panel_open: true });
  assert.equal((await ui.key("Enter")).defaultPrevented, false);
  assert(!ui.calls.some(({ command }) => command === "select_clipboard_item"));
  await ui.key("ArrowDown");
  await ui.key("Enter");
  assert.equal(ui.calls.find(({ command }) => command === "select_clipboard_item").args.id, 8);
});

test("interactive descendants and inputs retain their navigation keys", async () => {
  const ui = await clipboardUi();
  const buttonChild = new ui.Element("span", ui.document.getElementById("clipboard-clear"));
  for (const target of [buttonChild, new ui.Element("input"), new ui.Element("textarea")]) {
    assert.equal((await ui.key("ArrowDown", target)).defaultPrevented, false);
    assert.equal((await ui.key("Enter", target)).defaultPrevented, false);
  }
  const editable = new ui.Element();
  editable.contentEditable = "plaintext-only";
  assert.equal((await ui.key("ArrowUp", editable)).defaultPrevented, false);
  assert(!ui.calls.some((call) => call.command === "select_clipboard_item"));
});

test("composing and already handled keyboard events are ignored", async () => {
  const ui = await clipboardUi();
  await ui.key("ArrowDown");
  await ui.key("Enter", ui.document.body, { isComposing: true });
  await ui.key("Enter", ui.document.body, { defaultPrevented: true });
  await ui.key("Escape", ui.document.body, { isComposing: true });
  assert(!ui.calls.some((call) => ["select_clipboard_item", "close_clipboard_panel"].includes(call.command)));
});

test("Enter without a selection is ignored and Escape still dismisses the panel", async () => {
  const ui = await clipboardUi();
  assert.equal((await ui.key("Enter")).defaultPrevented, false);
  assert(!ui.calls.some((call) => call.command === "select_clipboard_item"));
  assert.equal((await ui.key("Escape", ui.document.getElementById("clipboard-clear"))).defaultPrevented, true);
  assert(ui.calls.some((call) => call.command === "close_clipboard_panel"));
});

test("storage failure snapshots do not unlock an in-flight paste", async () => {
  let finish;
  const selectPromise = new Promise((resolve) => { finish = resolve; });
  const ui = await clipboardUi({ selectPromise });
  await ui.key("ArrowDown");
  await ui.key("Enter");
  assert.equal(ui.selecting(), true);
  ui.snapshot({
    entries: [{ id: 7, text: "alpha", copied_at_unix_ms: 1 }],
    panel_open: true, last_error: "disk unavailable", storage_error: "disk unavailable", operation_error: "older selection failed",
  });
  assert.equal(ui.selecting(), true);
  await ui.key("Enter");
  assert.equal(ui.calls.filter((call) => call.command === "select_clipboard_item").length, 1);
  finish();
  await Promise.resolve();
  await Promise.resolve();
});

test("an older selection response cannot unlock or close the next panel session", async () => {
  const requests = [];
  const ui = await clipboardUi({ selectPromise: () => new Promise((resolve) => requests.push(resolve)) });
  for (const session of [1, 2]) {
    ui.snapshot({ entries: [{ id: 7, text: "alpha" }], panel_open: true, panel_session: session, revision: session });
    await ui.key("ArrowDown");
    await ui.key("Enter");
  }
  requests[0]();
  await Promise.resolve();
  await Promise.resolve();
  ui.snapshot({ entries: [], panel_open: false, panel_session: 1, revision: 1 });
  assert.equal(ui.selecting(), true);
  assert.equal(ui.nodes.get("clipboard-list").children.length, 1);
  assert.deepEqual(ui.calls.filter(({ command }) => command === "select_clipboard_item").map(({ args }) => args.session), [1, 2]);
  requests[1]();
});
