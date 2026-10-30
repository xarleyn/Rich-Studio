import { tr, locale } from './i18n.js';
// Public Telegram login and the signed-in account. Tokens stay in HttpOnly cookies.
import { api, login, probe, setPublicAuth } from './api.js';
import { closeDialog, dialogs, openDialog } from './dialogs.js';
import { $, act, h, setStoreScope } from './dom.js';
import { showAccountTokens } from './account-tokens.js';

/** @type {{telegram: boolean, bot_username: string | null}} */
let config;

async function showLogin() {
  if ($('#login-screen')) return;
  for (const dialog of [...dialogs]) closeDialog(dialog);
  document.body.classList.add('signed-out');
  const widget = h('div', { class: 'telegram-login', 'aria-live': 'polite', text: tr('Подключаем Telegram…') });
  const screen = h('section', { id: 'login-screen', class: 'login-screen' },
    h('div', { class: 'login-card' },
      h('div', { class: 'login-brand', text: 'RichPost' }),
      h('h1', { text: tr('Ваши каналы. Ваши посты.') }),
      h('p', { class: 'muted', text: tr('Создавайте посты с медиа, кнопками и красивым оформлением. Публикуйте сразу или по расписанию.') }),
      h('ol', { class: 'login-steps' },
        h('li', {}, h('b', { text: tr('Войдите через Telegram') }), h('span', { text: tr('Отдельная учётка с вашими черновиками и файлами.') })),
        h('li', {}, h('b', { text: tr('Подключите канал') }), h('span', { text: tr`Добавьте @${config.bot_username} администратором с правом публикации.` })),
        h('li', {}, h('b', { text: tr('Создайте первый пост') }), h('span', { text: tr('Предпросмотр покажет результат ещё до отправки.') }))),
      widget,
      h('p', { class: 'muted small', text: tr('Подтверждение входа откроется в Telegram. Пароль и номер телефона в студии не нужны.') }),
      h('button', { class: 'ghost small-btn', text: tr('Вход владельца сервиса'), onclick: () => {
        const key = h('input', { type: 'password', placeholder: tr('Ключ или токен'), autocomplete: 'off' });
        openDialog([h('h3', { text: tr('Вход владельца') }), key], [h('button', { text: tr('Войти'), type: 'button', onclick: () => act(async () => {
          if (!await login(key.value)) throw new Error(tr('Ключ или токен не подошёл'));
          closeDialog(); location.reload();
        }) })]);
      } })));
  document.body.prepend(screen);
  screen.querySelector('.login-card').prepend($('.locale-control'));
  try {
    const start = await api('POST', '/api/auth/telegram/start', {});
    const script = document.createElement('script');
    script.src = 'https://telegram.org/js/telegram-widget.js?22';
    script.async = true;
    script.setAttribute('data-telegram-login', start.bot_username);
    script.setAttribute('data-size', 'large');
    script.setAttribute('data-lang', locale);
    script.setAttribute('data-userpic', 'false');
    script.setAttribute('data-request-access', 'write');
    script.setAttribute('data-auth-url', start.auth_url);
    script.onerror = () => widget.replaceChildren(h('p', { class: 'err', text: tr('Telegram не загрузился. Проверьте соединение и обновите страницу.') }));
    widget.replaceChildren(script);
  } catch (e) { widget.replaceChildren(h('p', { class: 'err', text: e.message })); }
}

/** Return false until the user has a session: the editor must not start loading shared data. */
export async function prepareAccount() {
  config = await api('GET', '/api/auth/config');
  if (!config.telegram) return true;
  setPublicAuth(true);
  document.addEventListener('rs-login-required', () => showLogin());
  const response = await probe('/api/session');
  if (response.status === 401) { await showLogin(); return false; }
  if (!response.ok) throw new Error(tr('Не удалось проверить вход'));
  const session = await response.json();
  const user = session.user;
  if (user) setStoreScope('tg.' + user.telegram_id);
  const name = user ? [user.first_name, user.last_name].filter(Boolean).join(' ') : tr('Владелец');
  const button = $('#account-menu');
  button.hidden = false;
  button.textContent = name;
  button.onclick = () => openDialog([
    h('h3', { text: name }),
    h('p', { class: 'muted', text: user ? (user.username ? '@' + user.username : 'Telegram ID: ' + user.telegram_id) : tr('Доступ владельца сервиса') }),
    h('a', { class: 'btn', href: `https://t.me/${config.bot_username}`, target: '_blank', rel: 'noopener', text: tr('Открыть бота') }),
    h('p', { class: 'muted small', text: tr('Нажмите «Запустить» в боте, чтобы получать предпросмотры в личные сообщения.') }),
    ...(user ? [h('button', { type: 'button', text: tr('Токены API и MCP'), onclick: () => {
      closeDialog(); showAccountTokens();
    } })] : []),
  ], [h('button', { class: 'ghost', value: 'cancel', text: tr('Закрыть') }), h('button', { type: 'button', text: tr('Выйти'), onclick: () => act(async () => {
    await api('DELETE', '/api/session'); location.reload();
  }) })]);
  if (user) { $('#export').hidden = true; $('#spec-refresh').hidden = true; }
  return true;
}
