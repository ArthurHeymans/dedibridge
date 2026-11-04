import assert from "node:assert/strict";
import { test } from "node:test";
import { Installer } from "../.test-build/src/installer.js";
import { validateImage } from "../.test-build/src/images.js";

const image = () => validateImage("ch32v307", new Uint8Array(8));
function backend(overrides = {}) {
  const calls = [];
  return {
    calls,
    board: "ch32v307",
    name: "test VCT6",
    async install() {
      calls.push("erase");
      calls.push("verify");
      return { rebooted: true };
    },
    async close() {
      calls.push("close");
    },
    ...overrides,
  };
}

test("request callback runs synchronously in the click turn", async () => {
  const installer = new Installer();
  let requested = false;
  const pending = installer.connect(() => {
    requested = true;
    return Promise.resolve(backend());
  });
  assert.equal(requested, true);
  await pending;
  await installer.disconnect();
});

test("no device, confirmation, wrong board and mutated invalid image never erase", async () => {
  const installer = new Installer();
  await assert.rejects(
    installer.install(image(), true, () => {}),
    /Connect/,
  );
  const device = backend();
  await installer.connect(async () => device);
  await assert.rejects(
    installer.install(image(), false, () => {}),
    /Confirm/,
  );
  await assert.rejects(
    installer.install({ ...image(), board: "rp2040" }, true, () => {}),
    /Confirm/,
  );
  const invalid = image();
  invalid.bytes = new Uint8Array();
  await assert.rejects(
    installer.install(invalid, true, () => {}),
    /empty/,
  );
  assert.deepEqual(device.calls, []);
  await installer.disconnect();
});

test("installation verifies, closes, and does not retain a reusable USB session", async () => {
  const installer = new Installer();
  const device = backend();
  await installer.connect(async () => device);
  assert.deepEqual(await installer.install(image(), true, () => {}), {
    rebooted: true,
  });
  assert.deepEqual(device.calls, ["erase", "verify", "close"]);
  assert.equal(installer.connected, false);
  assert.equal(installer.busy, false);
});

test("overlapping operations are rejected; stop aborts and closes the USB session", async () => {
  const installer = new Installer();
  let complete;
  let signal;
  const device = backend({
    async install(_image, _progress, abort) {
      signal = abort;
      await new Promise((resolve) => {
        complete = resolve;
      });
      signal.throwIfAborted();
    },
  });
  await installer.connect(async () => device);
  const running = installer.install(image(), true, () => {});
  await assert.rejects(
    installer.install(image(), true, () => {}),
    /progress/,
  );
  await assert.rejects(installer.disconnect(), /progress/);
  await assert.rejects(
    installer.connect(async () => backend()),
    /Disconnect/,
  );
  installer.cancel();
  assert.equal(signal.aborted, true);
  complete();
  await assert.rejects(running, /stopped/);
  assert.equal(installer.busy, false);
  assert.equal(installer.connected, false);
  assert.ok(device.calls.includes("close"));
});

test("a failed install always closes the device and releases the busy state", async () => {
  const installer = new Installer();
  const device = backend({
    async install() {
      throw new Error("verification failed");
    },
  });
  await installer.connect(async () => device);
  await assert.rejects(
    installer.install(image(), true, () => {}),
    /verification failed/,
  );
  assert.deepEqual(device.calls, ["close"]);
  assert.equal(installer.busy, false);
  assert.equal(installer.connected, false);
});
