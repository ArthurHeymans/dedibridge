import { test, expect } from "@playwright/test";
import { uf2, manifest } from "../fixtures.mjs";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";

async function mockWch(page, { chipId = 0x70, stallWrite = false } = {}) {
  await page.addInitScript(
    ({ chipId, stallWrite }) => {
      window.usbCommands = [];
      let command = 0;
      let rejectRead;
      const device = {
        opened: false,
        vendorId: 0x4348,
        productId: 0x55e0,
        configuration: {
          configurationValue: 1,
          interfaces: [
            {
              interfaceNumber: 0,
              alternates: [
                {
                  interfaceClass: 0xff,
                  endpoints: [
                    {
                      endpointNumber: 2,
                      direction: "in",
                      type: "bulk",
                      packetSize: 64,
                    },
                    {
                      endpointNumber: 2,
                      direction: "out",
                      type: "bulk",
                      packetSize: 64,
                    },
                  ],
                },
              ],
            },
          ],
        },
        async open() {
          this.opened = true;
        },
        async claimInterface() {},
        async close() {
          this.opened = false;
          rejectRead?.(new DOMException("USB session closed", "AbortError"));
        },
        async transferOut(_endpoint, bytes) {
          command = bytes[0];
          window.usbCommands.push(command);
          return { status: "ok", bytesWritten: bytes.byteLength };
        },
        async transferIn() {
          if (command === 0xa5 && stallWrite)
            return new Promise((_resolve, reject) => {
              rejectRead = reject;
            });
          let payload = [0];
          if (command === 0xa1) payload = [chipId, 0x17];
          if (command === 0xa7) {
            payload = Array(26).fill(0);
            payload[2] = 0xa5;
            payload.fill(0xff, 10, 14);
            payload.splice(18, 8, 1, 2, 3, 4, 5, 6, 7, 8);
          }
          if (command === 0xa3) payload = [0x90]; // Native ISP XOR key checksum for UID 1..8 and chip 0x70.
          return {
            status: "ok",
            data: new DataView(
              Uint8Array.from([
                command,
                0,
                payload.length,
                0,
                ...payload,
              ]).buffer,
            ),
          };
        },
      };
      Object.defineProperty(navigator, "usb", {
        value: { requestDevice: async () => device },
      });
    },
    { chipId, stallWrite },
  );
}

async function openLocalBuild(page) {
  // Tests always exercise local-file mode, even when dist has release images.
  await page.route("**/firmware/manifest.json", (route) =>
    route.fulfill({ status: 404, body: "" }),
  );
  await page.goto("./");
  await expect(page.getByRole("status")).toHaveText(
    "Choose a local firmware image to begin.",
  );
}

async function loadWchImage(page) {
  await page.getByRole("radio", { name: /CH32V307 devboard/ }).check();
  await page.locator("#file").setInputFiles({
    name: "dedi-ch32v307.bin",
    mimeType: "application/octet-stream",
    buffer: Buffer.from([0x6f, 0, 0, 0, 1, 2, 3, 4]),
  });
  await expect(page.locator("#image-info")).toBeVisible();
}

async function mockRelease(page, corrupt = false) {
  const data = uf2();
  const release = manifest();
  release.files.rp2040.sha256 = createHash("sha256").update(data).digest("hex");
  await page.route("**/firmware/manifest.json", (route) =>
    route.fulfill({ json: release }),
  );
  await page.route("**/firmware/dedi-rp2040.uf2", (route) =>
    route.fulfill({
      body: Buffer.from(corrupt ? new Uint8Array(data.length) : data),
      contentType: "application/octet-stream",
    }),
  );
}

test("Pico access denied opens actionable bootloader udev help", async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.addInitScript(() => {
    const configuration = {
      configurationValue: 1,
      interfaces: [
        {
          interfaceNumber: 0,
          alternates: [
            {
              interfaceClass: 0xff,
              interfaceSubclass: 0,
              endpoints: [
                {
                  endpointNumber: 1,
                  direction: "out",
                  type: "bulk",
                  packetSize: 64,
                },
                {
                  endpointNumber: 2,
                  direction: "in",
                  type: "bulk",
                  packetSize: 64,
                },
              ],
            },
          ],
        },
      ],
    };
    Object.defineProperty(navigator, "usb", {
      value: {
        requestDevice: async () => ({
          opened: false,
          vendorId: 0x2e8a,
          productId: 0x0003,
          configurations: [configuration],
          async open() {
            throw new DOMException(
              "Failed to execute 'open' on 'USBDevice': Access denied.",
              "SecurityError",
            );
          },
          async close() {},
        }),
      },
    });
  });
  await openLocalBuild(page);
  await page.locator("#connect").click();
  await expect(page.getByRole("status")).toContainText("Access denied");
  await expect(page.locator("#usb-permissions")).toHaveAttribute("open", "");
  await expect(page.locator("#usb-permissions")).toContainText("2e8a:0003");
  await expect(page.locator("#usb-permissions")).toContainText(
    "sudo udevadm control --reload-rules",
  );
  await expect(page.locator("#usb-permissions")).toContainText(
    "sudo nixos-rebuild switch",
  );
  await expect(page.locator("#usb-permissions")).toContainText(
    "unplug the board",
  );
  await expect(page.locator("#connect")).toBeEnabled();
  await expect(page.locator("#install")).toBeDisabled();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
});

test("displayed and downloadable udev rules match the canonical file", async ({
  page,
}) => {
  await openLocalBuild(page);
  const expected = await readFile(
    new URL("../../../udev/70-dedibridge.rules", import.meta.url),
    "utf8",
  );
  expect(
    await page.locator("#usb-permissions pre code").first().textContent(),
  ).toBe(expected);
  const response = await page.request.get("70-dedibridge.rules");
  expect(response.ok()).toBe(true);
  expect(await response.text()).toBe(expected);
});

test("installer uses factual copy for the tested boards", async ({ page }) => {
  await openLocalBuild(page);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(
    "Install DediBridge firmware",
  );
  await expect(page.locator(".intro")).not.toContainText(
    /experimental|qualification|capable programmer|no debug probe/i,
  );
});

test("bundled release loads and links correctly at the project subpath", async ({
  page,
}) => {
  await mockRelease(page);
  await page.goto("./");
  await expect(page.locator("#release")).toContainText("v1.0");
  await expect(page.locator("#image-size")).toContainText("checksum matched");
  await expect(page.locator("#download")).toHaveAttribute(
    "href",
    "./firmware/dedi-rp2040.uf2",
  );
  await expect(page.locator("#install")).toBeDisabled();
});

test("a corrupt release image never becomes installable", async ({ page }) => {
  await mockRelease(page, true);
  await page.goto("./");
  await expect(page.getByRole("status")).toContainText(
    "checksum/size mismatch",
  );
  await expect(page.locator("#image-info")).toBeHidden();
  await expect(page.locator("#install")).toBeDisabled();
});

test("unsupported browsers retain download/manual guidance and disable USB", async ({
  page,
}) => {
  await page.addInitScript(() =>
    Object.defineProperty(navigator, "usb", { value: undefined }),
  );
  await openLocalBuild(page);
  await expect(page.locator("#unsupported")).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Connect bootloader" }),
  ).toBeDisabled();
  await expect(page.getByText("Prefer the manual route?")).toBeVisible();
});

test("UF2 validation and responsive layout work at the Pages project subpath", async ({
  page,
}) => {
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.setViewportSize({ width: 390, height: 844 });
  await openLocalBuild(page);
  await page.locator("#file").setInputFiles({
    name: "pico.uf2",
    mimeType: "application/octet-stream",
    buffer: Buffer.from(uf2()),
  });
  await expect(page.locator("#image-hash")).toContainText("SHA-256");
  await expect(page.locator("#install")).toBeDisabled();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  expect(errors).toEqual([]);
});

test("WCH install cannot erase before confirmation and verifies before reset", async ({
  page,
}) => {
  await mockWch(page);
  await openLocalBuild(page);
  await loadWchImage(page);
  await page.locator("#connect").click();
  await expect(page.locator("#device")).toContainText("CH32V307VCT6");
  expect(await page.evaluate(() => window.usbCommands.includes(0xa4))).toBe(
    false,
  );
  await expect(page.locator("#install")).toBeDisabled();
  await page.locator("#confirm").check();
  await page.locator("#install").click();
  await expect(page.getByRole("status")).toContainText(
    "Firmware written and verified.",
  );
  const commands = await page.evaluate(() => window.usbCommands);
  expect(commands.indexOf(0xa4)).toBeLessThan(commands.indexOf(0xa5));
  expect(commands.indexOf(0xa5)).toBeLessThan(commands.indexOf(0xa6));
  expect(commands.indexOf(0xa6)).toBeLessThan(commands.indexOf(0xa2));
  expect(commands).not.toContain(0xa8); // No option-byte writes.
  await expect(page.locator("#connect")).toBeEnabled();
});

test("wrong WCH variant is rejected without destructive commands", async ({
  page,
}) => {
  await mockWch(page, { chipId: 0x71 });
  await openLocalBuild(page);
  await loadWchImage(page);
  await page.locator("#connect").click();
  await expect(page.getByRole("status")).toContainText("requires CH32V307VCT6");
  const commands = await page.evaluate(() => window.usbCommands);
  expect(commands.every((command) => [0xa1, 0xa7].includes(command))).toBe(
    true,
  );
  await expect(page.locator("#install")).toBeDisabled();
});

test("Stop closes an in-flight write and never verifies or reboots afterward", async ({
  page,
}) => {
  await mockWch(page, { stallWrite: true });
  await openLocalBuild(page);
  await loadWchImage(page);
  await page.locator("#connect").click();
  await expect(page.locator("#device")).toContainText("CH32V307VCT6");
  await page.locator("#confirm").check();
  await page.locator("#install").click();
  await expect
    .poll(() => page.evaluate(() => window.usbCommands.includes(0xa5)))
    .toBe(true);
  await page.locator("#stop").click();
  await expect(page.locator("#connect")).toBeEnabled();
  const commands = await page.evaluate(() => window.usbCommands);
  expect(commands).not.toContain(0xa6);
  expect(commands).not.toContain(0xa2);
  await expect(page.locator("#install")).toBeDisabled();
});
