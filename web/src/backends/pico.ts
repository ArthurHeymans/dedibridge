// SPDX-License-Identifier: GPL-2.0-only
import { Picoboot } from "../../vendor/picoflash/picoboot.js";
import { Target } from "../../vendor/picoflash/target.js";
import { FLASH_START, type Image } from "../images.js";
import type { Backend, InstallResult, Progress } from "../installer.js";

export interface PicoConnection {
  setExclusiveAccess(value: number): Promise<unknown>;
  exitXip(): Promise<unknown>;
  flashErase(address: number, size: number): Promise<unknown>;
  flashWrite(address: number, bytes: Uint8Array): Promise<unknown>;
  flashRead(address: number, size: number): Promise<Uint8Array>;
  reboot(delay: number): Promise<unknown>;
}

export async function installPico(
  connection: PicoConnection,
  image: Image,
  progress: Progress,
  signal: AbortSignal,
): Promise<InstallResult> {
  signal.throwIfAborted();
  await connection.setExclusiveAccess(2);
  signal.throwIfAborted();
  await connection.exitXip();
  const data = image.data;
  const eraseSize = Math.ceil(data.length / 4096) * 4096;
  signal.throwIfAborted();
  progress("erase", 0, eraseSize);
  await connection.flashErase(FLASH_START, eraseSize);
  signal.throwIfAborted();
  progress("erase", eraseSize, eraseSize);
  const chunkSize = 16 * 1024;
  for (let offset = 0; offset < data.length; offset += chunkSize) {
    signal.throwIfAborted();
    const chunk = data.slice(offset, offset + chunkSize);
    await connection.flashWrite(FLASH_START + offset, chunk);
    progress("write", offset + chunk.length, data.length);
  }
  for (let offset = 0; offset < data.length; offset += chunkSize) {
    signal.throwIfAborted();
    const expected = data.subarray(offset, offset + chunkSize);
    const actual = await connection.flashRead(
      FLASH_START + offset,
      expected.length,
    );
    if (
      actual.length !== expected.length ||
      !expected.every((byte, index) => byte === actual[index])
    ) {
      throw new Error(
        `Pico readback verification failed at flash offset 0x${offset.toString(16)}.`,
      );
    }
    progress("verify", offset + expected.length, data.length);
  }
  signal.throwIfAborted();
  try {
    await connection.reboot(100);
    return { rebooted: true };
  } catch {
    signal.throwIfAborted();
    return { rebooted: false };
  }
}

export async function requestPico(): Promise<Backend> {
  const pico = await Picoboot.requestDevice([new Target("RP2040")]);
  const close = async () => {
    try {
      await pico.device.close();
    } catch {}
  };
  try {
    if (pico.getTarget().type !== "RP2040")
      throw new Error("Only the original RP2040 Pico is supported.");
    const connection = await pico.connect();
    return {
      board: "rp2040",
      name: "RP2040 ROM bootloader · confirm your board is an original Pico / Pico H",
      install: (image, progress, signal) =>
        installPico(connection, image, progress, signal),
      close,
    };
  } catch (error) {
    await close();
    throw error;
  }
}
