// Run the unchanged browser suite against an isolated Rust process and mock Telegram.
// Python is a development fixture only; no inherited studio credentials reach either process.
import { spawn } from 'node:child_process';
import { createWriteStream, existsSync } from 'node:fs';
import { copyFile, mkdtemp, readFile } from 'node:fs/promises';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { setTimeout } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const work = await mkdtemp(join(tmpdir(), 'rich-studio-browser-'));
const binary = process.env.RICH_STUDIO_BINARY || join(root, 'target', 'debug', process.platform === 'win32' ? 'rich-studio.exe' : 'rich-studio');
const venv = join(root, '.venv', process.platform === 'win32' ? 'Scripts/python.exe' : 'bin/python');
const python = process.env.RICH_STUDIO_PYTHON || (existsSync(venv) ? venv : 'python3');
const environment = {};
for (const [key, value] of Object.entries(process.env)) {
  if (['PATH', 'PATHEXT', 'SYSTEMROOT', 'WINDIR', 'COMSPEC', 'TEMP', 'TMP', 'HOME', 'USERPROFILE', 'LOCALAPPDATA'].includes(key.toUpperCase())) environment[key] = value;
}
const children = [];
const streams = [];

async function unusedPort() {
  const listener = createServer();
  await new Promise((resolve, reject) => { listener.once('error', reject); listener.listen(0, '127.0.0.1', resolve); });
  const port = listener.address().port;
  await new Promise((resolve, reject) => listener.close((error) => error ? reject(error) : resolve()));
  return port;
}
function launch(name, executable, args, env, inherit = false) {
  const output = inherit ? null : createWriteStream(join(work, `${name}.log`));
  if (output) streams.push(output);
  const child = spawn(executable, args, { cwd: root, env, windowsHide: true, stdio: inherit ? 'inherit' : ['ignore', 'pipe', 'pipe'] });
  const completed = new Promise((resolve, reject) => { child.once('error', reject); child.once('exit', (code, signal) => resolve({ code, signal })); });
  // Readiness reports startup failure with the preserved log; always handle rejection here.
  completed.catch(() => {});
  if (output) { child.stdout.pipe(output, { end: false }); child.stderr.pipe(output, { end: false }); }
  children.push({ child, completed, name });
  return { child, completed, name };
}
async function ready(process, url) {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (process.child.exitCode !== null || process.child.signalCode !== null) throw new Error(`${process.name} exited before readiness`);
    try { if ((await fetch(url, { signal: AbortSignal.timeout(1000) })).ok) return; } catch { /* starting */ }
    await setTimeout(100);
  }
  throw new Error(`${process.name} readiness timeout`);
}

let passed = false;
try {
  const mockPort = await unusedPort();
  const studioPort = await unusedPort();
  const mockOrigin = `http://127.0.0.1:${mockPort}`;
  const studioOrigin = `http://127.0.0.1:${studioPort}`;
  const mock = launch('mock', python, ['-m', 'uvicorn', 'mock_telegram:app', '--app-dir', 'tests', '--host', '127.0.0.1', '--port', String(mockPort)], { ...environment, PYTHONUTF8: '1' });
  await ready(mock, `${mockOrigin}/log`);
  // Windows cannot replace a running executable. Keep Cargo's output free for concurrent checks.
  const installed = join(work, process.platform === 'win32' ? 'rich-studio.exe' : 'rich-studio');
  await copyFile(binary, installed);
  const studio = launch('studio', installed, ['serve'], {
    ...environment, DATA_DIR: join(work, 'data'), STUDIO_BIND: `127.0.0.1:${studioPort}`,
    STUDIO_MODE: 'local', STUDIO_PUBLIC: 'off', STUDIO_STATIC_DIR: join(root, 'app', 'static'),
    STUDIO_BACKGROUND: 'on', TG_BOT_TOKEN: '1:x', TG_API_BASE: mockOrigin,
    TG_DEFAULT_CHAT: '@test', TG_PREVIEW_CHAT: '777', TG_UPDATES: 'on', TG_UPDATES_MODE: 'polling',
    SPEC_PIN: 'off', SPEC_URL: 'http://127.0.0.1:9/off', SHUTDOWN_GRACE: '10',
  });
  await ready(studio, `${studioOrigin}/readyz`);
  console.log(`Rust browser fixture: ${studioOrigin}; logs and isolated data: ${work}`);
  const suite = launch('browser', process.execPath, ['tests/e2e_ui.mjs'], {
    ...environment, BASE: studioOrigin, MOCK: mockOrigin, RICH_STUDIO_BINARY: installed,
    RICH_STUDIO_PYTHON: python, STUDIO_STATIC_DIR: join(root, 'app', 'static'),
    E2E_ONLY: process.env.E2E_ONLY || '',
  }, true);
  const result = await suite.completed;
  if (result.code !== 0) throw new Error(`browser suite exited with ${result.code ?? result.signal}`);
  const docsPort = await unusedPort();
  const docsOrigin = `http://127.0.0.1:${docsPort}`;
  const docsKey = 'isolated-api-docs-fixture-key';
  const documentation = launch('documentation', installed, ['serve'], {
    ...environment, DATA_DIR: join(work, 'documentation-data'), STUDIO_BIND: `127.0.0.1:${docsPort}`,
    STUDIO_MODE: 'local', STUDIO_API_KEY: docsKey, STUDIO_STATIC_DIR: join(root, 'app', 'static'),
    STUDIO_BACKGROUND: 'off', TG_UPDATES: 'off', SPEC_PIN: 'on',
  });
  await ready(documentation, `${docsOrigin}/readyz`);
  const docs = launch('documentation-browser', process.execPath, ['scripts/check-rust-api-docs.mjs'], {
    ...environment, BASE: docsOrigin, DOCS_FIXTURE_KEY: docsKey,
  }, true);
  const docsResult = await docs.completed;
  if (docsResult.code !== 0) throw new Error(`API documentation suite exited with ${docsResult.code ?? docsResult.signal}`);
  passed = true;
} catch (error) {
  console.error(error.message);
  for (const name of ['studio', 'mock', 'documentation']) {
    const log = await readFile(join(work, `${name}.log`), 'utf8').catch(() => '');
    if (log) console.error(`${name} log (tail):\n${log.slice(-8000)}`);
  }
} finally {
  for (const { child, completed } of children.reverse()) {
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');
    const result = await Promise.race([completed.catch(() => ({})), setTimeout(12000, null, { ref: false })]);
    if (result === null) { child.kill('SIGKILL'); await completed.catch(() => {}); }
  }
  for (const stream of streams) stream.end();
}
process.exitCode = passed ? 0 : 1;
