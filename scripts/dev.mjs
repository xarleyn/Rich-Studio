import { existsSync } from 'node:fs';
import { createServer } from 'node:net';
import { spawn } from 'node:child_process';
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
const python = process.platform === 'win32'
  ? (existsSync(path.join(root, '.venv', 'Scripts', 'python.exe')) ? path.join(root, '.venv', 'Scripts', 'python.exe') : 'python')
  : (existsSync(path.join(root, '.venv', 'bin', 'python')) ? path.join(root, '.venv', 'bin', 'python') : 'python3');

const reserve = createServer();
await new Promise((resolve, reject) => reserve.once('error', reject).listen(0, '127.0.0.1', resolve));
const mockPort = reserve.address().port;
await new Promise((resolve) => reserve.close(resolve));

const children = new Set();
let stopping = false;
const stop = (code = 0) => {
  if (stopping) return;
  stopping = true;
  children.forEach((child) => child.kill());
  setTimeout(() => process.exit(code), 500);
};
const start = (cwd, env, commandArgs) => {
  const child = spawn(python, ['-m', 'uvicorn', ...commandArgs], { cwd, env: { ...process.env, ...env }, stdio: 'inherit' });
  children.add(child);
  child.on('error', () => stop(1));
  child.on('exit', (code) => { if (!stopping) stop(code || 1); });
  return child;
};

start(path.join(root, 'tests'), {}, ['mock_telegram:app', '--host', '127.0.0.1', '--port', String(mockPort)]);
start(path.join(root, 'app'), {
  DATA_DIR: process.env.DATA_DIR || path.join(root, 'work', 'dev-data'),
  TG_BOT_TOKEN: '1:x',
  TG_API_BASE: `http://127.0.0.1:${mockPort}`,
  TG_DEFAULT_CHAT: '@test',
  TG_PREVIEW_CHAT: '777',
  SPEC_URL: 'http://127.0.0.1:9/off',
  STUDIO_URL: `http://localhost:${port}`,
  STUDIO_BACKGROUND: 'off',
}, ['main:app', '--host', host, '--port', String(port)]);

process.on('SIGINT', () => stop(0));
process.on('SIGTERM', () => stop(0));
