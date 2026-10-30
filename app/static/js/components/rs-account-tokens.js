import { css, html, LitElement } from '../../vendor/lit-core.min.js';
import { dateLocale, tr } from '../i18n.js';

// Presentation only. The controller owns API calls; credentials are never persisted by the browser.
export class RsAccountTokens extends LitElement {
  static properties = {
    tokens: { type: Array }, busy: { type: Boolean }, error: { type: String },
    secret: { type: String }, endpoint: { type: String }, name: { type: String },
    level: { type: String }, days: { type: String }, confirming: { type: String }, copied: { type: String },
  };
  static styles = css`
    :host { display: block; min-width: 0; color: inherit; }
    * { box-sizing: border-box; }
    p { line-height: 1.5; overflow-wrap: anywhere; }
    .muted, small { opacity: .75; }
    .fields { display: grid; grid-template-columns: 1fr 1fr; gap: 14px; margin: 14px 0; }
    label { display: block; font-weight: 600; margin-bottom: 6px; }
    input, select, textarea { width: 100%; min-width: 0; padding: 10px; border: 1px solid #8090a0;
      border-radius: 8px; background: var(--panel, #fff); color: inherit; font: inherit; }
    textarea { font-family: monospace; font-size: 12px; resize: vertical; overflow-wrap: anywhere; }
    input:focus-visible, select:focus-visible, textarea:focus-visible, button:focus-visible { outline: 2px solid #3390ec; outline-offset: 2px; }
    button { font: inherit; padding: 10px 14px; border-radius: 8px; cursor: pointer; border: 1px solid #8090a0;
      background: transparent; color: inherit; }
    button.primary { background: #2675bd; border-color: #2675bd; color: white; }
    button:disabled { opacity: .5; cursor: default; }
    .actions { display: flex; flex-wrap: wrap; gap: 8px; margin: 14px 0; }
    .new-secret { border: 2px solid #3390ec; border-radius: 12px; padding: 16px; margin: 16px 0; }
    .error { color: #d64d4d; }
    ul { padding: 0; list-style: none; }
    li { border-top: 1px solid #8090a066; padding: 14px 0; overflow-wrap: anywhere; }
    .meta { display: flex; gap: 8px; flex-wrap: wrap; margin-top: 6px; }
    .badge { font-size: 12px; padding: 3px 8px; border: 1px solid #8090a066; border-radius: 6px; }
    .dates { font-size: 12px; margin-bottom: 0; }
    code { overflow-wrap: anywhere; }
    @media (max-width: 540px) { .fields { grid-template-columns: 1fr; } .new-secret { padding: 12px; } }
  `;
  constructor() {
    super();
    /** @type {AccountToken[]} */
    this.tokens = [];
    this.busy = false; this.error = ''; this.secret = ''; this.endpoint = '';
    this.name = ''; this.level = 'draft'; this.days = '90'; this.confirming = ''; this.copied = '';
  }
  disconnectedCallback() {
    super.disconnectedCallback();
    this.secret = ''; this.copied = '';
  }
  request(kind, detail = {}) {
    if (this.busy) return;
    this.dispatchEvent(new CustomEvent('token-request', { detail: { kind, ...detail }, bubbles: true, composed: true }));
  }
  roleLabel(level) {
    return { read: tr('Чтение'), draft: tr('Черновики'), publish: tr('Публикация') }[level] || level;
  }
  date(at) { return at ? new Date(at * 1000).toLocaleString(dateLocale) : tr('Ещё не использовался'); }
  async copy(text, label) {
    try { await navigator.clipboard.writeText(text); this.copied = label; }
    catch { this.copied = tr('Не удалось скопировать. Выделите текст и скопируйте вручную.'); }
  }
  config() {
    return JSON.stringify({ mcpServers: { richpost: { type: 'http', url: this.endpoint, headers: { Authorization: 'Bearer ' + this.secret } } } }, null, 2);
  }
  render() {
    const description = {
      read: tr('Читает ваши черновики, посты и каналы. Не изменяет данные.'),
      draft: tr('Готовит черновики, управляет файлами и отправляет предпросмотр вам в личку. Не публикует в каналы.'),
      publish: tr('Все права на черновики, а также публикация, расписание, изменение и удаление постов в ваших каналах.'),
    }[this.level];
    return html`
      <p class="muted">${tr('Отдельный токен для каждого агента или приложения. Доступ ограничен вашей учёткой и выбранной ролью.')}</p>
      <p><strong>${tr('Адрес MCP:')}</strong> <code>${this.endpoint}</code></p>
      <p class="muted">${tr('Подключение по HTTP с заголовком Authorization: Bearer <токен>. Для REST API подходит тот же токен.')}</p>
      <div role="status" aria-live="polite" class="error">${this.error}</div>
      ${this.secret ? html`
        <section class="new-secret" aria-label=${tr('Новый токен')}>
          <h4>${tr('Сохраните токен сейчас')}</h4>
          <p>${tr('Он показан один раз. После закрытия восстановить его нельзя — можно создать новый.')}</p>
          <label for="secret">${tr('Токен')}</label>
          <input id="secret" readonly autocomplete="off" spellcheck="false" .value=${this.secret} @focus=${(e) => e.target.select()}>
          <div class="actions"><button @click=${() => this.copy(this.secret, tr('Токен скопирован'))}>${tr('Скопировать токен')}</button></div>
          <label for="mcp-config">${tr('Конфигурация MCP (JSON)')}</label>
          <textarea id="mcp-config" rows="9" readonly spellcheck="false" .value=${this.config()} @focus=${(e) => e.target.select()}></textarea>
          <small>${tr('Конфигурация содержит секрет. Сохраните её в настройках MCP-клиента и не публикуйте.')}</small>
          <div class="actions">
            <button @click=${() => this.copy(this.config(), tr('Конфигурация скопирована'))}>${tr('Скопировать конфигурацию')}</button>
            <button class="primary" @click=${() => { this.secret = ''; this.copied = ''; }}>${tr('Я сохранил токен')}</button>
          </div><p role="status">${this.copied}</p>
        </section>` : html`
        <form @submit=${(e) => { e.preventDefault(); if (this.name.trim()) this.request('create', { name: this.name.trim(), level: this.level, expires_in_days: this.days === 'none' ? null : Number(this.days) }); }}>
          <label for="token-name">${tr('Название подключения')}</label>
          <input id="token-name" required maxlength="80" autocomplete="off" placeholder="Claude laptop" .value=${this.name}
            ?disabled=${this.busy} @input=${(e) => { this.name = e.target.value; }}>
          <div class="fields">
            <div><label for="token-role">${tr('Роль')}</label>
              <select id="token-role" .value=${this.level} ?disabled=${this.busy} aria-describedby="role-description" @change=${(e) => { this.level = e.target.value; }}>
                <option value="read">${tr('Чтение')}</option><option value="draft">${tr('Черновики')}</option><option value="publish">${tr('Публикация')}</option>
              </select></div>
            <div><label for="token-expiry">${tr('Срок действия')}</label>
              <select id="token-expiry" .value=${this.days} ?disabled=${this.busy} @change=${(e) => { this.days = e.target.value; }}>
                <option value="7">${tr('7 дней')}</option><option value="30">${tr('30 дней')}</option>
                <option value="90">${tr('90 дней')}</option><option value="365">${tr('365 дней')}</option><option value="none">${tr('Без срока')}</option>
              </select></div>
          </div>
          <p id="role-description">${description}</p>
          <button class="primary" type="submit" ?disabled=${this.busy || !this.name.trim()}>${this.busy ? tr('Ожидание…') : tr('Создать токен')}</button>
        </form>`}
      <h4>${tr('Ваши токены')}</h4>
      <p class="muted small">${tr('Отзыв запрещает новые запросы. Уже принятые операции и запланированные посты сохраняются.')}</p>
      ${!this.tokens.length ? html`<p>${this.busy ? tr('Загрузка…') : tr('Токенов пока нет.')}</p>` : ''}
      <ul>${this.tokens.map((token) => {
        const expired = token.expires_at !== null && token.expires_at <= Date.now() / 1000;
        const active = !token.revoked_at && !expired;
        return html`<li>
          <strong>${token.name}</strong><div class="meta"><span class="badge">${this.roleLabel(token.level)}</span>
          <span class="badge">${token.revoked_at ? tr('Отозван') : expired ? tr('Истёк') : tr('Действует')}</span></div>
          <p class="dates">${tr('Создан:')} ${this.date(token.created_at)}<br>
            ${tr('Последнее использование:')} ${this.date(token.last_used_at)}<br>
            ${tr('Срок действия:')} ${token.expires_at ? this.date(token.expires_at) : tr('Без срока')}</p>
          ${active ? (this.confirming === token.id ? html`
            <p>${tr`Отозвать токен «${token.name}»? Подключение потеряет доступ.`}</p>
            <div class="actions"><button ?disabled=${this.busy} @click=${() => this.request('revoke', { id: token.id })}>${tr('Да, отозвать')}</button>
              <button ?disabled=${this.busy} @click=${() => { this.confirming = ''; }}>${tr('Отмена')}</button></div>` : html`
            <div class="actions"><button ?disabled=${this.busy} @click=${() => { this.confirming = token.id; }}>${tr('Отозвать')}</button></div>`) : ''}
        </li>`;
      })}</ul>
      <button ?disabled=${this.busy} @click=${() => this.request('refresh')}>${tr('Обновить список')}</button>`;
  }
}
customElements.define('rs-account-tokens', RsAccountTokens);
