import assert from "node:assert/strict";
import { test } from "node:test";
import { createHash } from "node:crypto";
import { mkdtemp, readFile, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  parseRelease,
  readResponse,
  loadReleaseImage,
} from "../.test-build/src/releases.js";
import { bundleFirmware } from "../scripts/firmware.mjs";
import { manifest, uf2 } from "./fixtures.mjs";

test("manifest constrains filenames, hashes, sizes and revision metadata", () => {
  assert.equal(parseRelease(manifest()).tag, "v1.0");
  for (const mutate of [
    (m) => {
      m.schema = 2;
    },
    (m) => {
      m.commit = "main";
    },
    (m) => {
      m.tag = "";
    },
    (m) => {
      m.files.rp2040.name = "https://example.com/malicious.uf2";
    },
    (m) => {
      m.files.ch32v307.size = 0;
    },
    (m) => {
      m.files.ch32v307.size = 1.5;
    },
    (m) => {
      m.files.ch32v307.sha256 = "bad";
    },
    (m) => {
      delete m.files.rp2040;
    },
  ]) {
    const value = manifest();
    mutate(value);
    assert.throws(() => parseRelease(value));
  }
  assert.throws(() => parseRelease(null));
});

test("streaming downloads reject oversized and HTTP error responses", async () => {
  await assert.rejects(
    readResponse(new Response(new Uint8Array(8)), 4),
    /expected size/,
  );
  await assert.rejects(
    readResponse(new Response("missing", { status: 404 }), 100),
    /404/,
  );
  assert.equal(
    (await readResponse(new Response(new Uint8Array(4)), 4)).length,
    4,
  );
});

test("release bytes are structurally validated only after matching size and hash", async (t) => {
  const data = uf2();
  const release = manifest();
  release.files.rp2040.sha256 = createHash("sha256").update(data).digest("hex");
  const fetch = t.mock.method(globalThis, "fetch", async (url) => {
    assert.equal(url, "./firmware/dedi-rp2040.uf2");
    return new Response(data);
  });
  assert.equal((await loadReleaseImage(release, "rp2040")).data.length, 512);
  release.files.rp2040.sha256 = "0".repeat(64);
  await assert.rejects(loadReleaseImage(release, "rp2040"), /mismatch/);
  fetch.mock.restore();
});

test("firmware bundling copies exact release bytes and rejects checksum tampering", async () => {
  const root = await mkdtemp(join(tmpdir(), "dedibridge-release-"));
  try {
    const files = new Map([
      ["dedi-rp2040.uf2", uf2()],
      ["dedi-ch32v307.bin", new Uint8Array(8)],
      ["dedi-rp2040.elf", new Uint8Array(16)],
      ["dedi-ch32v307.elf", new Uint8Array(16)],
    ]);
    let sums = "";
    for (const [name, bytes] of files) {
      await writeFile(join(root, name), bytes);
      sums += `${createHash("sha256").update(bytes).digest("hex")}  ${name}\n`;
    }
    await writeFile(join(root, "SHA256SUMS"), sums);
    const options = {
      input: root,
      output: join(root, "site"),
      tag: "v1.0",
      commit: "a".repeat(40),
    };
    await bundleFirmware(options);
    const bundled = parseRelease(
      JSON.parse(await readFile(join(options.output, "manifest.json"), "utf8")),
    );
    assert.equal(bundled.tag, "v1.0");
    for (const [name, bytes] of files)
      assert.deepEqual(
        new Uint8Array(await readFile(join(options.output, name))),
        bytes,
      );
    await writeFile(join(root, "dedi-ch32v307.bin"), new Uint8Array(9));
    await assert.rejects(bundleFirmware(options), /checksum mismatch/);
    await writeFile(
      join(root, "SHA256SUMS"),
      sums + sums.split("\n")[0] + "\n",
    );
    await assert.rejects(bundleFirmware(options), /duplicate/);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
