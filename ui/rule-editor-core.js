(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  if (root) root.ruleEditorCore = api;
})(typeof globalThis !== "undefined" ? globalThis : this, () => {
  const KEY_CODES = {
    KeyA: 0, KeyS: 1, KeyD: 2, KeyF: 3, KeyH: 4, KeyG: 5, KeyZ: 6, KeyX: 7,
    KeyC: 8, KeyV: 9, KeyB: 11, KeyQ: 12, KeyW: 13, KeyE: 14, KeyR: 15,
    KeyY: 16, KeyT: 17, Digit1: 18, Digit2: 19, Digit3: 20, Digit4: 21,
    Digit6: 22, Digit5: 23, Equal: 24, Digit9: 25, Digit7: 26, Minus: 27,
    Digit8: 28, Digit0: 29, BracketRight: 30, KeyO: 31, KeyU: 32, BracketLeft: 33,
    KeyI: 34, KeyP: 35, Enter: 36, KeyL: 37, KeyJ: 38, Quote: 39, KeyK: 40,
    Semicolon: 41, Backslash: 42, Comma: 43, Slash: 44, KeyN: 45, KeyM: 46,
    Period: 47, Tab: 48, Space: 49, Backquote: 50, Backspace: 51, Escape: 53,
    MetaLeft: 55, MetaRight: 54, ShiftLeft: 56, CapsLock: 57, AltLeft: 58, ControlLeft: 59,
    ShiftRight: 60, AltRight: 61, ControlRight: 62, ArrowRight: 124, ArrowLeft: 123,
    ArrowDown: 125, ArrowUp: 126, F1: 122, F2: 120, F3: 99, F4: 118, F5: 96,
    F6: 97, F7: 98, F8: 100, F9: 101, F10: 109, F11: 103, F12: 111,
  };

  const MODIFIER_CODES = new Set([
    "ControlLeft", "ControlRight", "AltLeft", "AltRight", "ShiftLeft", "ShiftRight",
    "MetaLeft", "MetaRight",
  ]);

  function normalizeRule(input) {
    const name = String(input?.name || "").trim() || "自定义规则";
    const trigger = String(input?.trigger || "").trim();
    const action = String(input?.action || "").trim();
    if (!trigger) throw new Error("触发键不能为空");
    if (!action) throw new Error("动作不能为空");
    return { name, trigger, action, enabled: input?.enabled !== false };
  }

  function modifierName(code) {
    return {
      ControlLeft: "left_control", ControlRight: "right_control",
      AltLeft: "left_option", AltRight: "right_option",
      ShiftLeft: "left_shift", ShiftRight: "right_shift",
      MetaLeft: "left_command", MetaRight: "right_command",
    }[code];
  }

  const MODIFIER_GROUPS = [
    ["ctrlKey", ["ControlLeft", "ControlRight"]],
    ["altKey", ["AltLeft", "AltRight"]],
    ["shiftKey", ["ShiftLeft", "ShiftRight"]],
    ["metaKey", ["MetaLeft", "MetaRight"]],
  ];

  function recordedTrigger(event, heldCodes) {
    const code = event?.code;
    const keyCode = KEY_CODES[code];
    if (keyCode === undefined) throw new Error("无法识别该按键，请改用文本填写");
    const modifiers = [];
    for (const [flag, codes] of MODIFIER_GROUPS) {
      if (heldCodes) {
        const held = codes.filter((candidate) => heldCodes.has(candidate));
        if (event[flag] && !held.length && !codes.includes(code)) {
          throw new Error("修饰键侧别未知，请松开所有修饰键后重新录制");
        }
        modifiers.push(...held.filter((candidate) => candidate !== code).map(modifierName));
      } else if (event[flag] && !codes.includes(code)) {
        modifiers.push(modifierName(codes[0]));
      }
    }
    if (MODIFIER_CODES.has(code)) {
      const names = [...modifiers, modifierName(code)];
      return names.length === 1 ? names[0] : `modifier_chord:${names.join("|")}`;
    }
    return modifiers.length ? `chord:${modifiers.join("|")}+key:${keyCode}` : `key:${keyCode}`;
  }

  function createRecorder() {
    const held = new Set();
    let chord = new Set();
    let recording = false;
    const cancel = () => { recording = false; chord.clear(); };
    return {
      start() { recording = true; chord = new Set(held); },
      cancel,
      reset() { cancel(); held.clear(); },
      keyDown(event) {
        if (MODIFIER_CODES.has(event.code)) {
          held.add(event.code);
          if (recording) held.forEach((code) => chord.add(code));
          return null;
        }
        if (!recording) return null;
        const result = recordedTrigger(event, held);
        cancel();
        return result;
      },
      keyUp(event) {
        if (!MODIFIER_CODES.has(event.code)) return null;
        held.delete(event.code);
        if (!recording || held.size || !chord.size) return null;
        const names = Array.from(MODIFIER_CODES).filter((code) => chord.has(code)).map(modifierName);
        cancel();
        return names.length === 1 ? names[0] : `modifier_chord:${names.join("|")}`;
      },
    };
  }

  return { normalizeRule, recordedTrigger, createRecorder };
});
