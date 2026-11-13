const test = require("node:test");
const assert = require("node:assert/strict");
const { normalizeSnapshot, previewText, moveSelection, resetSelection } = require("./clipboard-core.js");

test("normalizeSnapshot supplies safe defaults for an empty history", () => {
  assert.deepEqual(normalizeSnapshot(), { entries: [], panelOpen: false, lastError: null });
});

test("previewText collapses whitespace and clips long content", () => {
  assert.equal(previewText("  alpha\n beta  ", 20), "alpha beta");
  assert.equal(previewText("1234567890", 5), "12345…");
});

test("moveSelection wraps around the history", () => {
  assert.equal(moveSelection(0, -1, 3), 2);
  assert.equal(moveSelection(2, 1, 3), 0);
  assert.equal(moveSelection(-1, 1, 0), -1);
});

test("resetSelection clears stale selection when panel opens", () => {
  assert.equal(resetSelection(), -1);
});
