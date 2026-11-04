// requirements.lock must pin every package from requirements.txt and keep a hash for each block.
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const read = (name) => readFileSync(resolve(root, name), 'utf8').replaceAll('\r\n', '\n');

function pins(name, text) {
  const found = new Map();
  for (const match of text.matchAll(/^([A-Za-z0-9._-]+)==([^\s;\\]+)/gm)) {
    found.set(match[1].toLowerCase().replaceAll('_', '-'), match[2]);
  }
  return found;
}

const requirements = pins('requirements.txt', read('requirements.txt'));
const locked = pins('requirements.lock', read('requirements.lock'));
const stale = [...requirements].filter(([name, version]) => locked.get(name) !== version);
if (stale.length) {
  console.error(`requirements.lock отстал от requirements.txt (${stale.map(([name, version]) => `${name}==${version}`).join(', ')}): make lock и закоммитьте его`);
  process.exitCode = 1;
}

const lock = read('requirements.lock');
const body = lock.split('\n').slice(2).join('\n');
const blocks = body.split(/\n(?=[A-Za-z0-9])/).filter((block) => block.trim());
const withoutHash = blocks.filter((block) => !block.includes('--hash=sha256:'));
if (withoutHash.length) {
  console.error(`в requirements.lock нет хэшей у ${withoutHash.length} блок(ов)`);
  process.exitCode = 1;
}
if (!process.exitCode) console.log(`requirements.lock: ${locked.size} пакетов, хэши на месте`);
