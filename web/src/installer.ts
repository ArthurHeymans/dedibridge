// SPDX-License-Identifier: GPL-2.0-only
import { validateImage, type Board, type Image } from "./images.js";

export type Progress = (phase: string, done: number, total: number) => void;
export interface InstallResult {
  rebooted: boolean;
}
export interface Backend {
  board: Board;
  name: string;
  install(
    image: Image,
    progress: Progress,
    signal: AbortSignal,
  ): Promise<InstallResult>;
  close(): Promise<void>;
}

// This controller owns the USB session. UI code cannot overlap operations,
// reuse a failed session, or skip the image/board/confirmation checks.
export class Installer {
  private backend?: Backend;
  private abort?: AbortController;
  busy = false;
  get connected(): boolean {
    return Boolean(this.backend);
  }
  get deviceName(): string {
    return this.backend?.name ?? "Not connected";
  }

  async connect(request: () => Promise<Backend>): Promise<void> {
    if (this.busy || this.backend)
      throw new Error("Disconnect the current device first.");
    this.busy = true;
    try {
      // No await before request(): preserve the button's user activation.
      this.backend = await request();
    } finally {
      this.busy = false;
    }
  }

  async disconnect(): Promise<void> {
    if (this.busy) throw new Error("An operation is in progress.");
    this.busy = true;
    try {
      await this.close();
    } finally {
      this.busy = false;
    }
  }

  async install(
    image: Image,
    confirmed: boolean,
    progress: Progress,
  ): Promise<InstallResult> {
    if (this.busy) throw new Error("An operation is already in progress.");
    if (!this.backend) throw new Error("Connect a bootloader first.");
    if (!confirmed || this.backend.board !== image.board)
      throw new Error("Confirm the correct board before installing.");
    const validated = validateImage(image.board, image.bytes);
    this.busy = true;
    this.abort = new AbortController();
    try {
      return await this.backend.install(validated, progress, this.abort.signal);
    } finally {
      try {
        await this.close();
      } finally {
        this.abort = undefined;
        this.busy = false;
      }
    }
  }

  cancel(): void {
    this.abort?.abort(
      new Error(
        "Installation stopped. Re-enter bootloader mode before retrying.",
      ),
    );
    // WebUSB aborts pending transfers by closing the device. Do not try to
    // continue/reconnect midway through an interrupted erase or program.
    if (this.abort)
      void this.backend?.close().catch(() => {
        // The active operation reports the disconnect; cleanup still runs there.
      });
  }

  private async close(): Promise<void> {
    const backend = this.backend;
    this.backend = undefined;
    await backend?.close();
  }
}
