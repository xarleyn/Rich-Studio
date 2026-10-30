// pin + first comment + callback answer, then a click arrives through getUpdates
/** @param {import('./harness.mjs').Ctx} ctx */
export default async function ({ page, check, mockLog, push, applyJson }) {
  await applyJson({ rich_message: { html: '<p>Голосуем</p>' }, reply_markup: { inline_keyboard: [[{ text: '👍', callback_data: 'like' }]] } });
  await page.click('#panel-kb summary');
  await page.fill('.cb-answer input[type=text]', 'Спасибо, учтено');
  await page.click('#panel-after summary');
  await page.check('#after input[type=checkbox]');
  await page.fill('#comment-src', '<p>Обсуждение тут 👇</p>');
  await page.locator('#preview .tg-comment-label').waitFor();
  const commentShown = await page.locator('#preview .tg-comment-label').count();
  await Promise.all([
    page.waitForResponse((response) => response.url().endsWith('/api/send') && response.request().method() === 'POST' && response.ok()),
    page.click('#btn-publish'),
  ]);
  let log = await mockLog();
  const sentRich = log.filter((c) => c.method === 'sendRichMessage').pop();
  const pinned = log.filter((c) => c.method === 'pinChatMessage').pop();
  await push({ callback_query: { id: 'cq-e2e', data: 'like', message: { message_id: 1, chat: { id: -1001 } } } }); // unknown message: answered, not counted
  const posts = await page.evaluate(() => fetch('/api/posts').then((r) => r.json()));
  const mine = posts.find((p) => p.comment_status === 'waiting');
  await push({ callback_query: { id: 'cq-e2e2', data: 'like', message: { message_id: mine && (await page.evaluate((id) => fetch('/api/posts/' + id).then((r) => r.json()), mine.id)).message_id, chat: { id: -1001 } } } });
  let answer;
  const deadline = Date.now() + 15000;
  while (!answer && Date.now() < deadline) {
    log = await mockLog();
    answer = log.find((c) => c.method === 'answerCallbackQuery' && c.body.callback_query_id === 'cq-e2e2');
    if (!answer) await new Promise((resolve) => setTimeout(resolve, 100));
  }
  await Promise.all([
    page.waitForResponse((response) => new URL(response.url()).pathname === '/api/posts' && response.ok()),
    page.click('#side-tabs [data-tab=posts]'),
  ]);
  const row = page.locator('#posts li').filter({ hasText: 'Голосуем' });
  await row.waitFor();
  const statsText = await row.textContent();
  check('Pin, first comment and callback answer work end to end',
    commentShown === 1 && !sentRich.body.pin && !sentRich.body.comment && !!pinned && answer && answer.body.text === 'Спасибо, учтено' && /нажатия: 👍 1/.test(statsText) && /ждёт пересылки/.test(statsText),
    JSON.stringify({ commentShown, pinned: !!pinned, answer: answer && answer.body, statsText }));
}
