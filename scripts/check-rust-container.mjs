// Installed-image acceptance, with no external network, host ports, or application credentials.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { setTimeout } from 'node:timers/promises';

const image = process.env.IMAGE || 'rich-studio-rust-candidate';
const name = 'rs-rust-smoke-' + randomUUID();
const docker = (...args) => execFileSync('docker', args, { encoding: 'utf8', windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'], timeout: 60000 }).trim();
let started = false;
async function ready() {
  for (let attempt = 0; attempt < 40; attempt++) {
    try { docker('exec', name, 'rich-studio', 'healthcheck'); return; } catch { /* startup */ }
    if (docker('inspect', '-f', '{{.State.Running}}', name) !== 'true') throw new Error('container exited during startup');
    await setTimeout(250);
  }
  throw new Error('container readiness timeout');
}
try {
  docker('run', '-d', '--name', name, '--network', 'none', '--read-only', '--tmpfs', '/tmp', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges', '-e', 'TG_UPDATES=off', '-e', 'SPEC_PIN=on', image);
  started = true;
  await ready();
  assert.equal(docker('exec', name, 'id', '-u'), '10001');
  docker('exec', name, 'sh', '-c', '! command -v python && ! command -v python3 && ! command -v pip && ! command -v uvicorn && test ! -d /app && test -r /opt/rich-studio/static/index.html');
  const config = JSON.parse(docker('exec', name, 'rich-studio', 'request', 'GET', '/api/config'));
  assert.equal(config.public, false);
  const token = docker('exec', name, 'rich-studio', 'token', 'add', 'container-check', '--level', 'admin');
  assert.match(token, /^rs_[A-Za-z0-9_-]{40,}$/u);
  const request = (method, path, body) => JSON.parse(docker('exec', '-e', `STUDIO_TOKEN=${token}`, name, 'rich-studio', 'request', method, path, ...(body ? [JSON.stringify(body)] : [])));
  const draft = request('POST', '/api/drafts', { title: 'Rust image persistence fixture', post: { rich_message: { blocks: [] } } });
  assert.equal(typeof draft.id, 'string');
  docker('stop', '--time', '20', name);
  assert.equal(docker('inspect', '-f', '{{.State.ExitCode}}', name), '0', 'SIGTERM must finish gracefully');
  docker('start', name);
  await ready();
  const restored = request('GET', `/api/drafts/${draft.id}`);
  assert.equal(restored.title, 'Rust image persistence fixture');
  docker('stop', '--time', '20', name);
  assert.equal(docker('inspect', '-f', '{{.State.ExitCode}}', name), '0');
  console.log(`PASS ${image}: uid 10001, no Python, read-only root, native readiness/CLI, token+draft persistence, graceful SIGTERM.`);
} catch (error) {
  console.error(error.message);
  if (started) console.error(docker('logs', '--tail', '40', name));
  process.exitCode = 1;
} finally {
  if (started) docker('rm', '-v', '-f', name);
}
