// The Rust executable is the source of route metadata and the official MCP tool catalog.
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const executable = process.env.RICH_STUDIO_BINARY || join(root, 'target', 'debug', process.platform === 'win32' ? 'rich-studio.exe' : 'rich-studio');
const environment = {};
for (const [key, value] of Object.entries(process.env)) {
  if (['PATH', 'PATHEXT', 'SYSTEMROOT', 'WINDIR', 'COMSPEC', 'TEMP', 'TMP', 'HOME', 'USERPROFILE'].includes(key.toUpperCase())) environment[key] = value;
}
const load = (command) => execFileSync(executable, [command], { cwd: root, env: environment, encoding: 'utf8', windowsHide: true, maxBuffer: 8 * 1024 * 1024 }).replaceAll('\r\n', '\n');
const schemaText = load('openapi');
const schema = JSON.parse(schemaText);
const tools = JSON.parse(load('mcp-contract'));
const header = '<!-- generated: run `make docs`, do not edit by hand -->';
const tags = { posts: 'Проверка поста', preview: 'Предпросмотр', drafts: 'Черновики', telegram: 'Публикация и канал', channels: 'Каналы', media: 'Файлы', spec: 'Спецификация Bot API', service: 'Студия' };
const cell = (text) => text.trim().replace(/\s+/gu, ' ').replaceAll('|', '\\|');
const rows = new Map();
for (const [path, operations] of Object.entries(schema.paths)) {
  for (const [method, operation] of Object.entries(operations)) {
    const text = (operation.summary || '') + (operation.description ? '. ' + operation.description : '');
    const tag = operation.tags?.[0] || 'other';
    if (!rows.has(tag)) rows.set(tag, []);
    rows.get(tag).push(`| \`${method.toUpperCase()} ${path}\` | ${cell(text)} |`);
  }
}
const http = [header, '', '# HTTP API', '', 'Все маршруты студии. Интерактивно с примерами: `/docs` работающей студии.'];
const tagIndex = (tag) => Object.hasOwn(tags, tag) ? Object.keys(tags).indexOf(tag) : Object.keys(tags).length;
for (const tag of [...rows.keys()].sort((a, b) => tagIndex(a) - tagIndex(b))) http.push('', `## ${tags[tag] || tag}`, '', '| Маршрут | Что делает |', '|---|---|', ...rows.get(tag).sort());
const mcp = [header, '', `# Инструменты MCP (${tools.length})`, '', 'Сервер `/mcp`. Инструментов вида «публикует или меняет канал» нет при `MCP_PUBLISH=off`.', '', '| Инструмент | Что делает | Вид |', '|---|---|---|'];
for (const tool of tools.sort((a, b) => a.name < b.name ? -1 : a.name > b.name ? 1 : 0)) {
  const annotation = tool.annotations || {};
  const kind = annotation.destructiveHint === true ? 'публикует или меняет канал' : annotation.openWorldHint === true ? (annotation.readOnlyHint ? 'читает Telegram' : 'ходит в Telegram или в сеть') : annotation.readOnlyHint ? 'только читает' : 'меняет данные студии';
  mcp.push(`| \`${tool.name}\` | ${cell(tool.description || '')} | ${kind} |`);
}
const files = new Map([
  // Serde already orders object keys; retain its 1.0 versus 1 spelling from the wire contract.
  ['docs/openapi.json', schemaText.replace(/^ +/gm, (indent) => ' '.repeat(indent.length / 2)).trimEnd() + '\n'],
  ['docs/reference/http-api.md', http.join('\n') + '\n'],
  ['docs/reference/mcp-tools.md', mcp.join('\n') + '\n'],
]);
let changed = false;
for (const [path, content] of files) {
  const old = readFileSync(join(root, path), 'utf8').replaceAll('\r\n', '\n');
  if (content === old) continue;
  changed = true;
  if (process.argv.includes('--check')) console.error(`${path} is stale: run make docs`);
  else { writeFileSync(join(root, path), content, 'utf8'); console.log(`Updated ${path}`); }
}
if (process.argv.includes('--check') && changed) process.exitCode = 1;
else console.log(`Rust documentation: ${Object.values(schema.paths).reduce((n, methods) => n + Object.keys(methods).length, 0)} HTTP operations, ${tools.length} MCP tools.`);
