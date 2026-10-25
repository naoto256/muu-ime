import assert from "node:assert/strict";
import test from "node:test";
import { copyCode } from "../src/copy-code.js";
import { installCommand } from "../src/pages.11ty.js";

function controls() {
  const attributes = new Map();
  return {
    button: {
      disabled: false,
      innerHTML: "copy",
      setAttribute: (key, value) => attributes.set(key, value),
    },
    status: { textContent: "" },
    attributes,
  };
}

test("copying sends the exact command only on the explicit operation", async () => {
  const calls = [];
  const { button, status, attributes } = controls();
  const clipboard = {
    async writeText(value) {
      calls.push(value);
    },
  };
  assert.deepEqual(calls, []);
  await copyCode({ textContent: installCommand }, button, status, clipboard);
  assert.deepEqual(calls, [installCommand]);
  assert.equal(button.disabled, false);
  assert.equal(attributes.get("aria-label"), "コピーしました");
  assert.equal(status.textContent, "コピーしました");
});

test("a missing or denied clipboard keeps manual copying available", async () => {
  for (const clipboard of [
    undefined,
    {
      async writeText() {
        throw new Error("denied");
      },
    },
  ]) {
    const { button, status, attributes } = controls();
    await copyCode({ textContent: installCommand }, button, status, clipboard);
    assert.equal(button.disabled, false);
    assert.equal(button.innerHTML, "copy");
    assert.match(attributes.get("aria-label"), /文字列を選択/);
    assert.match(status.textContent, /コピーできません/);
  }
});
