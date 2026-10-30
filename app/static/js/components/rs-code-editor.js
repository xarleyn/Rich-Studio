// Local, lazily loaded source editor. Post state stays outside: value/spec/snippets in, source-change/snippet out.
import { css, html, LitElement } from '../../vendor/lit-core.min.js';
import { htmlTags, jsonSource, snippetSource } from '../source-completion.js';
import { h } from '../dom.js';

/** @type {Promise<typeof import('../../vendor/code-editor.js')> | undefined} */
let library;

export class RsCodeEditor extends LitElement {
  static properties = {
    language: { type: String }, placeholder: { type: String }, spec: { attribute: false }, snippets: { attribute: false },
    ready: { state: true }, message: { state: true }, position: { state: true },
  };

  static styles = css`
    :host { display: block; min-width: 0; color: var(--text, #17212b); }
    * { box-sizing: border-box; }
    .frame { border: 1px solid var(--border, #dfe3e8); border-radius: 9px; background: var(--panel, white); overflow: hidden; }
    .bar, .status { display: flex; align-items: center; flex-wrap: wrap; gap: 6px; padding: 6px 10px; font: 12px/1.4 system-ui, sans-serif; }
    .bar { border-bottom: 1px solid var(--border, #dfe3e8); background: var(--soft, #eef3f8); }
    .language { font-weight: 600; margin-right: auto; }
    button { font: inherit; border: 1px solid var(--border, #dfe3e8); border-radius: 6px; padding: 4px 8px;
      color: var(--text, #17212b); background: var(--panel, white); cursor: pointer; }
    button:hover { border-color: var(--accent, #3390ec); }
    button:focus-visible { outline: 2px solid var(--accent, #3390ec); outline-offset: 1px; }
    button:disabled { opacity: .5; cursor: default; }
    .mount, textarea { height: 420px; min-height: 240px; resize: vertical; overflow: auto; }
    textarea { display: block; width: 100%; border: 0; padding: 10px; background: inherit; color: inherit;
      font: 13px/1.6 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; tab-size: 2; }
    [hidden] { display: none !important; }
    .status { border-top: 1px solid var(--border, #dfe3e8); color: var(--muted, #707579); justify-content: space-between; }
    .message { color: var(--err, #d93025); }
    @media(max-width: 600px) { .mount, textarea { height: 380px; } .shortcut { display: none; } button { min-height: 32px; } }
  `;

  constructor() {
    super();
    this.language = 'html';
    this.placeholder = '';
    /** @type {SpecSummary | null} */
    this.spec = null;
    this.snippets = {};
    this.ready = false;
    this.message = '';
    this.position = 'Строка 1, столбец 1';
    this.buffer = '';
    this.loading = false;
    /** @type {import('../../vendor/code-editor.js').EditorView | undefined} */
    this.view = undefined;
    /** @type {typeof import('../../vendor/code-editor.js') | undefined} */
    this.cm = undefined;
    /** @type {IntersectionObserver | undefined} */
    this.observer = undefined;
    this.fallback = this.createFallback();
  }

  connectedCallback() {
    super.connectedCallback();
    this.updateComplete.then(() => {
      if (!this.isConnected) return;
      this.observer = new IntersectionObserver((entries) => { if (entries.some((e) => e.isIntersecting)) this.initialize(); });
      this.observer.observe(this);
    });
  }

  disconnectedCallback() {
    this.buffer = this.value;
    this.view?.destroy(); this.view = undefined; this.ready = false;
    this.observer?.disconnect();
    super.disconnectedCallback();
  }

  get value() { return this.view?.state.doc.toString() ?? this.buffer; }
  set value(value) {
    const next = String(value ?? '');
    // Lit's last rendered property can differ from a textarea edited by the user. Always sync external loads.
    const fallback = this.fallback;
    if (fallback && fallback.value !== next) fallback.value = next;
    if (next === this.value) return;
    this.buffer = next;
    this.message = '';
    if (this.view && this.cm) this.view.setState(this.editorState(next));
    this.requestUpdate();
  }

  async initialize() {
    if (this.view || this.loading) return;
    this.loading = true;
    try {
      this.cm = await (library ||= import('../../vendor/code-editor.js'));
      if (!this.isConnected) return;
      const fallback = /** @type {HTMLTextAreaElement} */ (this.renderRoot.querySelector('textarea'));
      const focused = /** @type {ShadowRoot} */ (this.renderRoot).activeElement === fallback;
      this.view = new this.cm.EditorView({ state: this.editorState(this.buffer),
        parent: /** @type {HTMLElement} */ (this.renderRoot.querySelector('.mount')), root: /** @type {ShadowRoot} */ (this.renderRoot) });
      this.ready = true;
      await this.updateComplete;
      if (!this.isConnected || !this.view) return;
      this.view.requestMeasure();
      if (focused) {
        this.view.dispatch({ selection: { anchor: fallback.selectionStart, head: fallback.selectionEnd } });
        this.view.focus();
      }
      this.observer?.disconnect();
    } catch {
      this.message = 'Подсказки временно недоступны. Текст можно редактировать и сохранять.';
    } finally { this.loading = false; }
  }

  updated(changed) {
    if (this.view && this.cm && ['snippets', 'language'].some((key) => changed.has(key))) {
      this.view.dispatch({ effects: this.cm.StateEffect.reconfigure.of(this.editorExtensions()) });
    }
  }

  resetHistory() {
    if (this.view) this.view.setState(this.editorState(this.value));
    else { this.fallback = this.createFallback(); this.requestUpdate(); }
  }

  createFallback() {
    return /** @type {HTMLTextAreaElement} */ (h('textarea', { value: this.buffer, spellcheck: 'false',
      oninput: (e) => { this.buffer = e.target.value; this.fireChange(); },
      onkeydown: (e) => { if (e.key === 'Tab' && !e.shiftKey) { e.preventDefault(); this.insertSnippet('  '); } },
    }));
  }

  editorState(doc) {
    if (!this.cm) throw new Error('Editor is not loaded');
    return this.cm.EditorState.create({ doc, extensions: this.editorExtensions() });
  }

  editorExtensions() {
    const cm = this.cm;
    if (!cm) throw new Error('Editor is not loaded');
    const tags = htmlTags(this.snippets);
    const htmlSupport = cm.html({ extraTags: tags });
    const snippets = snippetSource(this.language, this.snippets, (item, from, to) => {
      this.dispatchEvent(new CustomEvent('snippet', { detail: { item, from, to } }));
    });
    const completeHTML = cm.htmlCompletionSourceWith({ extraTags: tags });
    const sources = this.language === 'json' ? [jsonSource(cm, () => this.spec)]
      : [(context) => this.language !== 'markdown' || context.matchBefore(/<[^<>]*$/) ? completeHTML(context) : null, snippets];
    const language = this.language === 'json' ? cm.json() : this.language === 'markdown'
      ? cm.markdown({ base: cm.markdownLanguage, htmlTagLanguage: htmlSupport }) : htmlSupport;
    const palette = cm.EditorView.theme({
      '&': { height: '100%', backgroundColor: 'var(--panel, white)', color: 'var(--text, #17212b)', fontSize: '13px' },
      '&.cm-focused': { outline: 'none' },
      '.cm-scroller': { overflow: 'auto', fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, monospace', lineHeight: '1.6' },
      '.cm-content': { padding: '10px 0', caretColor: 'var(--text, #17212b)' },
      '.cm-line': { padding: '0 12px' },
      '.cm-gutters': { backgroundColor: 'var(--soft, #eef3f8)', color: 'var(--muted, #707579)', border: 'none' },
      '.cm-activeLine, .cm-activeLineGutter': { backgroundColor: 'var(--soft, #eef3f8)' },
      '.cm-cursor': { borderLeftColor: 'var(--text, #17212b)' },
      '.cm-selectionBackground, &.cm-focused .cm-selectionBackground': { backgroundColor: 'var(--chip, #d9eaff)' },
      '.cm-tooltip, .cm-panels': { backgroundColor: 'var(--panel, white)', color: 'var(--text, #17212b)', borderColor: 'var(--border, #dfe3e8)' },
      '.cm-tooltip-autocomplete ul li[aria-selected]': { backgroundColor: 'var(--accent, #3390ec)', color: '#fff' },
      '.cm-tooltip-autocomplete': { maxWidth: 'calc(100vw - 32px)' },
      '.cm-tooltip-autocomplete ul li': { overflow: 'hidden', textOverflow: 'ellipsis' },
      '.cm-search input': { background: 'var(--panel, white)', color: 'inherit', border: '1px solid var(--border, #dfe3e8)' },
      '.cm-button': { backgroundImage: 'none', backgroundColor: 'var(--soft, #eef3e8)', color: 'inherit', borderColor: 'var(--border, #dfe3e8)' },
    });
    return [
      language, palette, cm.EditorView.lineWrapping, cm.lineNumbers(), cm.highlightActiveLineGutter(),
      cm.drawSelection(), cm.dropCursor(), cm.highlightActiveLine(), cm.history(), cm.indentOnInput(),
      cm.bracketMatching(), cm.closeBrackets(), cm.foldGutter(), cm.highlightSelectionMatches(), cm.indentUnit.of('  '),
      cm.syntaxHighlighting(cm.HighlightStyle.define([
        { tag: [cm.tags.keyword, cm.tags.bool, cm.tags.null], color: 'light-dark(#7c3aad, #d5a3ff)' },
        { tag: [cm.tags.tagName, cm.tags.heading], color: 'light-dark(#176b9d, #79c5ff)' },
        { tag: [cm.tags.string, cm.tags.attributeValue, cm.tags.link], color: 'light-dark(#25713d, #8bd9a0)' },
        { tag: [cm.tags.attributeName, cm.tags.propertyName], color: 'light-dark(#8c4d14, #f2bd7d)' },
        { tag: cm.tags.number, color: 'light-dark(#a43a32, #ff9c93)' },
        { tag: [cm.tags.comment, cm.tags.meta], color: 'var(--muted, #707579)' },
        { tag: [cm.tags.strong, cm.tags.heading], fontWeight: 'bold' },
        { tag: cm.tags.emphasis, fontStyle: 'italic' },
        { tag: cm.tags.strikethrough, textDecoration: 'line-through' },
      ])),
      cm.autocompletion({ override: sources }),
      cm.keymap.of([{ key: 'Tab', run: cm.acceptCompletion }, ...cm.closeBracketsKeymap, ...cm.completionKeymap,
        ...cm.defaultKeymap, ...cm.historyKeymap, ...cm.searchKeymap, ...cm.foldKeymap, cm.indentWithTab]),
      cm.EditorView.contentAttributes.of({ 'aria-label': `Редактор ${this.language === 'markdown' ? 'Markdown' : this.language.toUpperCase()}`, spellcheck: 'false', autocapitalize: 'off' }),
      cm.EditorState.phrases.of({ 'Find': 'Найти', 'Replace': 'Заменить', 'next': 'Далее', 'previous': 'Назад', 'all': 'Все',
        'match case': 'Учитывать регистр', 'by word': 'Слова целиком', 'regexp': 'Регулярное выражение', 'replace': 'Заменить',
        'replace all': 'Заменить всё', 'close': 'Закрыть', 'Fold line': 'Свернуть строку', 'Unfold line': 'Развернуть строку' }),
      ...(this.language === 'json' ? [cm.linter(cm.jsonParseLinter()), cm.lintGutter()] : []),
      cm.EditorView.updateListener.of((update) => {
        if (update.docChanged) { this.buffer = update.state.doc.toString(); this.message = ''; this.fireChange(); }
        if (update.docChanged || update.selectionSet) {
          const at = update.state.selection.main.head;
          const line = update.state.doc.lineAt(at);
          this.position = `Строка ${line.number}, столбец ${at - line.from + 1}`;
        }
      }),
    ];
  }

  fireChange() { this.dispatchEvent(new CustomEvent('source-change', { detail: this.value })); }

  insertSnippet(snip, from, to) {
    const fallback = /** @type {HTMLTextAreaElement | null} */ (this.renderRoot.querySelector('textarea'));
    const range = this.view?.state.selection.main;
    const a = from ?? range?.from ?? fallback?.selectionStart ?? 0;
    const b = to ?? range?.to ?? fallback?.selectionEnd ?? a;
    const selected = from === undefined ? this.value.slice(a, b) : '';
    const text = snip.replace('$SEL$', selected);
    const caret = snip.includes('$SEL$') && !selected ? a + snip.indexOf('$SEL$') : a + text.length;
    if (this.view) {
      this.view.dispatch({ changes: { from: a, to: b, insert: text }, selection: { anchor: caret }, userEvent: 'input' });
      this.view.focus();
    } else if (fallback) {
      fallback.setRangeText(text, a, b, 'end'); fallback.setSelectionRange(caret, caret); fallback.focus();
      this.buffer = fallback.value; this.fireChange();
    }
  }

  suggest() { if (this.view && this.cm) { this.view.focus(); this.cm.startCompletion(this.view); } }
  search() { if (this.view && this.cm) this.cm.openSearchPanel(this.view); }
  format() {
    try {
      const text = JSON.stringify(JSON.parse(this.value), null, 2);
      if (this.view) this.view.dispatch({ changes: { from: 0, to: this.view.state.doc.length, insert: text }, userEvent: 'input' });
      else { this.value = text; this.fireChange(); }
    } catch { this.message = 'Сначала исправьте ошибку JSON — она отмечена в тексте.'; }
  }

  render() {
    this.fallback.hidden = this.ready;
    this.fallback.placeholder = this.placeholder;
    this.fallback.setAttribute('aria-label', `Редактор ${this.language}`);
    return html`<div class="frame">
      <div class="bar"><span class="language">${this.language === 'markdown' ? 'Markdown' : this.language.toUpperCase()}</span>
        <button type="button" ?disabled=${!this.ready} @click=${this.suggest} title="Показать подсказки (Ctrl+Space)">Подсказки <span class="shortcut">⌃Space</span></button>
        <button type="button" ?disabled=${!this.ready} @click=${this.search} title="Поиск и замена (Ctrl+F)">Найти</button>
        ${this.language === 'json' ? html`<button type="button" @click=${this.format}>Форматировать</button>` : ''}
      </div>
      <div class="mount" ?hidden=${!this.ready}></div>
      ${this.fallback}
      <div class="status"><span>${this.position}</span><span>${this.language === 'markdown' ? '/ в начале строки — сниппеты' : 'Ctrl+Space — подсказки'}</span>
        ${this.message ? html`<span class="message" role="status">${this.message}</span>` : ''}</div>
    </div>`;
  }
}

customElements.define('rs-code-editor', RsCodeEditor);
