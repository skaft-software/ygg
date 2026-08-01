import { createHash } from "node:crypto";
import {
  lstat,
  mkdir,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const MAX_ASSET_BYTES = 32 * 1024 * 1024;
const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const webDirectory = resolve(scriptDirectory, "..");
const repositoryDirectory = resolve(webDirectory, "..", "..");
const distributionDirectory = join(webDirectory, "dist");
const embeddedDirectory = join(
  repositoryDirectory,
  "extensions",
  "ygg-serve",
  "web",
);
const assets = [
  {
    path: "assets/app.css",
    mediaType: "text/css; charset=utf-8",
  },
  {
    path: "assets/app.js",
    mediaType: "text/javascript; charset=utf-8",
  },
  {
    path: "assets/chunk-FilesPanel.js",
    mediaType: "text/javascript; charset=utf-8",
  },
  {
    path: "assets/chunk-file-languages.js",
    mediaType: "text/javascript; charset=utf-8",
  },
  {
    path: "assets/chunk-rolldown-runtime.js",
    mediaType: "text/javascript; charset=utf-8",
  },
  {
    path: "assets/chunk-MarkdownMessage.js",
    mediaType: "text/javascript; charset=utf-8",
  },
  {
    path: "index.html",
    mediaType: "text/html; charset=utf-8",
  },
];
const payloadPaths = assets.map(({ path }) => path);
const embeddedPaths = [...payloadPaths, "SHA256SUMS", "bundle.sha256"].sort();

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function portablePath(path) {
  return path.split(sep).join("/");
}

async function listFiles(root) {
  const found = [];

  async function visit(directory) {
    const entries = await readdir(directory, { withFileTypes: true });
    for (const entry of entries.sort((left, right) =>
      left.name < right.name ? -1 : left.name > right.name ? 1 : 0,
    )) {
      const absolute = join(directory, entry.name);
      const metadata = await lstat(absolute);
      if (metadata.isSymbolicLink()) {
        throw new Error(`bundle contains a symbolic link: ${absolute}`);
      }
      if (metadata.isDirectory()) {
        await visit(absolute);
      } else if (metadata.isFile()) {
        found.push(portablePath(relative(root, absolute)));
      } else {
        throw new Error(`bundle contains a non-regular entry: ${absolute}`);
      }
    }
  }

  await visit(root);
  return found.sort();
}

function assertExactPaths(actual, expected, label) {
  const actualText = actual.join("\n");
  const expectedText = expected.join("\n");
  if (actualText !== expectedText) {
    throw new Error(
      `${label} file set differs\nexpected:\n${expectedText}\nactual:\n${actualText}`,
    );
  }
}

async function readPayload(root) {
  const files = await listFiles(root);
  assertExactPaths(files, [...payloadPaths].sort(), "Vite distribution");

  const payload = new Map();
  for (const asset of assets) {
    const bytes = await readFile(join(root, asset.path));
    if (bytes.length === 0 || bytes.length > MAX_ASSET_BYTES) {
      throw new Error(
        `${asset.path} must contain 1-${MAX_ASSET_BYTES} bytes; found ${bytes.length}`,
      );
    }
    payload.set(asset.path, bytes);
  }

  const index = payload.get("index.html").toString("utf8");
  if (!index.includes('src="/assets/app.js"')) {
    throw new Error("index.html does not reference the fixed app.js asset");
  }
  if (!index.includes('href="/assets/app.css"')) {
    throw new Error("index.html does not reference the fixed app.css asset");
  }
  if (
    /(?:src|href)=["'](?:https?:)?\/\//i.test(index) ||
    index.includes("/src/")
  ) {
    throw new Error("index.html contains a source or external asset reference");
  }
  if (payload.get("assets/app.js").includes("sourceMappingURL")) {
    throw new Error("app.js contains a source-map reference");
  }

  return payload;
}

function manifests(payload) {
  const sums = assets
    .map(({ path }) => `${sha256(payload.get(path))}  ${path}\n`)
    .join("");
  return {
    sums,
    bundleHash: sha256(Buffer.from(sums, "utf8")),
  };
}

async function writeEmbedded(payload, sums, bundleHash) {
  const expected = resolve(
    repositoryDirectory,
    "extensions",
    "ygg-serve",
    "web",
  );
  if (resolve(embeddedDirectory) !== expected) {
    throw new Error("refusing to replace an unexpected embedded directory");
  }

  await rm(embeddedDirectory, { force: true, recursive: true });
  await mkdir(join(embeddedDirectory, "assets"), { recursive: true });
  for (const { path } of assets) {
    await writeFile(join(embeddedDirectory, path), payload.get(path));
  }
  await writeFile(join(embeddedDirectory, "SHA256SUMS"), sums, "utf8");
  await writeFile(join(embeddedDirectory, "bundle.sha256"), bundleHash, "utf8");
}

async function checkEmbedded(payload, sums, bundleHash) {
  const files = await listFiles(embeddedDirectory);
  assertExactPaths(files, embeddedPaths, "embedded bundle");

  for (const { path } of assets) {
    const embedded = await readFile(join(embeddedDirectory, path));
    if (!embedded.equals(payload.get(path))) {
      throw new Error(`${path} differs from the tested Vite output`);
    }
  }

  const embeddedSums = await readFile(
    join(embeddedDirectory, "SHA256SUMS"),
    "utf8",
  );
  const embeddedBundleHash = await readFile(
    join(embeddedDirectory, "bundle.sha256"),
    "utf8",
  );
  if (embeddedSums !== sums) {
    throw new Error("embedded SHA256SUMS is stale");
  }
  if (embeddedBundleHash !== bundleHash) {
    throw new Error("embedded bundle.sha256 is stale");
  }
}

const mode = process.argv[2];
if (mode !== "--write" && mode !== "--check") {
  throw new Error("usage: sync-embedded.mjs --write|--check");
}

const payload = await readPayload(distributionDirectory);
const { sums, bundleHash } = manifests(payload);

if (mode === "--write") {
  await writeEmbedded(payload, sums, bundleHash);
  console.log(`embedded web bundle synchronized (${bundleHash})`);
} else {
  await checkEmbedded(payload, sums, bundleHash);
  console.log(`embedded web bundle matches Vite output (${bundleHash})`);
}
