import assert from "node:assert/strict";
import { test } from "node:test";
import { validateImage, sha256 } from "../.test-build/src/images.js";
import { uf2 } from "./fixtures.mjs";

test("valid UF2 is reconstructed, independent of physical block order or buffer offsets", () => {
  const input = uf2();
  const shuffled = new Uint8Array([
    ...input.subarray(512),
    ...input.subarray(0, 512),
  ]);
  const image = validateImage("rp2040", Buffer.from(shuffled));
  assert.equal(image.data.length, 512);
  assert.ok(image.data.subarray(0, 256).every((byte) => byte === 1));
  assert.ok(image.data.subarray(256).every((byte) => byte === 2));
  shuffled.fill(0);
  assert.notEqual(image.bytes[0], 0);
});

test("malformed, wrong-family, out-of-bounds and incomplete UF2 are rejected", () => {
  for (const [offset, value] of [
    [0, 0],
    [4, 0],
    [508, 0],
    [8, 0],
    [28, 0xe48bff59],
    [12, 0x20000000],
    [12, 0x10000001],
    [12, 0x10200000],
    [16, 257],
    [20, 2],
    [24, 3],
    [512 + 20, 0],
    [512 + 12, 0x10000000],
    [512 + 12, 0x10000200],
  ]) {
    const bytes = uf2();
    new DataView(bytes.buffer).setUint32(offset, value, true);
    assert.throws(
      () => validateImage("rp2040", bytes),
      undefined,
      `${offset}=${value}`,
    );
  }
  assert.throws(() => validateImage("rp2040", uf2().subarray(0, 511)));
  assert.throws(() => validateImage("rp2040", uf2().subarray(0, 512)));
  assert.throws(() => validateImage("rp2040", new Uint8Array()));
});

test("CH32 accepts a bounded raw binary but rejects ELF, UF2 and invalid sizes", () => {
  assert.equal(validateImage("ch32v307", new Uint8Array(8)).data.length, 8);
  for (const bytes of [
    new Uint8Array(),
    new Uint8Array(3),
    new Uint8Array(192 * 1024 + 1),
    uf2(),
    Uint8Array.of(0x7f, 0x45, 0x4c, 0x46),
  ]) {
    assert.throws(() => validateImage("ch32v307", bytes));
  }
});

test("SHA-256 hashes exactly the supplied bytes, including Buffer slices", async () => {
  const bytes = Buffer.from("prefixabcsuffix").subarray(6, 9);
  assert.equal(
    await sha256(bytes),
    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
  );
});
