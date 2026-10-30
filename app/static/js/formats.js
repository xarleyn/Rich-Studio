// Explicit, loss-aware adapters for the editor's source modes. Never changes a sent payload.
// This is not the palette/schema: unknown Bot API fields remain editable, but cannot be silently converted.
import { tr } from './i18n.js';

const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
const mdEsc = (s) => esc(s).replace(/([\\`*_{}[\]()#+.!|>~=$-])/g, '\\$1').replace(/\n/g, '  \n');
const copy = (v) => JSON.parse(JSON.stringify(v));
const WRAPS = { bold: 'b', italic: 'i', underline: 'u', strikethrough: 's', subscript: 'sub', superscript: 'sup', marked: 'mark', code: 'code', spoiler: 'tg-spoiler' };
const MEDIA = { photo: 'img', video: 'video', animation: 'video', audio: 'audio', voice_note: 'audio', document: 'tg-document' };
const fail = (detail) => { throw new Error(tr('Переключение отменено: преобразование без потерь недоступно для ') + detail + tr('. Исходное содержимое сохранено.')); };
function keys(value, allowed) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) fail('JSON');
  const extra = Object.keys(value).filter((k) => !allowed.split(' ').includes(k));
  if (extra.length) fail((value.type || '') + ' / ' + extra.join(', '));
}
const tag = (name, body, attrs = '') => `<${name}${attrs}>${body}</${name}>`;
const attr = (name, value) => value === undefined ? '' : ` ${name}="${esc(value)}"`;

/** Convert supported blocks to both source formats, retaining full media objects in the media list. */
export function blocksToSources(blocks, existingMedia = []) {
  const media = copy(existingMedia);
  const inline = (v, markdown = false) => {
    if (v == null) return '';
    if (typeof v === 'string') return markdown ? mdEsc(v) : esc(v).replace(/\n/g, '<br>');
    if (Array.isArray(v)) return v.map((x) => inline(x, markdown)).join('');
    const type = v.type;
    if (Object.hasOwn(WRAPS, type)) {
      keys(v, 'type text');
      // Native Markdown for the common marks; HTML preserves nesting and edge whitespace reliably.
      const marker = { bold: '**', italic: '*', strikethrough: '~~', spoiler: '||', marked: '==' }[type];
      if (markdown && marker && typeof v.text === 'string' && v.text.trim() === v.text && v.text && !v.text.includes('\n')) return marker + inline(v.text, true) + marker;
      return tag(WRAPS[type], inline(v.text));
    }
    if (type === 'url') { keys(v, 'type text url'); return tag('a', inline(v.text), attr('href', v.url)); }
    if (type === 'custom_emoji') { keys(v, 'type alternative_text custom_emoji_id'); return tag('tg-emoji', esc(v.alternative_text), attr('emoji-id', v.custom_emoji_id)); }
    if (type === 'mathematical_expression') { keys(v, 'type expression'); return tag('tg-math', esc(v.expression)); }
    if (type === 'anchor') { keys(v, 'type name'); return tag('a', '', attr('name', v.name)); }
    if (type === 'anchor_link' || type === 'reference_link') {
      const field = type === 'anchor_link' ? 'anchor_name' : 'reference_name';
      keys(v, 'type text ' + field); return tag('a', inline(v.text), attr('href', '#' + v[field]));
    }
    if (type === 'reference') { keys(v, 'type text name'); return tag('tg-reference', inline(v.text), attr('name', v.name)); }
    if (type === 'date_time') { keys(v, 'type text unix_time date_time_format'); if (!v.text) fail(type); return tag('tg-time', inline(v.text), attr('unix', v.unix_time) + attr('format', v.date_time_format)); }
    fail(type);
  };
  const caption = (c) => { if (!c) return ''; keys(c, 'text credit'); return tag('figcaption', inline(c.text) + (c.credit ? tag('cite', inline(c.credit)) : '')); };
  const render = (b, markdown = false) => {
    const t = b.type;
    if (markdown && ['thinking', 'mathematical_expression', 'collage', 'slideshow', 'anchor'].includes(t)) return tag('div', render(b));
    if (['paragraph', 'heading', 'footer', 'thinking'].includes(t)) {
      keys(b, 'type text' + (t === 'heading' ? ' size' : ''));
      if (t === 'heading' && (!Number.isInteger(b.size) || b.size < 1 || b.size > 6)) fail('heading.size');
      const plainMarkdown = typeof b.text === 'string' && b.text && b.text.trim() === b.text
        && !/\n[ \t]*\n|https?:\/\/|www\.|[\w.+-]+@[\w.-]+\./i.test(b.text)
        && (t !== 'heading' || !b.text.includes('\n'));
      if (markdown && ['paragraph', 'heading'].includes(t) && plainMarkdown) return (t === 'heading' ? '#'.repeat(b.size) + ' ' : '') + inline(b.text, true);
      return tag(t === 'heading' ? 'h' + b.size : t === 'paragraph' ? 'p' : t === 'thinking' ? 'tg-thinking' : t, inline(b.text));
    }
    if (t === 'pre') {
      keys(b, 'type text language');
      if (b.language && !/^[\w+#-]+$/.test(b.language)) fail('pre.language');
      return tag('pre', tag('code', typeof b.text === 'string' ? esc(b.text) : inline(b.text), b.language ? attr('class', 'language-' + b.language) : ''));
    }
    if (t === 'divider') { keys(b, 'type'); return markdown ? '---' : '<hr>'; }
    if (t === 'anchor') { keys(b, 'type name'); return tag('a', '', attr('name', b.name)); }
    if (t === 'mathematical_expression') { keys(b, 'type expression'); return tag('tg-math-block', esc(b.expression)); }
    if (t === 'blockquote') { keys(b, 'type blocks credit'); return tag('blockquote', b.blocks.map((x) => render(x)).join('') + (b.credit ? tag('cite', inline(b.credit)) : '')); }
    if (t === 'pullquote' || t === 'expandable_blockquote') {
      keys(b, 'type text credit'); return tag(t === 'pullquote' ? 'aside' : 'blockquote', inline(b.text) + (b.credit ? tag('cite', inline(b.credit)) : ''), t === 'expandable_blockquote' ? ' expandable' : '');
    }
    if (t === 'details') { keys(b, 'type summary blocks is_open'); return tag('details', tag('summary', inline(b.summary)) + b.blocks.map((x) => render(x)).join(''), b.is_open ? ' open' : ''); }
    if (t === 'collage' || t === 'slideshow') { keys(b, 'type blocks caption'); return tag('tg-' + t, b.blocks.map((x) => render(x)).join('') + caption(b.caption)); }
    if (t === 'list') {
      keys(b, 'type items'); const ordered = b.items.some((i) => i.value !== undefined || i.type);
      return tag(ordered ? 'ol' : 'ul', b.items.map((i) => {
        keys(i, 'blocks value type has_checkbox is_checked');
        return tag('li', (i.has_checkbox ? '<input type="checkbox"' + (i.is_checked ? ' checked' : '') + '>' : '') + i.blocks.map((x) => render(x)).join(''), attr('value', i.value) + attr('type', i.type));
      }).join(''));
    }
    if (t === 'table') {
      keys(b, 'type cells caption is_bordered is_striped is_compact');
      return tag('table', (b.caption ? tag('caption', inline(b.caption)) : '') + b.cells.map((row) => tag('tr', row.map((c) => {
        keys(c, 'text is_header align valign colspan rowspan'); if (c.text === undefined) fail('table / empty cell');
        return tag(c.is_header ? 'th' : 'td', inline(c.text), attr('align', c.align ?? 'left') + attr('valign', c.valign ?? 'top') + ['colspan', 'rowspan'].map((a) => attr(a, c[a])).join(''));
      }).join(''))).join(''), ['bordered', 'striped', 'compact'].map((a) => b['is_' + a] ? ' ' + a : '').join(''));
    }
    if (Object.hasOwn(MEDIA, t)) {
      keys(b, 'type ' + t + ' caption');
      let item = media.find((x) => JSON.stringify(x.media) === JSON.stringify(b[t]));
      if (!item) { let n = 1; while (media.some((x) => x.id === 'converted' + n)) n++; item = { id: 'converted' + n, media: copy(b[t]) }; media.push(item); }
      const attributes = attr('src', `tg://${t === 'voice_note' ? 'voice' : t}?id=${item.id}`);
      return tag('figure', (t === 'photo' ? '<img' + attributes + '>' : tag(MEDIA[t], '', attributes)) + caption(b.caption));
    }
    if (t === 'map') {
      keys(b, 'type location zoom width height caption'); keys(b.location, 'latitude longitude');
      return tag('figure', tag('tg-map', '', attr('lat', b.location.latitude) + attr('long', b.location.longitude) + ['zoom', 'width', 'height'].map((a) => attr(a, b[a])).join('')) + caption(b.caption));
    }
    fail(t);
  };
  if (!Array.isArray(blocks)) fail('blocks');
  const html = blocks.map((b) => render(b)).join('\n');
  const markdown = blocks.map((b) => render(b, true)).join('\n\n');
  return { html, markdown, media };
}

const INLINE = new Set(['b', 'strong', 'i', 'em', 'u', 'ins', 's', 'strike', 'del', 'sub', 'sup', 'mark', 'code', 'a', 'br', 'tg-spoiler', 'tg-emoji', 'tg-math', 'tg-time', 'tg-reference']);
/** Read an inert document. Unknown tags/attributes cause an atomic refusal rather than lost content. */
export function sourceToBlocks(source, format, media = []) {
  const html = format === 'markdown' ? RichRender.markdownHtml(source) : source;
  const doc = new DOMParser().parseFromString(html, 'text/html');
  if (doc.head.childNodes.length) fail('head');
  if (doc.body.attributes.length || doc.documentElement.attributes.length) fail('html/body attributes');
  const attrs = (e, names = '') => {
    for (const a of e.attributes) if (!names.split(' ').includes(a.name)) fail('<' + e.localName + '> / ' + a.name);
  };
  const rt = (nodes, pre = false) => {
    const values = [];
    for (const n of nodes) {
      if (n.nodeType === 3) {
        const value = pre ? n.textContent : n.textContent.replace(/[\t\n\r ]+/g, ' ');
        if (typeof values[values.length - 1] === 'string') values[values.length - 1] += value; else if (value) values.push(value);
        continue;
      }
      if (n.nodeType !== 1) fail('HTML comment');
      const t = n.localName;
      const a = (k) => n.getAttribute(k);
      const text = () => rt(n.childNodes, pre);
      const mark = Object.entries(WRAPS).find(([, tagName]) => tagName === t)?.[0] || { strong: 'bold', em: 'italic', ins: 'underline', strike: 'strikethrough', del: 'strikethrough' }[t];
      if (mark) { attrs(n); values.push({ type: mark, text: text() }); }
      else if (t === 'br') { attrs(n); values.push('\n'); }
      else if (t === 'a') {
        attrs(n, 'href name');
        if (a('name') && !a('href') && !n.textContent) values.push({ type: 'anchor', name: a('name') });
        else {
          const url = a('href'); if (!url || !RichRender.safeUrl(url) || a('name')) fail('<a>');
          const name = url.startsWith('#') ? url.slice(1) : null;
          const ref = name !== null && [...doc.querySelectorAll('tg-reference')].some((e) => e.getAttribute('name') === name);
          const anchor = name !== null && (!name || [...doc.querySelectorAll('a[name]')].some((e) => e.getAttribute('name') === name));
          if (ref || anchor) values.push({ type: ref ? 'reference_link' : 'anchor_link', [ref ? 'reference_name' : 'anchor_name']: name, text: text() });
          else values.push({ type: 'url', url, text: text() });
        }
      }
      else if (t === 'tg-emoji') { attrs(n, 'emoji-id'); if (n.children.length) fail(t); values.push({ type: 'custom_emoji', custom_emoji_id: a('emoji-id'), alternative_text: n.textContent }); }
      else if (t === 'tg-math') { attrs(n); if (n.children.length) fail(t); values.push({ type: 'mathematical_expression', expression: n.textContent }); }
      else if (t === 'tg-time') { attrs(n, 'unix format'); values.push({ type: 'date_time', unix_time: number(n, 'unix'), date_time_format: a('format') || 'wDT', text: text() }); }
      else if (t === 'tg-reference') { attrs(n, 'name'); values.push({ type: 'reference', name: a('name'), text: text() }); }
      else fail('<' + t + '>');
    }
    return values.length === 1 ? values[0] : values.length ? values : '';
  };
  const number = (e, name) => { const v = e.getAttribute(name); if (v === null || !v.trim() || !Number.isFinite(Number(v))) fail(e.localName + '.' + name); return Number(v); };
  const content = (e, except) => [...e.childNodes].filter((n) => n.nodeType !== 1 || !except.includes(n.localName));
  const child = (e, name) => {
    const matches = [...e.children].filter((n) => n.localName === name); if (matches.length > 1) fail(name);
    if (matches[0]) attrs(matches[0]); return matches[0];
  };
  const cap = (e) => {
    const c = child(e, 'figcaption'); if (!c) return {};
    const credit = child(c, 'cite'); return { caption: { text: rt(content(c, ['cite'])), ...(credit ? { credit: rt(credit.childNodes) } : {}) } };
  };
  const blocks = (nodes, depth = 0) => {
    if (depth > 40) fail('nesting');
    const out = []; let loose = [];
    const flush = () => { if (loose.some((n) => n.nodeType === 1 || n.textContent.trim())) out.push({ type: 'paragraph', text: rt(loose) }); loose = []; };
    for (const n of nodes) {
      if (n.nodeType === 3 || (n.nodeType === 1 && INLINE.has(n.localName) && !(n.localName === 'a' && n.hasAttribute('name') && !n.textContent))) { loose.push(n); continue; }
      flush(); if (n.nodeType !== 1) fail('HTML comment');
      const t = n.localName;
      const inner = (except = []) => blocks(content(n, except), depth + 1);
      if (t === 'div') { attrs(n); out.push(...inner()); }
      else if (t === 'p' || /^h[1-6]$/.test(t) || ['footer', 'tg-thinking'].includes(t)) {
        attrs(n); out.push({ type: t === 'p' ? 'paragraph' : t === 'footer' ? 'footer' : t === 'tg-thinking' ? 'thinking' : 'heading', ...(/^h/.test(t) ? { size: +t[1] } : {}), text: rt(n.childNodes) });
      } else if (t === 'hr') { attrs(n); out.push({ type: 'divider' }); }
      else if (t === 'a') { attrs(n, 'name'); out.push({ type: 'anchor', name: n.getAttribute('name') }); }
      else if (t === 'pre') {
        attrs(n); const c = n.firstElementChild;
        if (c && (c.localName !== 'code' || n.children.length !== 1 || [...n.childNodes].some((x) => x !== c && x.textContent.trim()))) fail('<pre>');
        if (c) attrs(c, 'class');
        const cls = c?.getAttribute('class'); if (cls && !/^language-[\w+#-]+$/.test(cls)) fail(cls);
        out.push({ type: 'pre', text: rt((c || n).childNodes, true), ...(cls ? { language: cls.slice(9) } : {}) });
      } else if (t === 'tg-math-block') { attrs(n); if (n.children.length) fail(t); out.push({ type: 'mathematical_expression', expression: n.textContent }); }
      else if (t === 'blockquote' || t === 'aside') {
        attrs(n, t === 'blockquote' ? 'expandable' : ''); const credit = child(n, 'cite');
        const basic = t === 'blockquote' && !n.hasAttribute('expandable');
        out.push({ type: basic ? 'blockquote' : t === 'aside' ? 'pullquote' : 'expandable_blockquote', ...(basic ? { blocks: inner(['cite']) } : { text: rt(content(n, ['cite'])) }), ...(credit ? { credit: rt(credit.childNodes) } : {}) });
      } else if (t === 'details') {
        attrs(n, 'open'); const summary = child(n, 'summary'); if (!summary) fail('<details>');
        out.push({ type: 'details', summary: rt(summary.childNodes), blocks: inner(['summary']), ...(n.hasAttribute('open') ? { is_open: true } : {}) });
      } else if (t === 'ul' || t === 'ol') {
        attrs(n, t === 'ol' ? 'start type' : ''); if ([...n.childNodes].some((x) => x.nodeType === 3 ? x.textContent.trim() : x.localName !== 'li')) fail(t);
        let nextValue = n.hasAttribute('start') ? number(n, 'start') : 1;
        const items = [...n.children].map((li) => {
          attrs(li, t === 'ol' ? 'value type' : ''); const box = li.querySelector(':scope > input, :scope > p:first-child > input:first-child');
          const checked = !!box?.hasAttribute('checked');
          if (box) { attrs(box, 'type checked'); if (box.getAttribute('type') !== 'checkbox') fail('input'); box.remove(); }
          const value = li.hasAttribute('value') ? number(li, 'value') : nextValue;
          nextValue = value + 1;
          return { blocks: blocks(li.childNodes, depth + 1), ...(t === 'ol' ? { value, ...(li.getAttribute('type') || n.getAttribute('type') ? { type: li.getAttribute('type') || n.getAttribute('type') } : {}) } : {}), ...(box ? { has_checkbox: true, is_checked: checked } : {}) };
        }); out.push({ type: 'list', items });
      } else if (t === 'table') {
        attrs(n, 'bordered striped compact'); const caption = child(n, 'caption');
        for (const container of n.children) if (!['caption', 'thead', 'tbody', 'tfoot', 'tr'].includes(container.localName)) fail(container.localName); else attrs(container);
        const rows = [...n.querySelectorAll(':scope > tr, :scope > thead > tr, :scope > tbody > tr, :scope > tfoot > tr')];
        const cells = rows.map((row) => { attrs(row); return [...row.children].map((c) => {
          if (!['th', 'td'].includes(c.localName)) fail(c.localName); attrs(c, 'align valign colspan rowspan');
          return { text: rt(c.childNodes), align: c.localName === 'th' ? 'center' : 'left', valign: 'middle', ...(c.localName === 'th' ? { is_header: true } : {}), ...Object.fromEntries(['align', 'valign', 'colspan', 'rowspan'].filter((k) => c.hasAttribute(k)).map((k) => [k, k.endsWith('span') ? number(c, k) : c.getAttribute(k)])) };
        }); });
        out.push({ type: 'table', cells, ...(caption ? { caption: rt(caption.childNodes) } : {}), ...Object.fromEntries(['bordered', 'striped', 'compact'].filter((k) => n.hasAttribute(k)).map((k) => ['is_' + k, true])) });
      } else if (t === 'figure') {
        attrs(n); const body = inner(['figcaption']); if (body.length !== 1) fail('<figure>'); out.push({ ...body[0], ...cap(n) });
      } else if (t === 'tg-collage' || t === 'tg-slideshow') { attrs(n); out.push({ type: t.slice(3), blocks: inner(['figcaption']), ...cap(n) }); }
      else if (['img', 'video', 'audio', 'tg-document'].includes(t)) {
        attrs(n, 'src alt tg-spoiler'); if (n.childNodes.length || n.getAttribute('alt')) fail(t + ' content');
        const src = n.getAttribute('src') || ''; const match = /^tg:\/\/(photo|video|animation|audio|voice|voice_note|document)\?id=([\w.-]+)$/.exec(src);
        const kind = match ? match[1] === 'voice' ? 'voice_note' : match[1] : { img: 'photo', video: 'video', audio: 'audio', 'tg-document': 'document' }[t];
        if (MEDIA[kind] !== t) fail(t + ' / ' + kind);
        const item = match ? copy(media.find((x) => x.id === match[2])?.media ?? null) : /^https?:\/\//.test(src) && RichRender.safeUrl(src) ? { type: kind, media: src } : null;
        if (!item) fail(src || t); if (n.hasAttribute('tg-spoiler')) { if (typeof item !== 'object') fail('tg-spoiler'); item.has_spoiler = true; }
        out.push({ type: kind, [kind]: item });
      } else if (t === 'tg-map') {
        attrs(n, 'lat long zoom width height'); if (n.childNodes.length) fail(t);
        out.push({ type: 'map', location: { latitude: number(n, 'lat'), longitude: number(n, 'long') }, ...Object.fromEntries(['zoom', 'width', 'height'].filter((k) => n.hasAttribute(k)).map((k) => [k, number(n, k)])) });
      } else fail('<' + t + '>');
    }
    flush(); return out;
  };
  return blocks(doc.body.childNodes);
}

/** All work happens on copies. Callers may apply the result only after conversion succeeds. */
export function convertFormats(format, buffers, media) {
  const blocks = format === 'blocks' ? copy(buffers.blocks) : sourceToBlocks(buffers[format], format, media);
  const sources = blocksToSources(blocks, media);
  return { buffers: { blocks, html: format === 'html' ? buffers.html : sources.html, markdown: format === 'markdown' ? buffers.markdown : sources.markdown }, media: sources.media };
}
