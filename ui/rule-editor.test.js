const test = require("node:test");
const assert = require("node:assert/strict");
const {
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
