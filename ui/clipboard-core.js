(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  if (root) root.clipboardCore = api;
})(typeof globalThis !== "undefined" ? globalThis : this, () => {
  function normalizeSnapshot(snapshot) {
    return {
      entries: Array.isArray(snapshot?.entries) ? snapshot.entries : [],
      panelOpen: Boolean(snapshot?.panel_open ?? snapshot?.panelOpen),
      lastError: snapshot?.last_error ?? snapshot?.lastError ?? null,
    };
  }

  function previewText(value, maxLength = 120) {
    const text = String(value ?? "").replace(/\s+/g, " ").trim();
    return text.length > maxLength ? text.slice(0, maxLength) + "…" : text;
  }

  function moveSelection(index, delta, count) {
    if (!Number.isInteger(count) || count <= 0) return -1;
    const current = Number.isInteger(index) ? index : 0;
    return (current + delta + count) % count;
  }

  function resetSelection() {
    return -1;
  }

  return { normalizeSnapshot, previewText, moveSelection, resetSelection };
});
