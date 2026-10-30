import { css, html, LitElement } from '../../vendor/lit-core.min.js';
import { clone } from '../dom.js';
import { tr } from '../i18n.js';
import { FIELD_LABELS } from '../labels.js';

/** @typedef {{key:string,label:string,type:string,required:boolean,default:string}} TemplateField */
/** @typedef {{id:string,title:string,payload:Json,fields:TemplateField[],version:number}} PostTemplate */

const variable = /\{\{([A-Za-z][A-Za-z0-9_]{0,31})\}\}/g;

/** Strings from the post itself, without assuming a fixed list of block types. */
function strings(value, path = /** @type {string[]} */ ([]), result = /** @type {{path:string[], value:string}[]} */ ([])) {
  if (typeof value === 'string') result.push({ path, value });
  else if (value && typeof value === 'object') Object.entries(value).forEach(([key, child]) => strings(child, [...path, key], result));
  return result;
}

// Presentation only. The controller supplies the source and performs persistence and navigation.
export class RsTemplates extends LitElement {
  static properties = {
    templates: { type: Array }, busy: { type: Boolean }, error: { type: String }, view: { type: String },
    current: { type: Object }, title: { type: String }, fields: { type: Array }, post: { type: Object },
    values: { type: Object }, selected: { type: String }, confirming: { type: String },
  };
  static styles = css`
    :host { display:block; color:inherit; min-width:0; }
    * { box-sizing:border-box; }
    p { line-height:1.5; overflow-wrap:anywhere; }
    .muted, small { opacity:.75; }
    label { display:block; font-weight:600; margin:12px 0 6px; }
    input, select, textarea { width:100%; min-width:0; padding:10px; border:1px solid #8090a0;
      border-radius:8px; background:var(--panel,#fff); color:inherit; font:inherit; }
    textarea { resize:vertical; min-height:76px; }
    button { font:inherit; padding:9px 12px; border-radius:8px; border:1px solid #8090a0;
      background:transparent; color:inherit; cursor:pointer; }
    button.primary { background:#2675bd; border-color:#2675bd; color:#fff; }
    button:disabled { opacity:.5; cursor:default; }
    :focus-visible { outline:2px solid #3390ec; outline-offset:2px; }
    .actions { display:flex; flex-wrap:wrap; gap:8px; margin:14px 0; }
    .error { color:#d64d4d; }
    ul { padding:0; list-style:none; }
    li, fieldset { padding:14px 0; border:0; border-top:1px solid #8090a066; margin:0; min-width:0; }
    li strong, code { overflow-wrap:anywhere; }
    .columns { display:grid; grid-template-columns:1fr 1fr; gap:12px; }
    .check { display:flex; align-items:center; gap:8px; font-weight:400; }
    .check input { width:auto; }
    @media(max-width:540px) { .columns { grid-template-columns:1fr; gap:0; } }
  `;
  constructor() {
    super();
    /** @type {PostTemplate[]} */ this.templates = [];
    /** @type {PostTemplate | null} */ this.current = null;
    /** @type {TemplateField[]} */ this.fields = [];
    /** @type {Json} */ this.post = {};
    /** @type {Record<string,string>} */ this.values = {};
    this.busy = false; this.error = ''; this.view = 'list'; this.title = ''; this.selected = ''; this.confirming = '';
  }
  request(kind, detail = {}) {
    if (!this.busy) this.dispatchEvent(new CustomEvent('template-request', { detail: { kind, ...detail }, bubbles: true, composed: true }));
  }
  editTemplate(template) {
    this.current = template.id ? template : null;
    this.title = template.title;
    this.post = clone(template.payload);
    const found = [...new Set(strings(this.post).flatMap(({ value }) => [...value.matchAll(variable)].map((m) => m[1])))];
    this.fields = found.map((key) => clone(template.fields?.find((field) => field.key === key) || { key, label: key, type: 'text', required: true, default: '' }));
    this.selected = ''; this.error = ''; this.view = 'edit';
  }
  fillTemplate(template) {
    this.current = template;
    this.title = template.title;
    this.values = Object.fromEntries(template.fields.map((field) => [field.key, field.default]));
    this.error = ''; this.view = 'fill';
  }
  candidates() {
    return strings(this.post).filter(({ path, value }) => value && !/\{\{[A-Za-z]/.test(value) && !['type', 'id', 'html', 'markdown'].includes(path.at(-1) || ''));
  }
  addField() {
    const source = this.candidates()[Number(this.selected)];
    if (!source || this.selected === '' || this.fields.length >= 50) return;
    let number = this.fields.length + 1;
    while (this.fields.some((field) => field.key === 'field_' + number)) number++;
    const key = 'field_' + number;
    const post = clone(this.post);
    let target = post;
    for (const part of source.path.slice(0, -1)) target = target[part];
    const property = source.path.at(-1) || '';
    target[property] = '{{' + key + '}}';
    this.post = post;
    this.fields = [...this.fields, { key, label: FIELD_LABELS[property] || property, type: property === 'url' ? 'url' : source.value.includes('\n') ? 'multiline' : 'text', required: true, default: source.value }];
    this.selected = '';
  }
  updateField(index, property, value) {
    this.fields = this.fields.map((field, i) => i === index ? { ...field, [property]: value } : field);
  }
  renderList() {
    return html`
      <p class="muted">${tr('Шаблон хранит оформление и постоянный текст. При создании поста заполните только заданные поля.')}</p>
      <button class="primary" ?disabled=${this.busy} @click=${() => this.request('new')}>${tr('Сохранить текущий пост как шаблон')}</button>
      ${!this.templates.length && !this.busy ? html`<p>${tr('Шаблонов пока нет. Начните с поста в редакторе.')}</p>` : ''}
      <ul>${this.templates.map((template) => html`<li>
        <strong>${template.title}</strong><p class="muted">${template.fields.map((field) => field.label).join(' · ') || tr('Без заполняемых полей')}</p>
        <div class="actions">
          <button class="primary" ?disabled=${this.busy} @click=${() => this.request('fill', { id: template.id })}>${tr('Создать по шаблону')}</button>
          <button ?disabled=${this.busy} @click=${() => this.request('edit', { id: template.id })}>${tr('Изменить')}</button>
          <button ?disabled=${this.busy} @click=${() => { this.confirming = template.id; }}>${tr('Удалить')}</button>
        </div>
        ${this.confirming === template.id ? html`<p>${tr('Удалить шаблон? Созданные черновики останутся.')}</p><div class="actions">
          <button ?disabled=${this.busy} @click=${() => this.request('delete', { id: template.id, version: template.version })}>${tr('Да, удалить')}</button>
          <button ?disabled=${this.busy} @click=${() => { this.confirming = ''; }}>${tr('Отмена')}</button></div>` : ''}
      </li>`)}</ul>`;
  }
  renderEdit() {
    const candidates = this.candidates();
    return html`<form @submit=${(e) => { e.preventDefault(); this.request('save', { id: this.current?.id, version: this.current?.version, title: this.title, post: this.post, fields: this.fields }); }}>
      <label for="template-name">${tr('Название шаблона')}</label>
      <input id="template-name" required maxlength="128" .value=${this.title} @input=${(e) => { this.title = e.target.value; }}>
      ${this.current ? html`<div class="actions"><button type="button" ?disabled=${this.busy} @click=${() => this.request('replace-source', { id: this.current?.id, version: this.current?.version, title: this.title, fields: this.fields })}>${tr('Взять оформление и текст из текущего поста')}</button></div>` : ''}
      <p class="muted">${tr('Выберите текст или ссылку из поста и превратите в поле. Для части строки заранее впишите {{name}} в редакторе; поле появится здесь.')}</p>
      ${candidates.length ? html`<label for="template-source">${tr('Что заполнять в новом посте')}</label>
        <select id="template-source" .value=${this.selected} @change=${(e) => { this.selected = e.target.value; }}>
          <option value="">${tr('Выберите текст или ссылку')}</option>
          ${candidates.map(({ path, value }, i) => html`<option value=${String(i)}>${FIELD_LABELS[path.at(-1) || ''] || path.at(-1)}: ${value.slice(0, 100)}</option>`)}
        </select><div class="actions"><button type="button" ?disabled=${this.selected === '' || this.busy || this.fields.length >= 50} @click=${() => this.addField()}>${tr('Добавить поле')}</button></div>` : ''}
      ${this.fields.map((field, index) => html`<fieldset>
        <small><code>${'{{' + field.key + '}}'}</code></small>
        <div class="columns"><div><label for=${'label-' + index}>${tr('Подпись поля')}</label>
          <input id=${'label-' + index} required maxlength="100" .value=${field.label} @input=${(e) => this.updateField(index, 'label', e.target.value)}></div>
          <div><label for=${'type-' + index}>${tr('Тип поля')}</label><select id=${'type-' + index} .value=${field.type} @change=${(e) => this.updateField(index, 'type', e.target.value)}>
            <option value="text">${tr('Текст')}</option><option value="multiline">${tr('Несколько строк')}</option><option value="url">${tr('Ссылка')}</option>
          </select></div></div>
        <label for=${'default-' + index}>${tr('Значение по умолчанию')}</label>
        <textarea id=${'default-' + index} maxlength="10000" .value=${field.default} @input=${(e) => this.updateField(index, 'default', e.target.value)}></textarea>
        <label class="check"><input type="checkbox" .checked=${field.required} @change=${(e) => this.updateField(index, 'required', e.target.checked)}>${tr('Обязательное поле')}</label>
      </fieldset>`)}
      ${!this.fields.length ? html`<p class="muted">${tr('Без полей шаблон создаёт копию поста. Поля можно добавить выше.')}</p>` : ''}
      <div class="actions"><button class="primary" type="submit" ?disabled=${this.busy}>${tr('Сохранить шаблон')}</button>
        <button type="button" ?disabled=${this.busy} @click=${() => { this.view = 'list'; this.error = ''; }}>${tr('Назад к шаблонам')}</button></div>
    </form>`;
  }
  renderFill() {
    const template = this.current;
    if (!template) return '';
    return html`<form @submit=${(e) => { e.preventDefault(); this.request('create-draft', { id: template.id, version: template.version, title: this.title, values: this.values }); }}>
      <h4>${template.title}</h4><p class="muted">${tr('После заполнения откроется новый черновик. Вы сможете проверить и изменить его перед публикацией.')}</p>
      <label for="new-draft-title">${tr('Название черновика')}</label><input id="new-draft-title" maxlength="128" .value=${this.title} @input=${(e) => { this.title = e.target.value; }}>
      ${template.fields.map((field, index) => html`<label for=${'value-' + index}>${field.label}${field.required ? ' *' : ''}</label>
        ${field.type === 'multiline' ? html`<textarea id=${'value-' + index} ?required=${field.required} maxlength="10000" .value=${this.values[field.key] || ''} @input=${(e) => { this.values = { ...this.values, [field.key]: e.target.value }; }}></textarea>`
          : html`<input id=${'value-' + index} type=${field.type === 'url' ? 'url' : 'text'} ?required=${field.required} maxlength="10000" .value=${this.values[field.key] || ''} @input=${(e) => { this.values = { ...this.values, [field.key]: e.target.value }; }}>`}`)}
      <div class="actions"><button class="primary" type="submit" ?disabled=${this.busy}>${tr('Создать черновик')}</button>
        <button type="button" ?disabled=${this.busy} @click=${() => { this.view = 'list'; this.error = ''; }}>${tr('Назад к шаблонам')}</button></div>
    </form>`;
  }
  render() {
    return html`<div role="status" aria-live="polite" class="error">${this.error}</div>
      ${this.view === 'edit' ? this.renderEdit() : this.view === 'fill' ? this.renderFill() : this.renderList()}`;
  }
}
customElements.define('rs-templates', RsTemplates);
