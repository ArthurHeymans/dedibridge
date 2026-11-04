import assert from "node:assert/strict";
import { test } from "node:test";
import { ChipDatabase } from "wchisp-web";
import { checkWchDevice } from "../.test-build/src/backends/wch.js";
import { installPico } from "../.test-build/src/backends/pico.js";
import { validateImage } from "../.test-build/src/images.js";
import { uf2 } from "./fixtures.mjs";

function wchInfo() {
  const configRaw = new Uint8Array(26);
  configRaw[2] = 0xa5;
  configRaw.fill(0xff, 10, 14);
  return {
    chipId: 0x70,
    deviceType: 0x17,
    chip: new ChipDatabase().find(0x70, 0x17),
    configRaw,
    uid: new Uint8Array(8),
    codeFlashProtected: false,
  };
}

test("only unprotected CH32V307VCT6 with adequate SRAM is accepted", () => {
  checkWchDevice(wchInfo());
  for (const override of [
    { chipId: 0x71 },
    { deviceType: 0x10 },
    { chip: undefined },
    { uid: new Uint8Array(4) },
    { configRaw: new Uint8Array(25) },
    { codeFlashProtected: true },
  ])
    assert.throws(() => checkWchDevice({ ...wchInfo(), ...override }));
  const writeProtected = wchInfo();
  writeProtected.configRaw[10] = 0;
  assert.throws(() => checkWchDevice(writeProtected), /protected/);
  const smallRam = wchInfo();
  smallRam.configRaw[4] = 0xc0;
  assert.throws(() => checkWchDevice(smallRam), /32 KiB/);
});

function picoConnection(overrides = {}) {
  const calls = [];
  const flash = new Uint8Array(2 * 1024 * 1024);
  return {
    calls,
    async setExclusiveAccess() {
      calls.push("exclusive");
    },
    async exitXip() {
      calls.push("exit-xip");
    },
    async flashErase(address, size) {
      calls.push(["erase", address, size]);
      flash.fill(0xff, address - 0x10000000, address - 0x10000000 + size);
    },
    async flashWrite(address, data) {
      calls.push("write");
      flash.set(data, address - 0x10000000);
    },
    async flashRead(address, size) {
      calls.push("verify");
      return flash.slice(address - 0x10000000, address - 0x10000000 + size);
    },
    async reboot() {
      calls.push("reboot");
    },
    ...overrides,
  };
}

test("Pico erases sector-aligned, writes, reads back, and only then reboots", async () => {
  const connection = picoConnection();
  const result = await installPico(
    connection,
    validateImage("rp2040", uf2()),
    () => {},
    new AbortController().signal,
  );
  assert.deepEqual(result, { rebooted: true });
  assert.deepEqual(connection.calls, [
    "exclusive",
    "exit-xip",
    ["erase", 0x10000000, 4096],
    "write",
    "verify",
    "reboot",
  ]);
});

test("Pico corruption, short reads and transfer errors prevent reboot", async () => {
  for (const override of [
    {
      async flashRead(_addr, size) {
        return new Uint8Array(size);
      },
    },
    {
      async flashRead() {
        return new Uint8Array(1);
      },
    },
    {
      async flashWrite() {
        throw new Error("USB disconnected");
      },
    },
  ]) {
    const connection = picoConnection(override);
    await assert.rejects(
      installPico(
        connection,
        validateImage("rp2040", uf2()),
        () => {},
        new AbortController().signal,
      ),
    );
    assert.ok(!connection.calls.includes("reboot"));
  }
});

test("Pico reboot failure is distinguished from a verified write", async () => {
  const connection = picoConnection({
    async reboot() {
      throw new Error("reset disconnect");
    },
  });
  assert.deepEqual(
    await installPico(
      connection,
      validateImage("rp2040", uf2()),
      () => {},
      new AbortController().signal,
    ),
    { rebooted: false },
  );
});

test("abort before Pico erase is non-destructive", async () => {
  const connection = picoConnection();
  const abort = new AbortController();
  abort.abort();
  await assert.rejects(
    installPico(
      connection,
      validateImage("rp2040", uf2()),
      () => {},
      abort.signal,
    ),
  );
  assert.deepEqual(connection.calls, []);
});
