// SPDX-License-Identifier: GPL-2.0-only
import {
  FILENAMES,
  MAX_IMAGE_BYTES,
  sha256,
  validateImage,
  type Board,
  type Image,
} from "./images.js";

export interface ReleaseFile {
  name: string;
  size: number;
  sha256: string;
}
export interface Release {
  schema: 1;
  tag: string;
  commit: string;
  files: Record<Board, ReleaseFile>;
}

export function parseRelease(value: unknown): Release {
  const input = value as Partial<Release> | null;
  if (
    !input ||
    input.schema !== 1 ||
    typeof input.tag !== "string" ||
    !input.tag ||
    input.tag.length > 128 ||
    typeof input.commit !== "string" ||
    !/^[a-f0-9]{40}$/.test(input.commit)
  ) {
    throw new Error("Invalid release manifest.");
  }
  const files = {} as Record<Board, ReleaseFile>;
  for (const board of ["rp2040", "ch32v307"] as const) {
    const file = input.files?.[board];
    if (
      !file ||
      file.name !== FILENAMES[board] ||
      !Number.isSafeInteger(file.size) ||
      file.size <= 0 ||
      file.size > MAX_IMAGE_BYTES ||
      typeof file.sha256 !== "string" ||
      !/^[a-f0-9]{64}$/.test(file.sha256)
    ) {
      throw new Error(`Invalid release image metadata for ${board}.`);
    }
    files[board] = { ...file };
  }
  return { schema: 1, tag: input.tag, commit: input.commit, files };
}

export async function readResponse(
  response: Response,
  limit: number,
): Promise<Uint8Array> {
  if (!response.ok || !response.body)
    throw new Error(`Download failed (HTTP ${response.status}).`);
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > limit) throw new Error("Download exceeds the expected size.");
      chunks.push(value);
    }
  } catch (error) {
    await reader.cancel().catch(() => {});
    throw error;
  } finally {
    reader.releaseLock();
  }
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.length;
  }
  return bytes;
}

export async function loadRelease(): Promise<Release | null> {
  const response = await fetch("./firmware/manifest.json", {
    cache: "no-store",
    signal: AbortSignal.timeout(15000),
  });
  if (response.status === 404) return null;
  const text = new TextDecoder().decode(
    await readResponse(response, 16 * 1024),
  );
  try {
    return parseRelease(JSON.parse(text));
  } catch (cause) {
    throw new Error(
      "The release manifest is invalid. Use a trusted local image or the manual installer.",
      { cause },
    );
  }
}

export async function loadReleaseImage(
  release: Release,
  board: Board,
): Promise<Image> {
  const file = release.files[board];
  const response = await fetch(`./firmware/${FILENAMES[board]}`, {
    cache: "no-store",
    signal: AbortSignal.timeout(30000),
  });
  const bytes = await readResponse(response, file.size);
  if (bytes.length !== file.size || (await sha256(bytes)) !== file.sha256) {
    throw new Error(
      "Release image checksum/size mismatch. Nothing was flashed.",
    );
  }
  return validateImage(board, bytes);
}
