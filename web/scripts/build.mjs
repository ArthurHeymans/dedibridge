// SPDX-License-Identifier: GPL-2.0-only
import { build } from "esbuild";
import { cp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import metadata from "../package.json" with { type: "json" };

process.chdir(fileURLToPath(new URL("..", import.meta.url)));
await rm("dist", { recursive: true, force: true });
await mkdir("dist/licenses", { recursive: true });
await build({
  entryPoints: ["src/app.ts"],
  bundle: true,
  format: "esm",
  platform: "browser",
  outfile: "dist/app.js",
  target: "es2022",
  sourcemap: true,
  minify: true,
  legalComments: "linked",
});
await cp("style.css", "dist/style.css");
const rules = await readFile("../udev/70-dedibridge.rules", "utf8");
const html = await readFile("index.html", "utf8");
const marker = "<!-- udev-rules -->";
if (!html.includes(marker)) throw new Error("Missing udev help placeholder.");
const escapedRules = rules
  .replaceAll("&", "&amp;")
  .replaceAll("<", "&lt;")
  .replaceAll(">", "&gt;");
await writeFile("dist/index.html", html.replace(marker, escapedRules));
await writeFile("dist/70-dedibridge.rules", rules);
await cp("LICENSE", "dist/licenses/GPL-2.0.txt");
await cp("vendor/picoflash/LICENSE", "dist/licenses/picoflash-MIT.txt");
await writeFile("dist/.nojekyll", "");

// Ship corresponding source/build inputs at the same origin, not merely links
// to mutable dependencies or a minified third-party bundle.
await mkdir("dist/sources/udev", { recursive: true });
await writeFile("dist/sources/udev/70-dedibridge.rules", rules);
await mkdir("dist/sources/installer", { recursive: true });
for (const file of [
  "src",
  "scripts",
  "tests",
  "vendor",
  "package.json",
  "package-lock.json",
  "tsconfig.json",
  "playwright.config.mjs",
  "index.html",
  "style.css",
  "README.md",
  "LICENSE",
]) {
  await cp(file, `dist/sources/installer/${file}`, { recursive: true });
}
await mkdir("dist/sources/wchisp-web", { recursive: true });
for (const file of [
  "src",
  "package.json",
  "tsconfig.json",
  "rolldown.config.js",
  "README.md",
  "LICENSE",
]) {
  await cp(
    `node_modules/wchisp-web/${file}`,
    `dist/sources/wchisp-web/${file}`,
    { recursive: true },
  );
}
const { dependencies } = metadata;
await writeFile(
  "dist/sources/README.md",
  `# Corresponding source and licenses\n\nInstaller: GPL-2.0-only, source/build inputs in installer/.\nWCH ISP: GPL-2.0-only, source/build inputs in wchisp-web/.\nPinned dependency: ${dependencies["wchisp-web"]}\nPICOBOOT: MIT, unmodified source and attribution in installer/vendor/.\n`,
);
