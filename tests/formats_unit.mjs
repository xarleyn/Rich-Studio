// Format conversion against the actual browser parser and Telegram preview dialect.
/* global RichRender -- classic script in page.evaluate */
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { chromium } from 'playwright';

const browser = await chromium.launch(process.env.CHROMIUM ? { executablePath: process.env.CHROMIUM } : {});
try {
  const page = await browser.newPage();
  await page.route('http://formats.test/**', async (route) => {
    const pathname = new URL(route.request().url()).pathname;
    if (pathname === '/') return route.fulfill({ contentType: 'text/html', body: '<script src="/vendor/markdown-it.min.js"></script><script src="/render.js"></script>' });
    const file = resolve('app/static', '.' + pathname);
    if (!file.startsWith(resolve('app/static') + '/')) {
      // Windows uses backslashes, resolve and check again with its actual separator.
      if (!file.startsWith(resolve('app/static') + '\\')) return route.abort();
    }
    await route.fulfill({ contentType: 'text/javascript', body: await readFile(file, 'utf8') });
  });
  await page.goto('http://formats.test/');
  const cases = await page.evaluate(async () => {
    const { blocksToSources, sourceToBlocks } = await import('/js/formats.js');
    const results = [];
    const check = (name, test) => { try { test(); results.push({ name, ok: true }); } catch (e) { results.push({ name, ok: false, error: e.message }); } };
    const equal = (a, b) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(JSON.stringify({ actual: a, expected: b })); };
    const examples = [
      { type: 'heading', size: 2, text: 'A *title* & <tag>' },
      { type: 'paragraph', text: ['Hello ', { type: 'bold', text: ['bold ', { type: 'italic', text: 'nested' }] }, '\nnew line'] },
      { type: 'paragraph', text: [{ type: 'url', url: 'https://example.com/?a=1&b=2', text: 'Link' }, { type: 'custom_emoji', custom_emoji_id: '123', alternative_text: '⭐' }, { type: 'spoiler', text: 'secret' }] },
      { type: 'pre', language: 'js', text: 'if (a < 3) {\n  call();\n}\n' },
      { type: 'divider' },
      { type: 'blockquote', blocks: [{ type: 'paragraph', text: 'Quote' }], credit: 'Author' },
      { type: 'expandable_blockquote', text: 'Fold me', credit: 'Author' },
      { type: 'details', summary: 'More', is_open: true, blocks: [{ type: 'paragraph', text: 'Body' }] },
      { type: 'list', items: [{ value: 3, blocks: [{ type: 'paragraph', text: 'Three' }] }, { value: 4, has_checkbox: true, is_checked: true, blocks: [{ type: 'paragraph', text: 'Done' }] }] },
      { type: 'table', is_bordered: true, cells: [[{ text: 'One', is_header: true, colspan: 2 }], [{ text: 'Two' }, { text: 'Three', align: 'right' }]] },
      { type: 'photo', photo: { type: 'photo', media: 'upload:cat.png', has_spoiler: true, future_media: 'keep' }, caption: { text: 'Cat', credit: 'Me' } },
      { type: 'map', location: { latitude: 55.7, longitude: 37.6 }, width: 640, height: 400, zoom: 12 },
      { type: 'mathematical_expression', expression: 'x^2 + y^2' },
      { type: 'paragraph', text: [{ type: 'bold', text: 'one' }, { type: 'bold', text: 'two' }] },
      { type: 'paragraph', text: ['hello', { type: 'bold', text: '!' }, 'world'] },
      { type: 'paragraph', text: '    four spaces' },
      { type: 'paragraph', text: '' },
      { type: 'heading', size: 1, text: 'One\nTwo' },
      { type: 'paragraph', text: 'One\n\nTwo' },
      { type: 'paragraph', text: 'https://example.com' },
    ];
    for (const b of examples) for (const format of ['html', 'markdown']) check(b.type + ' → ' + format + ' → blocks', () => {
      const sources = blocksToSources([b]);
      const parsed = sourceToBlocks(sources[format], format, sources.media);
      const before = RichRender.outline({ rich_message: { blocks: [b] } });
      const after = RichRender.outline({ rich_message: { blocks: parsed } });
      equal(after, before);
      const sourcePost = { rich_message: { [format]: sources[format], media: sources.media } };
      // outline encodes a block's BRs and a source PRE's literal newlines differently.
      // Inspect actual code text here, where whitespace is semantically significant.
      if (b.type === 'pre') { equal(parsed[0].text, b.text); equal(RichRender.render(sourcePost).querySelector('pre').textContent, b.text); }
      else equal(RichRender.outline(sourcePost), before);
      if (b.type === 'photo') equal(parsed[0].photo, b.photo);
    });
    check('native Markdown heading/emphasis and lists become editable blocks', () => {
      const parsed = sourceToBlocks('# Title\n\n**Bold** and *italic*\n\n- [x] Done\n- Next', 'markdown');
      equal(parsed.map((x) => x.type), ['heading', 'paragraph', 'list']);
      equal(parsed[2].items[0].is_checked, true);
    });
    check('media IDs never collide with existing mappings', () => {
      const result = blocksToSources([examples[10]], [{ id: 'converted1', media: { type: 'photo', media: 'other' } }]);
      equal(result.media.map((x) => x.id), ['converted1', 'converted2']);
    });
    check('ordered list numbering follows explicit resets', () => equal(sourceToBlocks('<ol><li value="5">Five</li><li>Six</li></ol>', 'html')[0].items.map((x) => x.value), [5, 6]));
    check('fragment without anchor stays an ordinary URL', () => equal(sourceToBlocks('<p><a href="#missing">Link</a></p>', 'html')[0].text.type, 'url'));
    for (const html of ['<p onclick="alert(1)">x</p>', '<script>alert(1)</script>', '<tg-future value="keep">x</tg-future>', '<p><a href="javascript:alert(1)">x</a></p>', '<p style="color:red">x</p>', '<p><tg-emoji emoji-id="1"><b>bold</b></tg-emoji></p>', '<body onload="alert(1)">text</body>']) check('unsupported or hostile HTML is refused: ' + html, () => {
      let refused = false; try { sourceToBlocks(html, 'html'); } catch { refused = true; } if (!refused) throw new Error('not refused');
    });
    for (const b of [{ type: 'future', payload: 'keep' }, { type: 'paragraph', text: 'Keep', new_field: 42 }, { type: 'constructor' }, { type: 'paragraph', text: { type: 'constructor', text: 'keep' } }]) check('unknown Bot API fields are refused, never discarded', () => {
      let refused = false; try { blocksToSources([b]); } catch { refused = true; } if (!refused) throw new Error('not refused');
    });
    return results;
  });
  for (const c of cases) console.log(`${c.ok ? 'PASS' : 'FAIL'} ${c.name}${c.error ? ': ' + c.error : ''}`);
  assert(cases.every((c) => c.ok), 'format conversion failed');
} finally { await browser.close(); }
