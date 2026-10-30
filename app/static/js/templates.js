import { api } from './api.js';
import { closeDialog, openDialog } from './dialogs.js';
import { $, clone, h, toast } from './dom.js';
import { openDraft, saveDraft } from './drafts.js';
import { buildPost } from './editor.js';
import { tr } from './i18n.js';
import { dirty, S } from './state.js';
import './components/rs-templates.js';

export function showTemplates() {
  const panel = /** @type {import('./components/rs-templates.js').RsTemplates} */ (document.createElement('rs-templates'));
  const dialog = openDialog([h('h3', { text: tr('Шаблоны постов') }), panel]);
  const refresh = async () => {
    const result = await api('GET', '/api/templates');
    if (dialog.open) panel.templates = result;
  };
  const run = async (action) => {
    if (panel.busy) return;
    panel.busy = true; panel.error = '';
    try { await action(); }
    catch (e) { if (dialog.open) panel.error = e.message; }
    finally { panel.busy = false; }
  };
  panel.addEventListener('template-request', (event) => run(async () => {
    const { kind, id, ...body } = /** @type {CustomEvent} */ (event).detail;
    if (kind === 'new' || kind === 'replace-source') {
      if (S.pendingJson !== null) throw new Error(tr('Сначала примените правки JSON, затем сохраните пост как шаблон.'));
      panel.editTemplate({ id, version: body.version, title: body.title ?? $('#title').value ?? '', payload: clone(buildPost()), fields: body.fields || [] });
    } else if (kind === 'fill' || kind === 'edit') {
      const template = await api('GET', '/api/templates/' + encodeURIComponent(id));
      if (!dialog.open) return;
      if (kind === 'fill') panel.fillTemplate(template); else panel.editTemplate(template);
    } else if (kind === 'save') {
      await api(id ? 'PUT' : 'POST', '/api/templates' + (id ? '/' + encodeURIComponent(id) : ''), body);
      if (dialog.open) { panel.view = 'list'; await refresh(); }
      toast(tr('Шаблон сохранён'), 'ok');
    } else if (kind === 'delete') {
      await api('DELETE', '/api/templates/' + encodeURIComponent(id) + '?version=' + body.version);
      panel.confirming = ''; await refresh();
    } else if (kind === 'create-draft') {
      if (S.conflict) throw new Error(tr('Сначала разрешите конфликт сохранения текущего черновика.'));
      if (dirty.value && !(await saveDraft(true))) throw new Error(tr('Сначала сохраните текущий черновик.'));
      const draft = await api('POST', '/api/templates/' + encodeURIComponent(id) + '/drafts', body);
      if (!dialog.open) { toast(tr('Черновик создан. Откройте его из списка черновиков.'), 'ok'); return; }
      await openDraft(draft.id);
      closeDialog(dialog);
      toast(tr('Черновик создан по шаблону'), 'ok');
    }
  }));
  run(refresh);
}
