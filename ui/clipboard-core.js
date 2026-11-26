(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  if (root) root.clipboardCore = api;
})(typeof globalThis !== "undefined" ? globalThis : this, () => {
  function normalizeSnapshot(snapshot) {
    return {
      entries: Array.isArray(snapshot?.entries) ? snapshot.entries : [],
      panelOpen: Boolean(snapshot?.panel_open ?? snapshot?.panelOpen),
      canPaste: Boolean(snapshot?.can_paste),
      panelSession: snapshot?.panel_session ?? 0,
      revision: snapshot?.revision ?? 0,
      lastError: snapshot?.last_error ?? snapshot?.lastError ?? null,
      operationError: snapshot?.operation_error ?? snapshot?.operationError ?? null,
    };
  }

  function moveSelection(index, delta, count) {
    if (!Number.isInteger(count) || count <= 0) return -1;
    const current = Number.isInteger(index) && index >= 0 && index < count
      ? index
      : (delta > 0 ? -1 : 0);
    return (current + delta + count) % count;
  }

  return { normalizeSnapshot, moveSelection };
});
