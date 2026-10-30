// Completion behavior against real CodeMirror syntax trees, including incomplete JSON and future spec fields.
import assert from 'node:assert/strict';
import * as cm from '../app/static/vendor/code-editor.js';
import { htmlTags, jsonSource, snippetSource } from '../app/static/js/source-completion.js';

const field = (name, types) => ({ name, types, description: name, required: false });
const spec = { types: {
  InputRichMessage: { fields: [field('blocks', ['Array of InputRichBlock']), field('markdown', ['String'])] },
  InputRichBlock: { subtypes: ['Paragraph', 'Heading'] },
  Paragraph: { const: 'paragraph', fields: [field('type', ['String']), field('text', ['RichText'])] },
  Heading: { const: 'heading', fields: [field('type', ['String']), field('size', ['Integer']), field('text', ['RichText'])] },
}, methods: { sendRichMessage: { fields: [field('chat_id', ['String']), field('disable_notification', ['Boolean'])] } } };
let passed = 0;
function check(name, fn) { fn(); passed++; console.log('PASS  ' + name); }
function complete(source, explicit = true) {
  const pos = source.indexOf('|');
  const doc = source.replace('|', '');
  const state = cm.EditorState.create({ doc, extensions: [cm.json()] });
  return { result: jsonSource(cm, () => spec)(new cm.CompletionContext(state, pos, explicit)), doc };
}
const labels = (source) => complete(source).result?.options.map((o) => o.label) || [];

check('JSON root field inserts a quoted name, colon and editable value', () => {
  const { result, doc } = complete('{"ri|"}');
  const option = result.options.find((o) => o.label === '"rich_message"');
  let output;
  option.apply({ dispatch: (tx) => { const c = tx.changes; output = doc.slice(0, c.from) + c.insert + doc.slice(c.to); } }, option, result.from, result.to);
  assert.deepEqual(JSON.parse(output), { rich_message: {} });
});
check('JSON nested fields follow the spec', () => assert(labels('{"rich_message": {|}}').includes('"blocks"')));
check('JSON completion preserves an existing colon and value', () => {
  const { result, doc } = complete('{"rich_message": {"mark|": "keep me"}}');
  const option = result.options.find((o) => o.label === '"markdown"');
  let output;
  option.apply({ dispatch: (tx) => { const c = tx.changes; output = doc.slice(0, c.from) + c.insert + doc.slice(c.to); } }, option, result.from, result.to);
  assert.equal(JSON.parse(output).rich_message.markdown, 'keep me');
});
check('JSON does not suggest duplicate object keys', () => assert(!labels('{"pin": true, |}').includes('"pin"')));
check('JSON boolean values are completed', () => assert.deepEqual(labels('{"pin": |}'), ['true', 'false']));
check('JSON boolean completion replaces the whole existing token', () => {
  for (const input of ['{"pin": tr|ue}', '{"pin": |true}', '{"pin": fa|lse}']) {
    const { result, doc } = complete(input);
    assert.deepEqual(JSON.parse(doc.slice(0, result.from) + 'false' + doc.slice(result.to)), { pin: false });
  }
});
check('All block discriminants are suggested inside arrays', () => {
  assert.deepEqual(labels('{"rich_message":{"blocks":[{"type": |}]}}').sort(), ['"heading"', '"paragraph"']);
});
check('Quoted JSON completion filters the prefix and replaces the closing quote', () => {
  for (const input of ['{"rich_message":{"blocks":[{"type": "|"}]}}', '{"rich_message":{"blocks":[{"type": "par|"}]}}']) {
    const { result, doc } = complete(input);
    assert.equal(result.filter, false);
    assert(result.options.some((o) => o.label === '"paragraph"'));
    assert.equal(JSON.parse(doc.slice(0, result.from) + '"paragraph"' + doc.slice(result.to)).rich_message.blocks[0].type, 'paragraph');
  }
  assert.deepEqual(labels('{"rich_message":{"blocks":[{"type": "par|"}]}}'), ['"paragraph"']);
});
check('A chosen block narrows the suggested fields', () => {
  assert(!labels('{"rich_message":{"blocks":[{"type":"paragraph", |}]}}').includes('"size"'));
  assert(labels('{"rich_message":{"blocks":[{"type":"heading", |}]}}').includes('"size"'));
});
check('Options use current method parameters and exclude routing fields', () => {
  assert(labels('{"options": {|}}').includes('"disable_notification"'));
  assert(!labels('{"options": {|}}').includes('"chat_id"'));
});
check('New spec fields become suggestions without changing editor code', () => {
  spec.types.InputRichMessage.fields.push(field('future_feature', ['String']));
  assert(labels('{"rich_message": {|}}').includes('"future_feature"'));
});
check('Literal True fields never suggest false', () => {
  spec.types.InputRichMessage.fields.push(field('enabled', ['True']));
  assert.deepEqual(labels('{"rich_message":{"enabled": |}}'), ['true']);
});
check('Ordinary JSON string values do not receive unrelated field suggestions', () => {
  assert.equal(complete('{"rich_message":{"markdown":"hello|"}}').result, null);
});
const snippets = { html: [{ group: 'Telegram', items: [{ label: 'Example', snip: '<tg-future mode="wide">$SEL$</tg-future>' }] }],
  markdown: [{ group: 'Markdown', items: [{ label: 'Ссылка', snip: '[$SEL$](https://)' }] }] };
check('Custom HTML tags and attributes come from the snippet catalog', () => assert.deepEqual(htmlTags(snippets)['tg-future'], { attrs: { mode: null } }));
check('Markdown slash menu ignores URLs', () => {
  const source = snippetSource('markdown', snippets, () => {});
  for (const doc of ['https://example.com/', 'Текст /']) {
    assert.equal(source(new cm.CompletionContext(cm.EditorState.create({ doc }), doc.length, false)), null);
  }
  assert(source(new cm.CompletionContext(cm.EditorState.create({ doc: '/' }), 1, false)).options.length);
});
console.log(`\n${passed}/${passed} source editor checks passed`);
