export function uf2(count = 2) {
  const bytes = new Uint8Array(count * 512);
  const view = new DataView(bytes.buffer);
  for (let i = 0; i < count; i++) {
    const offset = i * 512;
    for (const [field, value] of [
      [0, 0x0a324655],
      [4, 0x9e5d5157],
      [8, 0x2000],
      [12, 0x10000000 + i * 256],
      [16, 256],
      [20, i],
      [24, count],
      [28, 0xe48bff56],
      [508, 0x0ab16f30],
    ])
      view.setUint32(offset + field, value, true);
    bytes.fill(i + 1, offset + 32, offset + 288);
  }
  return bytes;
}

export function manifest() {
  return {
    schema: 1,
    tag: "v1.0",
    commit: "a".repeat(40),
    files: {
      rp2040: { name: "dedi-rp2040.uf2", size: 1024, sha256: "b".repeat(64) },
      ch32v307: { name: "dedi-ch32v307.bin", size: 8, sha256: "c".repeat(64) },
    },
  };
}
