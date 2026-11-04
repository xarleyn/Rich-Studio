// Dev launcher: a Rust studio and the C# fake Telegram on ports of their own.
// Usage: npm run dev [-- --host 127.0.0.1 --port 7424]
import { spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const option = (name, fallback) => {
  const index = args.indexOf(name);
  return index >= 0 && args[index + 1] ? args[index + 1] : fallback;
};
const host = option('--host', '127.0.0.1');
const port = Number(option('--port', '7424'));
const work = mkdtempSync(path.join(tmpdir(), 'rich-studio-dev-'));
const children = new Set();
let stopping = false;

const stop = (code = 0) => {
  if (stopping) return;
  stopping = true;
  for (const child of children) child.kill();
  setTimeout(() => {
    rmSync(work, { recursive: true, force: true });
    process.exit(code);
  }, 500);
};

const run = (command, commandArgs, env = {}) => {
  const child = spawn(command, commandArgs, {
    cwd: root,
    env: { ...process.env, ...env },
    stdio: 'inherit',
    windowsHide: true,
  });
  children.add(child);
  child.on('error', () => stop(1));
  child.on('exit', (code) => { if (!stopping) stop(code || 1); });
  return child;
};

const build = (command, commandArgs) => new Promise((resolve, reject) => {
  const child = spawn(command, commandArgs, { cwd: root, stdio: 'inherit', windowsHide: true });
  child.on('error', reject);
  child.on('exit', (code) => code === 0 ? resolve() : reject(new Error(`${command} exited with ${code}`)));
});

const freePort = async () => {
  const listener = createServer();
  await new Promise((resolve, reject) => listener.once('error', reject).listen(0, '127.0.0.1', resolve));
  const value = listener.address().port;
  await new Promise((resolve) => listener.close(resolve));
  return value;
};

const ready = async (url) => {
  for (let attempt = 0; attempt < 120; attempt++) {
    try {
      if ((await fetch(url, { signal: AbortSignal.timeout(1000) })).ok) return;
    } catch { /* starting */ }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`${url} did not become ready`);
};

process.on('SIGINT', () => stop(0));
process.on('SIGTERM', () => stop(0));

try {
  const mockPort = await freePort();
  const mockOrigin = `http://127.0.0.1:${mockPort}`;
  const mockBin = path.join(work, 'mock-bin');
  await build('dotnet', ['build', 'tests/mock_telegram.cs', '--nologo', '-o', mockBin]);
  await build('cargo', ['build', '--locked', '--bin', 'rich-studio']);
  run('dotnet', [path.join(mockBin, 'mock_telegram.dll'), '--urls', mockOrigin]);
  const binary = path.join(root, 'target', 'debug', process.platform === 'win32' ? 'rich-studio.exe' : 'rich-studio');
  run(binary, ['serve'], {
    DATA_DIR: path.join(work, 'data'),
    STUDIO_BIND: `${host}:${port}`,
    STUDIO_MODE: 'local',
    STUDIO_STATIC_DIR: path.join(root, 'app', 'static'),
    TG_BOT_TOKEN: '1:x',
    TG_API_BASE: mockOrigin,
    TG_DEFAULT_CHAT: '@test',
    TG_PREVIEW_CHAT: '777',
    SPEC_URL: 'http://127.0.0.1:9/off',
  });
  await ready(`${mockOrigin}/log`);
  await ready(`http://${host}:${port}/readyz`);
  console.log(`Studio: http://${host}:${port}; mock Telegram: ${mockOrigin}; data: ${work}`);
} catch (error) {
  console.error(error.message);
  stop(1);
}
