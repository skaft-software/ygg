import { readFileSync, readdirSync, statSync } from "node:fs";
import { extname, join, relative } from "node:path";

const root = new URL("../", import.meta.url);
const ignored = new Set(["node_modules", "dist", "test-results"]);
const failures = [];

function visit(directory) {
  for (const entry of readdirSync(directory)) {
    if (ignored.has(entry)) continue;
    const path = join(directory, entry);
    if (statSync(path).isDirectory()) {
      visit(path);
      continue;
    }
    if (![".html", ".css", ".ts", ".tsx"].includes(extname(path))) continue;
    const source = readFileSync(path, "utf8");
    const unsafe = [
      ...source.matchAll(
        /(?:src|href)\s*=\s*["']https?:\/\/|@import\s+["']https?:\/\/|fetch\(\s*["']https?:\/\//g,
      ),
    ];
    if (unsafe.length) {
      failures.push(relative(root.pathname, path));
    }
  }
}

visit(root.pathname);
if (failures.length) {
  console.error(`External runtime references found:\n${failures.join("\n")}`);
  process.exit(1);
}
console.log("No external runtime assets or requests found.");
