// Run only when updating the pinned WidgetSandbox runtime; Docker and normal development need no Node build.
// Checkout (tag v0.6.1) и зависимости кэшируются в node_modules/.cache/widget-sandbox;
// выход — app/static/vendor/widget (коммитится в репозиторий). --check сверяет выход.
import { build } from 'esbuild';
import { execFileSync } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
const TAG = 'v0.6.1';
const REPO = 'https://github.com/TelegramMessenger/widget-sandbox';
const cache = path.join(root, 'node_modules', '.cache', 'widget-sandbox');
const checkout = path.join(cache, TAG);
const output = path.join(root, 'app', 'static', 'vendor', 'widget');
const check = process.argv.includes('--check');

const run = (command, args, options = {}) =>
  execFileSync(command, args, { stdio: 'inherit', shell: process.platform === 'win32', ...options });

function ensureCheckout() {
  const marker = path.join(checkout, 'packages', 'core', 'src', 'host', 'widget-host.ts');
  if (existsSync(marker)) return;
  rmSync(checkout, { recursive: true, force: true });
  mkdirSync(cache, { recursive: true });
  run('git', ['clone', '--depth', '1', '--branch', TAG, REPO, checkout]);
  run('pnpm', ['install', '--frozen-lockfile'], { cwd: checkout });
}

/** Компилирует TS-сборщик в кэш вместе с их sandbox-build.ts: node любого поколения исполняет обычный JS. */
async function compileBuilder() {
  const bundled = path.join(cache, 'widget-vendor-build.mjs');
  await build({
    entryPoints: [path.join(root, 'scripts', 'widget-vendor-build.ts')],
    bundle: true,
    format: 'esm',
    platform: 'node',
    target: 'node22',
    external: ['esbuild'],
    alias: {
      'widget-sandbox-build': path.join(checkout, 'packages', 'core', 'scripts', 'sandbox-build.ts'),
    },
    outfile: bundled,
    logLevel: 'warning',
  });
  return bundled;
}

async function buildInto(destination) {
  ensureCheckout();
  const bundled = await compileBuilder();
  const { buildAll } = await import(new URL('file:///' + bundled.replaceAll('\\', '/')).href);
  return buildAll(checkout, destination, root);
}

/** Относительные пути всех файлов каталога. */
const tree = (dir) =>
  readdirSync(dir, { recursive: true, withFileTypes: true })
    .filter((entry) => entry.isFile())
    .map((entry) => path.relative(dir, path.join(entry.parentPath ?? entry.path, entry.name)).replaceAll('\\', '/'))
    .sort();

if (!check) {
  const files = await buildInto(output);
  console.log(`WidgetSandbox ${TAG}: ${files.length} файлов в app/static/vendor/widget`);
} else {
  const temp = mkdtempSync(path.join(tmpdir(), 'widget-vendor-'));
  try {
    await buildInto(temp);
    const fresh = tree(temp);
    const committed = tree(output);
    let stale = false;
    for (const relative of fresh) {
      const twin = path.join(output, relative);
      if (!committed.includes(relative) || !existsSync(twin)) {
        console.error(`нет в репозитории: ${relative}`);
        stale = true;
      } else if (!readFileSync(path.join(temp, relative)).equals(readFileSync(twin))) {
        console.error(`отличается: ${relative}`);
        stale = true;
      }
    }
    for (const relative of committed) {
      if (!fresh.includes(relative)) {
        console.error(`лишний в репозитории: ${relative}`);
        stale = true;
      }
    }
    if (stale) {
      console.error('Бандл WidgetSandbox устарел: npm run vendor:widget');
      process.exitCode = 1;
    } else {
      console.log(`WidgetSandbox ${TAG}: бандл соответствует checkout`);
    }
  } finally {
    rmSync(temp, { recursive: true, force: true });
  }
}
