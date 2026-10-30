// Catch missing English UI copy before shipping; runtime translation never walks user content.
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { Linter } from 'eslint';
import english from '../app/static/js/locale-en.js';

globalThis.location = { search: '?lang=en' };
const { tr } = await import('../app/static/js/i18n.js');
assert.equal(tr('Сохранить'), 'Save');
assert.equal(tr`Добавьте @${'<not-markup>'} администратором с правом публикации.`, 'Add @<not-markup> as an administrator with permission to post messages.');
assert.equal(tr('future_unknown_field'), 'future_unknown_field');
assert.equal(tr('constructor'), 'constructor');
assert.equal(tr('__proto__'), '__proto__');
assert.equal(tr`Строка ${12}, столбец ${3}`, 'Line 12, column 3');
assert.equal(tr`Строка ${'{1}'}, столбец ${3}`, 'Line {1}, column 3');

const missing = new Set();
const check = (text) => { if (/[а-яА-ЯёЁ]/.test(text) && !Object.hasOwn(english, text)) missing.add(text); };
const linter = new Linter();
for (const file of readdirSync('app/static/js', { recursive: true }).filter((f) => f.endsWith('.js') && !['locale-en.js', 'i18n.js'].includes(f))) {
  const source = readFileSync('app/static/js/' + file, 'utf8');
  const result = linter.verify(source, [{ languageOptions: { ecmaVersion: 'latest', sourceType: 'module' },
    plugins: { translations: { rules: { complete: { create() { return {
      CallExpression(node) { if (node.callee.name === 'tr' && node.arguments[0]?.type === 'Literal') check(node.arguments[0].value); },
      TaggedTemplateExpression(node) { if (node.tag.name === 'tr') check(node.quasi.quasis.map((q, i) => q.value.cooked + (i < node.quasi.expressions.length ? `{${i}}` : '')).join('')); },
    }; } } } } }, rules: { 'translations/complete': 'error' } }]);
  assert.equal(result.length, 0, file + ': ' + JSON.stringify(result));
}
const markup = readFileSync('app/static/index.html', 'utf8');
const decode = (s) => s.replaceAll('\r\n', '\n').replaceAll('&quot;', '"').replaceAll('&#x27;', "'").replaceAll('&lt;', '<').replaceAll('&gt;', '>').replaceAll('&amp;', '&');
for (const match of markup.matchAll(/data-i18n(?:-[\w-]+)?="([^"]*)"/g)) check(decode(match[1]));
// Snippets remain a classic script shared with other render consumers.
globalThis.window = {};
await import('../app/static/snippets.js');
for (const groups of Object.values(window.SNIPPETS)) for (const group of groups) {
  check(group.group);
  for (const item of group.items) for (const text of [item.label, item.title || '', item.snip]) check(text);
}
assert.deepEqual([...missing], [], 'Missing English translations');
for (const [source, text] of Object.entries(english)) {
  assert.deepEqual([...source.matchAll(/\{\d+\}/g)].map((m) => m[0]).sort(), [...text.matchAll(/\{\d+\}/g)].map((m) => m[0]).sort(), 'Translation changed placeholders: ' + source);
}
console.log(`PASS English catalog: ${Object.keys(english).length} strings, literal/template/static/snippet coverage and safe interpolation`);
