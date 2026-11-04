// SPDX-License-Identifier: GPL-2.0-only
import { Installer } from "./installer.js";
import {
  FILENAMES,
  MAX_IMAGE_BYTES,
  sha256,
  validateImage,
  type Board,
  type Image,
} from "./images.js";
import { loadRelease, loadReleaseImage, type Release } from "./releases.js";
import { requestWch } from "./backends/wch.js";
import { requestPico } from "./backends/pico.js";

function element<T extends HTMLElement>(id: string): T {
  const found = document.getElementById(id);
  if (!found) throw new Error(`Missing UI element: ${id}`);
  return found as T;
}
const installer = new Installer();
const controls = {
  board: element<HTMLFieldSetElement>("board-controls"),
  file: element<HTMLInputElement>("file"),
  confirm: element<HTMLInputElement>("confirm"),
  connect: element<HTMLButtonElement>("connect"),
  disconnect: element<HTMLButtonElement>("disconnect"),
  install: element<HTMLButtonElement>("install"),
  stop: element<HTMLButtonElement>("stop"),
  useRelease: element<HTMLButtonElement>("use-release"),
  progress: element<HTMLProgressElement>("progress"),
};
let board: Board = "rp2040";
let release: Release | null = null;
let image: Image | undefined;
let uiBusy = false;
let installing = false;
const supported = window.isSecureContext && Boolean(navigator.usb);

function log(message: string): void {
  const output = element("log");
  output.textContent =
    `${output.textContent}\n${new Date().toLocaleTimeString()}  ${message}`
      .trim()
      .slice(-20000);
  output.scrollTop = output.scrollHeight;
}
function status(message: string, kind = "info"): void {
  element("status").textContent = message;
  element("status").dataset.kind = kind;
}
function render(): void {
  const busy = uiBusy || installer.busy;
  controls.board.disabled = busy;
  // The connected board selection stays fixed until disconnected.
  controls.board.disabled ||= installer.connected;
  controls.file.disabled = busy;
  controls.confirm.disabled = busy;
  controls.useRelease.disabled = busy || !release;
  controls.connect.disabled = busy || installer.connected || !supported;
  controls.disconnect.disabled = busy || !installer.connected;
  controls.install.disabled =
    busy || !image || !installer.connected || !controls.confirm.checked;
  controls.stop.disabled = !installing;
  element("device").textContent = installer.deviceName;
  element("device-indicator").classList.toggle(
    "connected",
    installer.connected,
  );
  element("pico-guide").hidden = board !== "rp2040";
  element("wch-guide").hidden = board !== "ch32v307";
  element("unsupported").hidden = supported;
  const guide = element<HTMLAnchorElement>("board-docs");
  guide.href = `https://github.com/ArthurHeymans/dedibridge/blob/main/docs/boards/${board === "rp2040" ? "pico" : "ch32v307"}.md`;
  controls.file.accept = board === "rp2040" ? ".uf2" : ".bin";
  element("file-format").textContent = `(${controls.file.accept})`;
  const download = element<HTMLAnchorElement>("download");
  download.hidden = !release;
  download.href = `./firmware/${FILENAMES[board]}`;
  download.download = FILENAMES[board];
}

async function action(operation: () => Promise<void>): Promise<void> {
  if (uiBusy) return;
  uiBusy = true;
  render();
  try {
    await operation();
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    const accessDenied =
      /access denied|permission denied|libusb_error_access/i.test(message);
    if (accessDenied)
      element<HTMLDetailsElement>("usb-permissions").open = true;
    status(
      accessDenied
        ? `${message} See USB access & udev rules below, then reconnect the bootloader after applying the permissions.`
        : message,
      "error",
    );
    log(message);
  } finally {
    uiBusy = false;
    render();
  }
}

function clearImage(): void {
  image = undefined;
  controls.confirm.checked = false;
  controls.progress.value = 0;
  element("image-info").hidden = true;
}

async function setImage(value: Image, source: string): Promise<void> {
  const hash = await sha256(value.bytes);
  image = value;
  controls.confirm.checked = false;
  element("image-info").hidden = false;
  element("image-name").textContent = FILENAMES[board];
  element("image-size").textContent =
    `${value.bytes.length.toLocaleString()} bytes · ${source}`;
  element("image-hash").textContent = `SHA-256 ${hash}`;
  status(
    "Image checked. Connect the bootloader, then confirm the board and erase warning.",
  );
  log(`Image validated: ${FILENAMES[board]} · ${source} · SHA-256 ${hash}`);
}

async function useRelease(): Promise<void> {
  clearImage();
  if (!release) return;
  status("Downloading and checking the firmware image…");
  await setImage(
    await loadReleaseImage(release, board),
    `build ${release.tag} · checksum matched`,
  );
}

for (const radio of document.querySelectorAll<HTMLInputElement>(
  'input[name="board"]',
)) {
  radio.addEventListener("change", () => {
    void action(async () => {
      if (installer.connected)
        throw new Error("Disconnect before changing boards.");
      board = radio.value as Board;
      clearImage();
      controls.file.value = "";
      render();
      if (release) await useRelease();
      else status("Choose a local image for this board.");
    });
  });
}
controls.useRelease.addEventListener("click", () => {
  void action(useRelease);
});
controls.file.addEventListener("change", () => {
  const file = controls.file.files?.[0];
  clearImage();
  if (!file) {
    render();
    return;
  }
  void action(async () => {
    const extension = board === "rp2040" ? ".uf2" : ".bin";
    if (!file.name.toLowerCase().endsWith(extension))
      throw new Error(`Choose a ${extension} file for this board.`);
    if (file.size <= 0 || file.size > MAX_IMAGE_BYTES)
      throw new Error("File is empty or too large.");
    await setImage(
      validateImage(board, new Uint8Array(await file.arrayBuffer())),
      "local file · not release-authenticated",
    );
    log(`Local filename: ${file.name}`);
  });
});
controls.confirm.addEventListener("change", render);
controls.connect.addEventListener("click", () => {
  void action(async () => {
    status("Select the bootloader in the browser USB chooser…");
    await installer.connect(board === "rp2040" ? requestPico : requestWch);
    controls.confirm.checked = false;
    status(
      "Bootloader connected. Check the image and confirm before installing.",
    );
    log(`Connected: ${installer.deviceName}`);
  });
});
controls.disconnect.addEventListener("click", () => {
  void action(async () => {
    await installer.disconnect();
    controls.confirm.checked = false;
    status("Disconnected.");
    log("Disconnected.");
  });
});
controls.install.addEventListener("click", () => {
  void action(async () => {
    if (!image) throw new Error("Choose an image first.");
    installing = true;
    render();
    let lastPhase = "";
    try {
      const result = await installer.install(
        image,
        controls.confirm.checked,
        (phase, done, total) => {
          const fraction = total > 0 ? Math.min(1, done / total) : 0;
          const stages: Record<string, number> = {
            erase: 0,
            write: 1,
            verify: 2,
          };
          controls.progress.value =
            (((stages[phase] ?? 0) + fraction) / 3) * 100;
          status(
            `${phase.charAt(0).toUpperCase() + phase.slice(1)} · ${Math.round(fraction * 100)}%`,
          );
          if (phase !== lastPhase) {
            log(`Starting ${phase}.`);
            lastPhase = phase;
          }
        },
      );
      controls.progress.value = 100;
      const message = result.rebooted
        ? "Firmware written and verified. Reboot requested; check that DediBridge enumerates."
        : "Firmware written and verified, but automatic reboot was not confirmed. Restore boot settings and reset/power-cycle manually.";
      status(message, "success");
      log(message);
    } finally {
      installing = false;
      controls.confirm.checked = false;
    }
  });
});
controls.stop.addEventListener("click", () => {
  installer.cancel();
  controls.stop.disabled = true;
  status(
    "Stopping. Firmware may be incomplete; re-enter bootloader mode before retrying.",
  );
  log("Stop requested.");
});
window.addEventListener("pagehide", () => {
  if (installer.busy) installer.cancel();
  else void installer.disconnect();
});

render();
void action(async () => {
  try {
    release = await loadRelease();
  } catch (error) {
    element("release").textContent =
      "Bundled release unavailable. Choose a trusted local image or use the manual download.";
    throw error;
  }
  if (!release) {
    element("release").textContent =
      "No release bundled with this build. Choose a local image or download one from GitHub Releases.";
    status("Choose a local firmware image to begin.");
    return;
  }
  element("release").textContent =
    `${release.tag} · source ${release.commit.slice(0, 12)}`;
  await useRelease();
});
