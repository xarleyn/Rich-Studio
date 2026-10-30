import { tr } from './i18n.js';
// Suggestions are data: Bot API fields come from the current spec, Rich HTML/Markdown from the snippet catalog.
// This module has no application state or DOM access. CodeMirror is supplied by the lazily loaded component.

export function htmlTags(snippets) {
  const tags = {};
  for (const group of snippets.html || []) for (const item of group.items) {
    for (const match of item.snip.matchAll(/<([\w-]+)\b([^>]*)>/g)) {
      const tag = tags[match[1]] ||= { attrs: {} };
      for (const attr of match[2].matchAll(/\s([\w-]+)(?:="[^"]*")?/g)) tag.attrs[attr[1]] = null;
    }
  }
  return tags;
}

export function snippetSource(kind, snippets, insert) {
  return (context) => {
    const prefix = context.matchBefore(kind === 'html' ? /<[\w-]*/ : /\/[\p{L}\w-]*/u);
    if (!context.explicit && !prefix) return null;
    // Markdown uses / at the start of a line; don't open a menu in URLs or ordinary prose.
    if (kind === 'markdown' && prefix && context.state.doc.lineAt(prefix.from).from !== prefix.from) return null;
    const options = (snippets[kind] || []).flatMap((group) => group.items.map((item) => ({
      label: kind === 'html' ? (item.snip.match(/^<([\w-]+)/)?.[1] || tr(item.label)) : tr(item.label),
      displayLabel: kind === 'html' ? `${item.snip.match(/^<([\w-]+)/)?.[0] || ''}> · ${tr(item.title || item.label)}` : tr(item.title || item.label),
      detail: tr(group.group), type: 'text', info: tr(item.snip).replaceAll('$SEL$', '…').replaceAll('$ID$', tr('медиа')),
      apply: (view, completion, from, to) => insert(item, prefix ? prefix.from : from, to),
    })));
    // Match names without the < or /; applying the snippet replaces the trigger as well.
    return { from: prefix ? prefix.from + 1 : context.pos, options, validFor: /^[\p{L}\w-]*$/u };
  };
}

function propertyName(property, doc) {
  const key = property.getChild('PropertyName');
  if (!key) return null;
  try { return JSON.parse(doc.sliceString(key.from, key.to)); } catch { return null; }
}

function discriminator(object, doc) {
  const values = {};
  for (const property of object.getChildren('Property')) {
    const key = propertyName(property, doc);
    if (!['type', 'source', 'status'].includes(key)) continue;
    const value = property.getChild('String');
    if (value) { try { values[key] = JSON.parse(doc.sliceString(value.from, value.to)); } catch { /* incomplete input */ } }
  }
  return values;
}

function definitions(spec) {
  const field = (name, types, description) => ({ name, types, description, required: false });
  return {
    ...spec.types,
    $Post: { fields: [
      field('rich_message', ['InputRichMessage'], tr('Содержимое поста: HTML, Markdown или блоки')),
      field('reply_markup', ['InlineKeyboardMarkup'], tr('Кнопки под постом')),
      field('options', ['$Options'], tr('Дополнительные параметры sendRichMessage')),
      field('pin', ['Boolean'], tr('Закрепить пост после публикации')),
      field('comment', ['InputRichMessage'], tr('Первый комментарий в обсуждении')),
      field('poll_after', ['$Poll'], tr('Опрос после поста')),
      field('callbacks', ['Object'], tr('Ответы на кнопки callback_data')),
    ] },
    $Options: { fields: (spec.methods.sendRichMessage?.fields || []).filter((f) => !['chat_id', 'rich_message', 'reply_markup'].includes(f.name)) },
    $Poll: { fields: (spec.methods.sendPoll?.fields || []).filter((f) => f.name !== 'chat_id') },
  };
}

function fieldsFor(types, object, doc, defs, seen = new Set()) {
  const fields = new Map();
  const add = (key, field) => {
    const prior = fields.get(key);
    fields.set(key, prior ? { ...field, types: [...new Set([...prior.types, ...field.types])],
      enumDescriptions: { ...prior.enumDescriptions, ...field.enumDescriptions },
      enum: [...new Set([...(prior.enum || []), ...(field.enum || [])])] } : field);
  };
  const values = discriminator(object, doc);
  for (const name of types) {
    if (seen.has(name)) continue;
    const type = defs[name];
    if (!type) continue;
    const visited = new Set([...seen, name]);
    if (type.subtypes?.length) {
      const matching = type.subtypes.filter((n) => defs[n]?.const && Object.values(values).includes(defs[n].const));
      for (const [key, field] of fieldsFor(matching.length ? matching : type.subtypes, object, doc, defs, visited)) {
        add(key, field);
      }
    }
    for (const f of type.fields || []) {
      const constant = type.const && ['type', 'source', 'status'].includes(f.name) ? [type.const] : [];
      add(f.name, { ...f, enum: [...(f.enum || []), ...constant],
        enumDescriptions: Object.fromEntries(constant.map((value) => [value, f.description])) });
    }
  }
  return fields;
}

function containerTypes(node, doc, defs, depth = 0) {
  if (depth > 100) return [];
  const parent = node.parent;
  if (!parent || parent.name === 'JsonText') return ['$Post'];
  if (parent.name === 'Array') return containerTypes(parent, doc, defs, depth + 1).filter((t) => t.startsWith('Array of ')).map((t) => t.slice(9));
  if (parent.name === 'Property' && parent.parent?.name === 'Object') {
    const object = parent.parent;
    return fieldsFor(containerTypes(object, doc, defs, depth + 1), object, doc, defs).get(propertyName(parent, doc))?.types || [];
  }
  return [];
}

function emptyValue(types) {
  if (types.includes('String') || types.includes('RichText')) return '""';
  if (types.some((t) => t.startsWith('Array of '))) return '[]';
  if (types.includes('Boolean')) return 'false';
  if (types.includes('True')) return 'true';
  if (types.includes('Integer') || types.includes('Float')) return '0';
  return '{}';
}

export function jsonSource(cm, getSpec) {
  return (context) => {
    const spec = getSpec();
    if (!spec) return null;
    const { state, pos } = context;
    const doc = state.doc;
    const node = cm.syntaxTree(state).resolveInner(pos, -1);
    let object = node;
    while (object && object.name !== 'Object') object = object.parent;
    if (!object) return null;
    const defs = definitions(spec);
    const fields = fieldsFor(containerTypes(object, doc, defs), object, doc, defs);
    let property = node;
    while (property && property !== object && property.name !== 'Property') property = property.parent;
    const key = property?.name === 'Property' ? property.getChild('PropertyName') : null;
    const colon = property?.name === 'Property' ? property.getChild(':') : null;
    const inValue = !!colon && pos > colon.from;
    if (inValue) {
      const field = fields.get(propertyName(property, doc));
      if (!field) return null;
      const values = [...(field.enum || []).map((v) => JSON.stringify(v))];
      if (field.types.includes('Boolean')) values.push('true', 'false');
      else if (field.types.includes('True')) values.push('true');
      if (!values.length) return null;
      const value = property.getChild('String') || property.getChild('True') || property.getChild('False');
      const word = context.matchBefore(/[\w-]*/);
      const from = value ? value.from : word?.from ?? pos;
      // Filter only the text before the cursor; replace the whole token, including its closing quote.
      const prefix = doc.sliceString(from, pos).toLowerCase();
      return { from, to: value ? value.to : pos, filter: false,
        options: [...new Set(values)].filter((label) => label.toLowerCase().startsWith(prefix))
          .map((label) => ({ label, type: 'enum', info: field.enumDescriptions?.[JSON.parse(label)] || field.description })) };
    }
    const word = context.matchBefore(/"?[\w]*"?/);
    if (!context.explicit && !word?.text) return null;
    const from = key ? key.from : word?.from ?? pos;
    const to = key ? key.to : pos;
    const existing = object.getChildren('Property').filter((p) => p !== property).map((p) => propertyName(p, doc));
    const prefix = doc.sliceString(from, pos).toLowerCase();
    return { from, to, filter: false, options: [...fields.values()].filter((f) => !existing.includes(f.name)
      && JSON.stringify(f.name).toLowerCase().startsWith(prefix.startsWith('"') ? prefix : '"' + prefix)).map((f) => ({
      label: JSON.stringify(f.name), displayLabel: f.name, type: 'property',
      detail: f.types.join(' | '), info: (f.required ? tr('Обязательное поле. ') : '') + f.description,
      apply: (view, completion, start, end) => {
        const value = emptyValue(f.types);
        const text = JSON.stringify(f.name) + (colon ? '' : ': ' + value);
        view.dispatch({ changes: { from: start, to: end, insert: text },
          selection: { anchor: start + text.length - (!colon && ['""', '{}', '[]'].includes(value) ? 1 : 0) }, userEvent: 'input.complete' });
      },
    })) };
  };
}
