import { api } from './api.js';
import { openDialog } from './dialogs.js';
import { h } from './dom.js';
import { tr } from './i18n.js';
import './components/rs-account-tokens.js';

export function showAccountTokens() {
  const panel = /** @type {import('./components/rs-account-tokens.js').RsAccountTokens} */ (document.createElement('rs-account-tokens'));
  const dialog = openDialog([h('h3', { text: tr('Токены API и MCP') }), panel]);
  const refresh = async () => {
    const result = /** @type {AccountTokensResponse} */ (await api('GET', '/api/account/tokens'));
    if (dialog.open) { panel.tokens = result.tokens; panel.endpoint = result.mcp_url; }
  };
  const run = async (action) => {
    if (panel.busy) return;
    panel.busy = true; panel.error = '';
    try { await action(); }
    catch (e) { if (dialog.open) panel.error = e.message; }
    finally { panel.busy = false; }
  };
  panel.addEventListener('token-request', (event) => run(async () => {
    const { kind, id, ...body } = /** @type {CustomEvent} */ (event).detail;
    if (kind === 'create') {
      const result = /** @type {AccountTokenCreated} */ (await api('POST', '/api/account/tokens', body));
      if (!dialog.open) return; // never redisplay a secret after its dialog was closed
      panel.secret = result.secret;
      panel.name = ''; panel.copied = '';
      panel.tokens = [result.token, ...panel.tokens];
      await panel.updateComplete;
      panel.renderRoot.querySelector('input')?.focus();
    } else if (kind === 'revoke') {
      await api('DELETE', '/api/account/tokens/' + encodeURIComponent(id));
      panel.confirming = '';
      await refresh();
    } else await refresh();
  }));
  run(refresh);
}
