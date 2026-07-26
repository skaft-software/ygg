import { readFileSync, readdirSync, statSync } from "node:fs";
import { extname, join, relative } from "node:path";

const root = new URL("../src/", import.meta.url);
const expectedTokens = new Map([
  ["--font-body", "13px"],
  ["--font-meta", "12px"],
  ["--font-chat", "15px"],
  ["--font-prompt", "15px"],
  ["--font-display", "20px"],
]);
const allowedUses = new Set(
  [...expectedTokens.keys()].map((token) => `var(${token})`),
);
const failures = [];
const declarations = new Map();

function location(source, offset) {
  return source.slice(0, offset).split("\n").length;
}

function fail(path, source, offset, message) {
  failures.push(`${relative(root.pathname, path)}:${location(source, offset)}: ${message}`);
}

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

    for (const match of source.matchAll(/(--[a-z0-9_-]+)\s*:\s*([^;{}]+)/gi)) {
      const [, name, rawValue] = match;
      const value = rawValue.trim();
      const looksLikeTypeSize =
        name.startsWith("--font-") ||
        (/(?:font|text|type|typography|size)/i.test(name) &&
          /(?:^|[\s,(])(?:\d*\.?\d+)(?:px|rem|em|ch|ex|cap|ic|lh|vw|vh|vmin|vmax)\b/i.test(
            value,
          ));
      if (!looksLikeTypeSize) continue;

      if (!expectedTokens.has(name)) {
        fail(path, source, match.index, `unexpected font-size token ${name}: ${value}`);
        continue;
      }

      const seen = declarations.get(name) ?? [];
      seen.push({ path, source, offset: match.index, value });
      declarations.set(name, seen);
    }

    for (const match of source.matchAll(/font-size\s*:\s*([^;}]+)/g)) {
      const value = match[1].trim();
      if (!allowedUses.has(value)) {
        fail(path, source, match.index, `arbitrary font-size ${value}`);
      }
    }

    for (const match of source.matchAll(/(^|[;{]\s*)font\s*:\s*([^;}]+)/gm)) {
      const value = match[2].trim();
      if (value !== "inherit") {
        fail(path, source, match.index, `font shorthand hides a size: ${value}`);
      }
    }

    for (const match of source.matchAll(/\bfontSize\s*:/g)) {
      fail(path, source, match.index, "inline fontSize is not allowed");
    }

    for (const match of source.matchAll(/\bfont\s*:\s*(?!inherit\b)/g)) {
      if (extname(path) !== ".css") {
        fail(path, source, match.index, "inline font shorthand is not allowed");
      }
    }
  }
}

visit(root.pathname);

for (const [name, expected] of expectedTokens) {
  const seen = declarations.get(name) ?? [];
  if (seen.length !== 1) {
    failures.push(
      `${name}: expected exactly one declaration with ${expected}, found ${seen.length}`,
    );
    continue;
  }
  if (seen[0].value !== expected) {
    fail(
      seen[0].path,
      seen[0].source,
      seen[0].offset,
      `${name} must be ${expected}, found ${seen[0].value}`,
    );
  }
}

if (failures.length) {
  console.error(
    "Typography policy failed: only the measured Cowork typography tokens are allowed.",
  );
  console.error(failures.join("\n"));
  process.exit(1);
}
console.log(
  "Typography policy passed: measured Cowork UI, metadata, chat, prompt, and title tokens.",
);
