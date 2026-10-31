// Public, script-free entry points for a person sharing one URL with their agent.
import assert from 'node:assert/strict';
import { mkdirSync } from 'node:fs';

export default async function ({ browser, BASE, check }) {
  const context = await browser.newContext();
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  mkdirSync('test-results/ui', { recursive: true });
  try {
    for (const [lang, prefix, title] of [['ru', '', 'Дайте агенту ссылку.'], ['en', '/en', 'Give your agent a link.']]) {
      await page.goto(BASE + prefix + '/features/ai-agents/');
      assert.equal(await page.locator('html').getAttribute('lang'), lang);
      assert.ok((await page.locator('h1').textContent()).includes(title));
      assert.equal(await page.locator('link[rel=describedby]').getAttribute('href'), '/llms.txt');
      assert.ok(await page.locator('a[href="/skills/richpost/SKILL.md"]').count());
      for (const width of [1400, 390]) {
        await page.setViewportSize({ width, height: width === 390 ? 844 : 1000 });
        assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
        await page.screenshot({ path: `test-results/ui/agent-setup-${lang}-${width}.png`, fullPage: true });
      }
    }
    check('Agent setup pages explain human-issued tokens in RU/EN and fit desktop/phone', true);
    for (const path of ['/llms.txt', '/agents.md', '/agents.txt', '/agents.json', '/llms-full.txt', '/skills/richpost/SKILL.md']) {
      const response = await context.request.get(BASE + path);
      assert.equal(response.status(), 200);
      assert.ok(!(await response.text()).includes('__APP_URL__'));
    }
    await page.goto(BASE + '/landing');
    await page.locator('footer a[href="/features/ai-agents/"]').click();
    await page.locator('h1').filter({ hasText: 'Дайте агенту ссылку.' }).waitFor();
    check('Landing links to agent onboarding and all machine-readable setup documents resolve', true);
    check('Agent onboarding pages have no JavaScript errors', errors.length === 0, errors.join(' | '));
  } finally { await context.close(); }
}
