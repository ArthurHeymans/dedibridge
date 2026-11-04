// SPDX-License-Identifier: GPL-2.0-only
import { createHash } from "node:crypto";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { resolve, join } from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import { validateImage } from "../src/images.ts";

const expected = [
  "dedi-rp2040.elf",
  "dedi-rp2040.uf2",
  "dedi-ch32v307.elf",
  "dedi-ch32v307.bin",
];

export async function bundleFirmware({ input, output, tag, commit }) {
  if (
    !input ||
    !output ||
    !tag ||
    tag.length > 128 ||
    !/^[a-f0-9]{40}$/.test(commit ?? "")
  ) {
    throw new Error(
      "Expected --input, --output, --tag and a full --commit SHA.",
    );
  }
  if (resolve(input) === resolve(output))
    throw new Error("Input and output must be separate directories.");
  const sums = await readFile(join(input, "SHA256SUMS"), "utf8");
  const hashes = new Map();
  for (const line of sums.trim().split("\n")) {
    const match = /^([a-f0-9]{64})  (dedi-[a-z0-9]+\.(?:elf|uf2|bin))$/.exec(
      line,
    );
    if (!match || !expected.includes(match[2]) || hashes.has(match[2]))
      throw new Error("Invalid or duplicate SHA256SUMS entry.");
    hashes.set(match[2], match[1]);
  }
  const contents = new Map();
  for (const name of expected) {
    const bytes = await readFile(join(input, name));
    if (
      !hashes.has(name) ||
      createHash("sha256").update(bytes).digest("hex") !== hashes.get(name)
    ) {
      throw new Error(`Release checksum mismatch: ${name}`);
    }
    contents.set(name, bytes);
  }
  validateImage("rp2040", contents.get("dedi-rp2040.uf2"));
  validateImage("ch32v307", contents.get("dedi-ch32v307.bin"));
  const files = {};
  for (const [board, name] of [
    ["rp2040", "dedi-rp2040.uf2"],
    ["ch32v307", "dedi-ch32v307.bin"],
  ]) {
    files[board] = {
      name,
      size: contents.get(name).length,
      sha256: hashes.get(name),
    };
  }
  await mkdir(output, { recursive: true });
  // Write the verified bytes, not a second read of potentially changed inputs.
  for (const [name, bytes] of contents)
    await writeFile(join(output, name), bytes);
  await writeFile(join(output, "SHA256SUMS"), sums);
  await writeFile(
    join(output, "manifest.json"),
    JSON.stringify({ schema: 1, tag, commit, files }, null, 2) + "\n",
  );
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(resolve(process.argv[1])).href
) {
  const { values } = parseArgs({
    options: {
      input: { type: "string" },
      output: { type: "string" },
      tag: { type: "string" },
      commit: { type: "string" },
    },
  });
  await bundleFirmware(values);
}
