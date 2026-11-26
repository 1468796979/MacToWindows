const test = require("node:test");
const assert = require("node:assert/strict");
const {
  createRecorder,
  normalizeRule,
  recordedTrigger,
} = require("./rule-editor-core.js");

test("normalizeRule trims fields and supplies a readable default name", () => {
  assert.deepEqual(
    normalizeRule({
      name: "  ",
      trigger: "  chord:left_control+key:8  ",
      action: "  command:key:8  ",
      enabled: true,
    }),
    {
      name: "自定义规则",
      trigger: "chord:left_control+key:8",
      action: "command:key:8",
      enabled: true,
    },
  );
});

test("normalizeRule rejects an incomplete rule", () => {
  assert.throws(
    () => normalizeRule({ name: "test", trigger: "", action: "pass" }),
    /触发键不能为空/,
  );
  assert.throws(
    () => normalizeRule({ name: "test", trigger: "key:0", action: "" }),
    /动作不能为空/,
  );
});

test("recordedTrigger converts a browser key event to the config syntax", () => {
  assert.equal(
    recordedTrigger({ code: "KeyC", ctrlKey: true, shiftKey: false, altKey: false, metaKey: false }),
    "chord:left_control+key:8",
  );
  assert.equal(
    recordedTrigger({ code: "F5", ctrlKey: true, shiftKey: false, altKey: false, metaKey: false }),
    "chord:left_control+key:96",
  );
});

test("recordedTrigger joins multiple modifiers with the backend delimiter", () => {
  assert.equal(recordedTrigger({ code: "KeyB", ctrlKey: true, shiftKey: true, altKey: true }), "chord:left_control|left_option|left_shift+key:11");
});

test("recorder preserves right and simultaneous left/right modifiers", () => {
  const recorder = createRecorder();
  recorder.keyDown({ code: "ControlRight", ctrlKey: true });
  recorder.start();
  recorder.keyDown({ code: "ShiftRight", ctrlKey: true, shiftKey: true });
  assert.equal(recorder.keyDown({ code: "KeyB", ctrlKey: true, shiftKey: true }), "chord:right_control|right_shift+key:11");
  recorder.keyUp({ code: "ControlRight" });
  recorder.keyUp({ code: "ShiftRight" });
  recorder.start();
  recorder.keyDown({ code: "ControlLeft", ctrlKey: true });
  recorder.keyDown({ code: "ControlRight", ctrlKey: true });
  assert.equal(recorder.keyDown({ code: "KeyC", ctrlKey: true }), "chord:left_control|right_control+key:8");
});

test("pure modifier combinations commit after all modifiers are released", () => {
  const recorder = createRecorder();
  recorder.start();
  assert.equal(recorder.keyDown({ code: "ControlRight", ctrlKey: true }), null);
  assert.equal(recorder.keyDown({ code: "ShiftLeft", ctrlKey: true, shiftKey: true }), null);
  assert.equal(recorder.keyUp({ code: "ShiftLeft", ctrlKey: true }), null);
  assert.equal(recorder.keyUp({ code: "ControlRight", ctrlKey: false }), "modifier_chord:right_control|left_shift");
  recorder.start();
  recorder.keyDown({ code: "MetaRight", metaKey: true });
  assert.equal(recorder.keyUp({ code: "MetaRight", metaKey: false }), "right_command");
});

test("a newly pressed modifier after the first release cancels the recording", () => {
  const recorder = createRecorder();
  recorder.start();
  recorder.keyDown({ code: "ControlLeft" });
  recorder.keyDown({ code: "ShiftLeft" });
  assert.equal(recorder.keyUp({ code: "ControlLeft" }), null);
  assert.throws(() => recorder.keyDown({ code: "AltLeft" }), /重新录制/);
  assert.equal(recorder.keyUp({ code: "ShiftLeft" }), null);
  assert.equal(recorder.keyUp({ code: "AltLeft" }), null);
  recorder.start();
  for (const code of ["ControlRight", "ControlLeft", "ShiftRight"]) recorder.keyDown({ code });
  assert.equal(recorder.keyUp({ code: "ControlLeft" }), null);
  assert.equal(recorder.keyUp({ code: "ShiftRight" }), null);
  assert.equal(recorder.keyUp({ code: "ControlRight" }), "modifier_chord:left_control|right_control|right_shift");
});

test("macOS navigation keys and keypad Enter use their physical key codes", () => {
  const keyCodes = { Delete: 117, Home: 115, End: 119, PageUp: 116, PageDown: 121, NumpadEnter: 76 };
  const recorder = createRecorder();
  for (const [code, keyCode] of Object.entries(keyCodes)) {
    recorder.start();
    assert.equal(recorder.keyDown({ code }), `key:${keyCode}`);
  }
  assert.notEqual(recordedTrigger({ code: "Backspace" }), recordedTrigger({ code: "Delete" }));
  assert.notEqual(recordedTrigger({ code: "Enter" }), recordedTrigger({ code: "NumpadEnter" }));
});

test("blur reset and modifier releases prevent stale recorded sides", () => {
  const recorder = createRecorder();
  recorder.keyDown({ code: "ControlRight", ctrlKey: true });
  recorder.reset();
  recorder.start();
  assert.throws(() => recorder.keyDown({ code: "KeyC", ctrlKey: true }), /侧别未知/);
  recorder.reset();
  recorder.keyDown({ code: "ControlRight", ctrlKey: true });
  recorder.keyUp({ code: "ControlRight", ctrlKey: false });
  recorder.start();
  assert.equal(recorder.keyDown({ code: "KeyC", ctrlKey: false }), "key:8");
});
