import { tr } from './i18n.js';
// HTML and Markdown modes: snippet bars and the source-editor bridge.
import { $, h } from './dom.js';
import { changed } from './editor.js';
import { addMediaEntry } from './media.js';
import { S } from './state.js';

// ---------- code modes ----------
export function insertAtCursor(ta, snip) {
  if (ta.insertSnippet) { ta.insertSnippet(snip); return; }
  const [a, b] = [ta.selectionStart, ta.selectionEnd];
  const sel = ta.value.slice(a, b);
  const text = snip.replace('$SEL$', sel);
  const caret = snip.includes('$SEL$') && !sel ? a + snip.indexOf('$SEL$') : a + text.length;
  ta.setRangeText(text, a, b, 'end');
  ta.focus();
  ta.setSelectionRange(caret, caret);
  ta.dispatchEvent(new Event('input'));
}

export function snippetBar(kind) {
  const bar = $('#snip-' + kind);
  bar.replaceChildren();
  (window.SNIPPETS[kind] || []).forEach((g) => {
    bar.append(h('div', { class: 'snip-group' }, h('span', { text: tr(g.group) }), g.items.map((it) => h('button', {
      type: 'button', class: it.verified === false ? 'unverified' : '', text: tr(it.label),
      title: tr(it.title || it.label) + (it.verified === false ? tr('\n⚠ синтаксис из сторонних источников, проверьте на тестовом канале') : ''),
      onclick: () => {
        let snip = tr(it.snip);
        if (it.media) snip = snip.replace('$ID$', addMediaEntry(it.media, ''));
        insertAtCursor($('#src-' + kind), snip);
      },
    }))));
  });
}

export function bindCode(kind) {
  const ta = $('#src-' + kind);
  ta.addEventListener('source-change', () => {
    if (kind === 'json') S.pendingJson = ta.value;
    else S.buf[kind] = ta.value;
    changed();
  });
  ta.addEventListener('snippet', (e) => {
    const { item, from, to } = e.detail;
    const template = tr(item.snip);
    const snip = item.media ? template.replace('$ID$', addMediaEntry(item.media, '')) : template;
    ta.insertSnippet(snip, from, to);
  });
}
