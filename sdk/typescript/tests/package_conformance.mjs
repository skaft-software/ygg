import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const packageRoot = resolve(new URL('..', import.meta.url).pathname);
const staging = mkdtempSync(join(tmpdir(), 'ygg-api-v03-pack-'));
const installRoot = mkdtempSync(join(tmpdir(), 'ygg-api-v03-install-'));

try {
  const packed = JSON.parse(
    execFileSync('npm', ['pack', '--json', '--pack-destination', staging], {
      cwd: packageRoot,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'inherit'],
    }),
  );
  assert.equal(packed.length, 1);
  const tarball = join(staging, packed[0].filename);
  writeFileSync(
    join(installRoot, 'package.json'),
    JSON.stringify({ name: 'offline-api-v03-consumer', private: true, type: 'module' }),
  );
  execFileSync('npm', ['install', '--offline', '--ignore-scripts', '--no-package-lock', tarball], {
    cwd: installRoot,
    stdio: 'inherit',
  });
  const output = execFileSync(
    process.execPath,
    [
      '--input-type=module',
      '--eval',
      "import { API_VERSION, hostOffer } from '@ygg/extension-api-v03'; console.log(API_VERSION, hostOffer(32, 1).limits.max_frame_bytes);",
    ],
    { cwd: installRoot, encoding: 'utf8' },
  ).trim();
  assert.equal(output, '0.3 32');
} finally {
  rmSync(staging, { recursive: true, force: true });
  rmSync(installRoot, { recursive: true, force: true });
}
