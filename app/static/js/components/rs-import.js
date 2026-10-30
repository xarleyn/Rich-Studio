import { tr } from '../i18n.js';
// Import form owns presentation only; requests and editor changes stay in the controller.
import { css, html, LitElement } from '../../vendor/lit-core.min.js';
import { parseTelegramLink } from '../import-links.js';

export class RsImport extends LitElement {
  static properties = {
    url: { type: String }, busy: { type: Boolean }, error: { type: String },
    message: { type: String }, forwardAvailable: { type: Boolean }, botUrl: { type: String },
  };
  static styles = css`
    :host { display: block; color: inherit; min-width: 0; }
    label { display: block; margin-bottom: 8px; font-weight: 600; }
    input { box-sizing: border-box; width: 100%; padding: 12px; border: 1px solid #8090a0;
      border-radius: 8px; background: transparent; color: inherit; font: inherit; }
    p { line-height: 1.5; overflow-wrap: anywhere; }
    .error { color: #d64d4d; }
    .actions { display: flex; flex-wrap: wrap; gap: 10px; margin-top: 16px; }
    button, a { font: inherit; padding: 10px 14px; border-radius: 8px; cursor: pointer; }
    button { background: #3390ec; color: white; border: 0; }
    button.secondary { background: transparent; color: inherit; border: 1px solid #8090a0; }
    button:disabled { opacity: .5; cursor: default; }
    a { color: #3390ec; }
  `;
  constructor() {
    super();
    this.url = ''; this.busy = false; this.error = ''; this.message = '';
    this.forwardAvailable = false; this.botUrl = '';
  }
  validation() {
    if (!this.url.trim()) return '';
    try { parseTelegramLink(this.url); return ''; } catch (e) { return e.message; }
  }
  submit(kind) {
    if (this.busy || !this.url.trim() || this.validation()) return;
    this.dispatchEvent(new CustomEvent('import-request', { detail: { kind, url: parseTelegramLink(this.url).url }, bubbles: true, composed: true }));
  }
  render() {
    const invalid = this.validation();
    return html`
      <label for="url">${tr("Ссылка на сообщение")}</label>
      <input id="url" type="text" inputmode="url" placeholder="https://t.me/simple_agi/203"
        .value=${this.url} ?disabled=${this.busy} aria-describedby="import-feedback"
        @input=${(e) => { this.url = e.target.value; this.error = ''; }}
        @keydown=${(e) => { if (e.key === 'Enter') { e.preventDefault(); this.submit('link'); } }}>
      <p>${tr("Создадим новый черновик с визуальными блоками, HTML и Markdown. Текущий пост сохранится отдельно.")}</p>
      <p id="import-feedback" role="status" class=${invalid || this.error ? 'error' : ''}>${invalid || this.error || this.message}</p>
      ${this.botUrl ? html`<p>${tr("Откройте бота, нажмите Start, затем перешлите нужное сообщение. Ожидание действует 10 минут.")}</p>
        <a href=${this.botUrl} target="_blank" rel="noopener noreferrer">${tr("Открыть бота Telegram ↗")}</a>` : ''}
      <div class="actions">
        <button ?disabled=${this.busy || !this.url.trim() || !!invalid} @click=${() => this.submit('link')}>${this.busy ? tr('Ожидание…') : tr('Импортировать')}</button>
        ${this.forwardAvailable ? html`<button class="secondary" ?disabled=${this.busy || !this.url.trim() || !!invalid}
          @click=${() => this.submit('forward')}>${tr("Через пересылку боту")}</button>` : ''}
      </div>`;
  }
}
customElements.define('rs-import', RsImport);
