// SPDX-License-Identifier: GPL-2.0-only
export type Board = "rp2040" | "ch32v307";
export const FLASH_START = 0x10000000;
export const MAX_IMAGE_BYTES = 4 * 1024 * 1024; // UF2 has 2x overhead for the 2 MiB Pico.
export const FILENAMES: Record<Board, string> = {
  rp2040: "dedi-rp2040.uf2",
  ch32v307: "dedi-ch32v307.bin",
};

export interface Image {
  board: Board;
  bytes: Uint8Array;
  data: Uint8Array;
}

function require(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

export function validateImage(board: Board, source: Uint8Array): Image {
  require(source.length > 0 &&
    source.length <= MAX_IMAGE_BYTES, "Image is empty or too large.");
  const bytes = Uint8Array.from(source);
  const view = new DataView(bytes.buffer);
  if (board === "ch32v307") {
    // Stay within the smallest supported flash partition. The adapter also
    // rejects the 32 KiB SRAM partition; it cannot run this firmware safely.
    require(bytes.length >= 4 &&
      bytes.length <=
        192 * 1024, "CH32 image must be a raw BIN of 4–196608 bytes.");
    require(view.getUint32(0, true) !== 0x464c457f &&
      view.getUint32(0, true) !==
        0x0a324655, "Choose the CH32 BIN, not an ELF or Pico UF2.");
    return { board, bytes, data: bytes };
  }
  require(board === "rp2040", "Unsupported board.");
  require(bytes.length % 512 ===
    0, "UF2 length is not a multiple of 512 bytes.");
  const count = bytes.length / 512;
  const seen = new Set<number>();
  const blocks: { address: number; payload: Uint8Array }[] = [];
  for (let i = 0; i < count; i++) {
    const offset = i * 512;
    require(view.getUint32(offset, true) === 0x0a324655 &&
      view.getUint32(offset + 4, true) === 0x9e5d5157 &&
      view.getUint32(offset + 508, true) ===
        0x0ab16f30, "Invalid UF2 block magic.");
    require(view.getUint32(offset + 8, true) ===
      0x2000, "Unsupported UF2 flags.");
    require(view.getUint32(offset + 28, true) ===
      0xe48bff56, "UF2 is not for RP2040.");
    const address = view.getUint32(offset + 12, true);
    const size = view.getUint32(offset + 16, true);
    const number = view.getUint32(offset + 20, true);
    require(size === 256, "Expected 256-byte UF2 flash payloads.");
    require(view.getUint32(offset + 24, true) === count &&
      number < count &&
      !seen.has(number), "Incomplete or duplicate UF2 block sequence.");
    require(address % 256 === 0 &&
      address >= FLASH_START &&
      address + size <=
        FLASH_START +
          2 * 1024 * 1024, "UF2 address is outside the original Pico flash.");
    seen.add(number);
    blocks.push({
      address,
      payload: bytes.slice(offset + 32, offset + 32 + size),
    });
  }
  blocks.sort((a, b) => a.address - b.address);
  const data = new Uint8Array(count * 256);
  for (const [index, block] of blocks.entries()) {
    require(block.address ===
      FLASH_START +
        index *
          256, "UF2 must start at flash offset 0, without gaps or overlaps.");
    data.set(block.payload, index * 256);
  }
  return { board, bytes, data };
}

export async function sha256(bytes: Uint8Array): Promise<string> {
  const hash = await crypto.subtle.digest(
    "SHA-256",
    Uint8Array.from(bytes).buffer,
  );
  return Array.from(new Uint8Array(hash), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}
