import { readFileSync, readdirSync, statSync } from "node:fs";
import { extname, join, relative } from "node:path";

const root = new URL("../src/", import.meta.url);
const allowed = new Set(["var(--font-body)", "var(--font-display)"]);
const failures = [];

function visit(directory) {
  for (const entry of readdirSync(directory)) {
    const path = join(directory, entry);
    if (statSync(path).isDirectory()) {
      visit(path);
      continue;
    }
    if (![".css", ".tsx", ".ts"].includes(extname(path))) continue;
    if (path.endsWith("fixtures.ts")) continue;
    const source = readFileSync(path, "utf8");
    for (const match of source.matchAll(/font-size\s*:\s*([^;}]+)/g)) {
      const value = match[1].trim();
      if (!allowed.has(value)) {
        failures.push(`${relative(root.pathname, path)}: ${value}`);
      }
    }
    for (const match of source.matchAll(/fontSize\s*:\s*["'`]([^"'`]+)["'`]/g)) {
      failures.push(`${relative(root.pathname, path)}: inline ${match[1]}`);
    }
  }
}

visit(root.pathname);
if (failures.length) {
  console.error("Only the 17px body and 21px display semantic sizes are allowed.");
  console.error(failures.join("\n"));
  process.exit(1);
}
console.log("Font-size policy passed: body 17px, display 21px.");
