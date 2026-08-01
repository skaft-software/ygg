import { readFile, readdir } from "node:fs/promises";
import { dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const distributionDirectory = resolve(scriptDirectory, "..", "dist");
const textExtensions = new Set([".html", ".js"]);

// These strings cross all three fixture-only surfaces: catalog/session data,
// transport behavior, and inline preview markup. A production build containing
// any one of them is not safe to embed or ship.
const fixtureSentinels = [
  "fixture",
  "Refine onboarding preview",
  "Unknown fixture session",
  "Demo data · responses and actions are simulated",
  "ygg release pulse",
];

async function listTextAssets(directory) {
  const assets = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) {
      assets.push(...(await listTextAssets(path)));
    } else if (entry.isFile() && textExtensions.has(extname(entry.name))) {
      assets.push(path);
    }
  }
  return assets;
}

const assets = await listTextAssets(distributionDirectory);
if (assets.length === 0) {
  throw new Error(`production distribution is empty: ${distributionDirectory}`);
}

for (const path of assets) {
  const content = await readFile(path, "utf8");
  for (const sentinel of fixtureSentinels) {
    if (content.toLocaleLowerCase().includes(sentinel.toLocaleLowerCase())) {
      throw new Error(
        `production asset ${path} contains fixture-only content: ${JSON.stringify(sentinel)}`,
      );
    }
  }
}

console.log(
  `production fixture boundary verified (${assets.length} text assets)`,
);
