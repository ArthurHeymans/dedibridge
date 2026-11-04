# DediBridge browser installer

Static, framework-free TypeScript + WebUSB. Only the original Pico/Pico H and
CH32V307VCT6 are enabled. No server, CDN script, runtime token or WASM is needed.
The browser application is **GPL-2.0-only** (see `LICENSE`), separately from the
MIT/Apache-licensed DediBridge firmware. PICOBOOT is vendored under MIT; WCH ISP
uses a commit-pinned, fixed `ArthurHeymans/wchisp-web` fork. Builds ship source,
build inputs and third-party licenses under `dist/sources/` and `dist/licenses/`.

## Develop and test

Node 24+ is required, including for the firmware bundling script's native
TypeScript import. From this directory:

```sh
npm ci
npm run format:check
npm test
npm run build
npx playwright install --with-deps chromium
npm run test:browser
```

Browser tests use mocked USB, a test-only static server and the Pages project
subpath; they do not touch attached devices. On NixOS, the downloaded browser
also needs a compatible runtime library environment (or a patched browser).

Serve `dist/` over HTTPS or localhost with a static HTTP server. Opening an HTML
file directly is not a supported development mode. A build without a firmware
manifest supports local image selection; it never silently fetches binaries
from third-party origins.

From the repository root, after `cargo xtask artifacts`, create `artifacts/SHA256SUMS`
as CI does, then bundle the exact images with the application:

```sh
cd web
npm run firmware -- --input ../artifacts --output dist/firmware \
  --tag v0.1.0 --commit FULL_40_CHARACTER_SOURCE_COMMIT
```

The script verifies all four release files against SHA256SUMS and structurally
validates UF2/BIN before writing a manifest. `npm run build` cleans `dist/`, so
bundle firmware **after** building. The site uses same-origin relative URLs,
including under GitHub Pages' `/dedibridge/` project path. Browser downloads are
size-limited and checked against manifest hashes before any erase operation.
Checksums are not independent authentication of a compromised release/site.

## Safety and limitations

- Explicit board/image/erase confirmation is required. USB calls start directly
  from a click; there is no automatic device selection or runtime reboot trick.
- Only one USB operation can run at a time. Failed/interrupted installs close
  the session and require reconnecting in bootloader mode. Stop cannot undo an
  erase; never promise the previous firmware survives an interruption.
- UF2 must be RP2040-family, complete, contiguous from flash offset zero, with
  non-overlapping 256-byte blocks entirely within the original Pico's 2 MiB.
- Raw CH32 BIN cannot prove its target identity. Local image selection is
  explicitly unauthenticated. The connected chip must identify as VCT6;
  protected flash and the incompatible 32 KiB SRAM partition are rejected.
  BIN is capped at 192 KiB to fit every supported flash partition. No option
  bytes or protection settings are changed.
- WCH ISP VERIFY and Pico byte-for-byte flash readback are mandatory. Reboot
  failures are distinguished from verification failures; actual runtime
  enumeration is not inferred from a disconnect or reset command.
- Desktop Chrome/Edge first. Windows may need WinUSB on the vendor bootloader
  interface; Linux needs the appropriate bootloader udev permissions. Manual
  UF2 copy/native wchisp instructions remain visible in unsupported browsers.
  The USB-permission panel opens on access-denied errors and displays/downloads
  the canonical `../udev/70-dedibridge.rules`, with reload/replug and NixOS steps.
  The build also ships that file under `sources/udev/` so source builds retain
  the same rule input.

USB installation and programmer operation have been tested end to end on
Pico and CH32V307VCT6. Mock tests additionally cover invalid images, failed
transfers and cancellation; see the [validation notes](../docs/web-flasher.md).

## Deployment

CI checks the web app on pushes and pull requests. Successful default-branch
and tag builds deploy the matching app/images through GitHub Pages Actions.
Tag builds also publish GitHub Release assets. PRs and other branches never
deploy. The live installer is https://arthurheymans.github.io/dedibridge/;
the repository homepage points there as well.

Pages must use the **GitHub Actions** source. Its deployment environment must
allow the default branch and, for release deployments, tags. Deployment is
serialized; the displayed branch/tag and source commit identify the exact
bundled firmware.
