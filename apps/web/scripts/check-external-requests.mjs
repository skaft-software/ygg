import { readFileSync, readdirSync, statSync } from "node:fs";
import { extname, join, relative } from "node:path";

const root = new URL("../", import.meta.url);
const failures = [];

function location(source, offset) {
  return source.slice(0, offset).split("\n").length;
}

function fail(scope, path, source, offset, message) {
  failures.push(
    `${scope}/${relative(path.scopeRoot, path.file)}:${location(source, offset)}: ${message}`,
  );
}

function isAuditedRootRelativeFetch(source, argument) {
  const expression = argument.match(
    /^([A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)*)/,
  )?.[1];
  if (expression !== "encoded.endpoint") return false;

  const initializer = source.match(
    /encoded\s*=\s*\{\s*endpoint:\s*([\s\S]*?),\s*\n\s*body:\s*JSON\.stringify/,
  )?.[1];
  if (!initializer) return false;

  return /^command\.type\s*===\s*["']session\.create["']\s*\?\s*["']\/[^"'`]+["']\s*:\s*["']\/[^"'`]+["']$/.test(
    initializer.trim(),
  );
}

function assertCsp(scope, file, source) {
  const meta = source.match(
    /http-equiv=(["'])Content-Security-Policy\1[^>]*content=(["'])(.*?)\2/i,
  );
  if (!meta) {
    failures.push(`${scope}/${relative(root.pathname, file)}: missing CSP`);
    return;
  }

  const directives = new Map(
    meta[3]
      .split(";")
      .map((directive) => directive.trim())
      .filter(Boolean)
      .map((directive) => {
        const [name, ...values] = directive.split(/\s+/);
        return [name, values];
      }),
  );

  const exact = new Map([
    ["connect-src", ["'self'"]],
    ["object-src", ["'none'"]],
    ["base-uri", ["'none'"]],
    ["form-action", ["'self'"]],
  ]);
  for (const [name, expected] of exact) {
    const actual = directives.get(name);
    if (
      !actual ||
      actual.length !== expected.length ||
      actual.some((value, index) => value !== expected[index])
    ) {
      failures.push(
        `${scope}/${relative(root.pathname, file)}: ${name} must be exactly ${expected.join(" ")}`,
      );
    }
  }

  const defaultSrc = directives.get("default-src") ?? [];
  if (!defaultSrc.includes("'self'") || defaultSrc.includes("*")) {
    failures.push(
      `${scope}/${relative(root.pathname, file)}: default-src must be self-only by default`,
    );
  }
  const scriptSrc = directives.get("script-src") ?? [];
  if (
    !scriptSrc.includes("'self'") ||
    scriptSrc.some((value) => value === "'unsafe-inline'" || value === "'unsafe-eval'")
  ) {
    failures.push(
      `${scope}/${relative(root.pathname, file)}: script-src must be self without unsafe execution`,
    );
  }
}

function inspectRuntimeFile(scope, scopeRoot, file) {
  const source = readFileSync(file, "utf8");
  const path = { scopeRoot, file };

  if (extname(file) === ".html" && file.endsWith("index.html")) {
    assertCsp(scope, file, source);
  }

  const bannedPatterns = [
    [/(?:src|href|action|formAction)\s*=\s*["'`]\s*\/\//gi, "scheme-relative resource"],
    [/@import\s+(?:url\()?["']?\s*\/\//gi, "scheme-relative CSS import"],
    [/url\(\s*["']?\s*\/\//gi, "scheme-relative CSS URL"],
    [/\bnew\s+XMLHttpRequest\s*\(/g, "XMLHttpRequest is not an audited transport"],
    [/\bwindow\.open\s*\(/g, "window.open is not an audited navigation"],
    [/\bnavigator\.sendBeacon\s*\(/g, "sendBeacon is not an audited transport"],
    [/\bnew\s+EventSource\s*\(/g, "EventSource is not an audited transport"],
  ];
  for (const [pattern, message] of bannedPatterns) {
    for (const match of source.matchAll(pattern)) {
      fail(scope, path, source, match.index, message);
    }
  }

  const builtAbsoluteAllowlist = [
    /^http:\/\/www\.w3\.org\//,
    /^https:\/\/react\.dev\/errors\//,
  ];
  for (const match of source.matchAll(/\b(?:https?|wss?):\/\/[^\s"'`)]+/gi)) {
    if (
      scope === "built" &&
      builtAbsoluteAllowlist.some((allowed) => allowed.test(match[0]))
    ) {
      continue;
    }
    fail(scope, path, source, match.index, `absolute network URL ${match[0]}`);
  }

  if (scope !== "built") {
    for (const match of source.matchAll(/\bfetch\s*\(\s*/g)) {
      const argument = source.slice(match.index + match[0].length);
      if (
        !/^(?:["'`]\/)/.test(argument) &&
        !isAuditedRootRelativeFetch(source, argument)
      ) {
        fail(
          scope,
          path,
          source,
          match.index,
          "fetch URL must be a literal root-relative path",
        );
      }
    }
  }

  for (const match of source.matchAll(/\bnew\s+WebSocket\s*\(\s*/g)) {
    const argument = source.slice(match.index + match[0].length);
    const derivesFromPage =
      scope === "built"
        ? argument.slice(0, 200).includes("window.location.host")
        : argument.startsWith("`${scheme}//${window.location.host}/");
    if (!derivesFromPage) {
      fail(
        scope,
        path,
        source,
        match.index,
        "WebSocket URL must derive from window.location.host",
      );
    }
  }
}

function visit(scope, directory, scopeRoot) {
  for (const entry of readdirSync(directory)) {
    if (entry.includes(".test.")) continue;
    const path = join(directory, entry);
    if (statSync(path).isDirectory()) {
      visit(scope, path, scopeRoot);
      continue;
    }
    if (![".html", ".css", ".js", ".ts", ".tsx"].includes(extname(path))) continue;
    inspectRuntimeFile(scope, scopeRoot, path);
  }
}

const sourceIndex = new URL("../index.html", import.meta.url).pathname;
inspectRuntimeFile("source", root.pathname, sourceIndex);
visit("source", new URL("../src/", import.meta.url).pathname, root.pathname);

const dist = new URL("../dist/", import.meta.url).pathname;
if (statSync(new URL("../", import.meta.url).pathname).isDirectory()) {
  try {
    if (statSync(dist).isDirectory()) visit("built", dist, dist);
  } catch {
    // A source-only check is valid before the first production build.
  }
}

if (failures.length) {
  console.error(`Runtime network/CSP policy failed:\n${failures.join("\n")}`);
  process.exit(1);
}
console.log(
  "Runtime network/CSP policy passed: audited calls and built assets are same-origin only.",
);
