#!/usr/bin/env node
// Read-only acceptance against an installed public site. No account or search-engine credentials.
import { createHash } from 'node:crypto';
import { request as httpRequest } from 'node:http';
import { request as httpsRequest } from 'node:https';
import { parseArgs } from 'node:util';

const { values } = parseArgs({ options: {
  site: { type: 'string', default: 'https://richpost.ru' },
  app: { type: 'string' },
  www: { type: 'string' },
  connect: { type: 'string' },
  timeout: { type: 'string', default: '15000' },
  'expected-pages': { type: 'string', default: '12' },
  help: { type: 'boolean' },
} });
if (values.help) {
  console.log('Usage: node scripts/check-public-seo.mjs --site https://example.com [--app https://app.example.com] [--www https://www.example.com] [--connect http://127.0.0.1:7424]');
  process.exit(0);
}

const origin = (value) => {
  const parsed = new URL(value);
  if (!['http:', 'https:'].includes(parsed.protocol) || parsed.username || parsed.password || parsed.pathname !== '/' || parsed.search || parsed.hash) {
    throw new Error(`Expected an HTTP(S) origin: ${value}`);
  }
  return parsed.origin;
};
const site = origin(values.site);
const app = values.app ? origin(values.app) : null;
const www = values.www ? origin(values.www) : null;
const connect = values.connect ? origin(values.connect) : null;
const timeout = Number(values.timeout);
const expectedPages = Number(values['expected-pages']);
if (!Number.isSafeInteger(timeout) || timeout < 1 || !Number.isSafeInteger(expectedPages) || expectedPages < 1) {
  throw new Error('--timeout and --expected-pages must be positive integers');
}

let checks = 0;
const failures = [];
const check = (ok, label) => { checks++; if (!ok) failures.push(label); };
const decode = (text) => text.replace(/&(?:amp|lt|gt|quot|apos|#39|#x27);/g, (entity) => ({
  '&amp;': '&', '&lt;': '<', '&gt;': '>', '&quot;': '"', '&apos;': "'", '&#39;': "'", '&#x27;': "'",
})[entity]);
const attributes = (tag) => Object.fromEntries([...tag.matchAll(/([\w:-]+)\s*=\s*(["'])(.*?)\2/gs)].map((match) => [match[1].toLowerCase(), decode(match[3])]));
const tags = (html, name) => [...html.matchAll(new RegExp(`<${name}\\b[^>]*>`, 'gi'))].map((match) => attributes(match[0]));
const meta = (html, key) => tags(html, 'meta').find((tag) => tag.name === key || tag.property === key)?.content;
const link = (html, rel) => tags(html, 'link').find((tag) => tag.rel === rel)?.href;
const assets = new Set();
const socialImages = new Set();
const seenTitles = new Set();
const seenDescriptions = new Set();
const pages = new Map();

async function request(url, method = 'GET', options = {}) {
  const target = new URL(url);
  if (![site, app, www].includes(target.origin)) throw new Error(`Refusing to crawl an unrelated origin: ${target.origin}`);
  const actual = connect && target.origin !== www ? new URL(target.pathname + target.search, connect).href : target.href;
  return new Promise((resolve, reject) => {
    const send = actual.startsWith('https:') ? httpsRequest : httpRequest;
    const req = send(actual, { method, signal: AbortSignal.timeout(timeout), headers: {
      ...(actual !== target.href ? { Host: target.host } : {}), ...options.headers,
    } }, (response) => {
      const chunks = [];
      let size = 0;
      response.on('data', (chunk) => {
        size += chunk.length;
        if (size > 8 * 1024 * 1024) req.destroy(new Error(`Response exceeds 8 MiB: ${url}`));
        else chunks.push(chunk);
      });
      response.on('error', reject);
      response.on('end', () => {
        const bytes = Buffer.concat(chunks);
        const headers = new Headers();
        for (const [key, value] of Object.entries(response.headers)) {
          if (value !== undefined) headers.set(key, Array.isArray(value) ? value.join(', ') : value);
        }
        resolve({ status: response.statusCode, headers, bytes, text: bytes.toString('utf8') });
      });
    });
    req.on('error', reject);
    req.end();
  });
}

async function pair(url, expected = 200) {
  const get = await request(url);
  const head = await request(url, 'HEAD');
  check(get.status === expected, `${url}: GET ${get.status}, expected ${expected}`);
  check(head.status === expected, `${url}: HEAD ${head.status}, expected ${expected}`);
  check(head.bytes.length === 0, `${url}: HEAD must have no body`);
  check(get.headers.get('content-type') === head.headers.get('content-type'), `${url}: HEAD Content-Type differs`);
  check(get.headers.get('cache-control') === head.headers.get('cache-control'), `${url}: HEAD cache policy differs`);
  return get;
}

function pageCheck(url, html, headers) {
  check((headers.get('content-type') || '').startsWith('text/html'), `${url}: HTML Content-Type`);
  check(!/noindex/i.test(headers.get('x-robots-tag') || meta(html, 'robots') || ''), `${url}: public page has noindex`);
  check(link(html, 'canonical') === url, `${url}: canonical must equal sitemap URL`);
  check((html.match(/<h1\b/gi) || []).length === 1, `${url}: exactly one H1`);
  check(!/__[A-Z_]+__/.test(html), `${url}: unresolved template token`);
  const title = html.match(/<title>(.*?)<\/title>/is)?.[1]?.trim();
  const description = meta(html, 'description');
  check(Boolean(title) && !seenTitles.has(title), `${url}: missing or duplicate title`);
  check(Boolean(description) && !seenDescriptions.has(description), `${url}: missing or duplicate description`);
  seenTitles.add(title);
  seenDescriptions.add(description);
  check(meta(html, 'og:title') === decode(title || ''), `${url}: og:title differs from title`);
  check(meta(html, 'og:url') === url, `${url}: og:url differs from canonical`);
  check(meta(html, 'twitter:card') === 'summary_large_image', `${url}: Twitter card`);
  const image = meta(html, 'og:image');
  check(Boolean(image) && image.startsWith(`${site}/static/`), `${url}: local absolute og:image`);
  check(meta(html, 'og:image:width') === '1200' && meta(html, 'og:image:height') === '630', `${url}: social image dimensions`);
  if (image?.startsWith(`${site}/static/`)) { assets.add(image); socialImages.add(image); }
  check(Boolean(link(html, 'apple-touch-icon')), `${url}: apple-touch-icon`);

  const alternate = tags(html, 'link').filter((tag) => tag.rel === 'alternate' && tag.hreflang);
  const alternates = Object.fromEntries(alternate.map((tag) => [tag.hreflang, tag.href]));
  const path = new URL(url).pathname;
  const ruPath = path.startsWith('/en/') ? path.slice(3) : path;
  const expected = { ru: site + ruPath, en: site + '/en' + ruPath, 'x-default': site + ruPath };
  check(alternate.length === 3 && Object.entries(expected).every(([lang, href]) => alternates[lang] === href), `${url}: reciprocal RU/EN/x-default links`);

  const scripts = [...html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/gi)].filter((match) => attributes(match[1]).type === 'application/ld+json');
  check(scripts.length > 0, `${url}: JSON-LD present`);
  const schemas = [];
  for (const script of scripts) {
    try {
      const parsed = JSON.parse(script[2]);
      check(parsed['@context'] === 'https://schema.org', `${url}: Schema.org context`);
      schemas.push(...(parsed['@graph'] || [parsed]));
      const digest = createHash('sha256').update(script[2]).digest('base64');
      check((headers.get('content-security-policy') || '').includes(`'sha256-${digest}'`), `${url}: CSP hash for JSON-LD`);
    } catch (error) { check(false, `${url}: invalid JSON-LD: ${error.message}`); }
  }
  const types = schemas.flatMap((schema) => [schema['@type']].flat());
  const isHome = path === '/' || path === '/en/';
  check(isHome ? types.includes('Organization') && types.includes('SoftwareApplication') : types.includes('BreadcrumbList'), `${url}: expected page schema`);
  check(!(headers.get('content-security-policy') || '').match(/script-src[^;]*'unsafe-inline'/), `${url}: CSP must not allow arbitrary inline scripts`);
  for (const tag of [...tags(html, 'img'), ...tags(html, 'script'), ...tags(html, 'link'), ...tags(html, 'source')]) {
    const resource = tag.src || (['stylesheet', 'preload', 'icon', 'apple-touch-icon'].includes(tag.rel) ? tag.href : undefined) || tag.srcset?.split(',')[0].trim().split(/\s+/)[0];
    if (resource?.startsWith('/static/')) assets.add(new URL(resource, site).href);
  }
  pages.set(url, { alternates });
}

async function run() {
  const robots = await pair(site + '/robots.txt');
  check(robots.text.includes(`Sitemap: ${site}/sitemap.xml`), 'robots.txt: canonical sitemap advertised');
  check(!/^Disallow:\s*\/\s*$/mi.test(robots.text), 'robots.txt: public site must allow crawling');
  const sitemap = await pair(site + '/sitemap.xml');
  check(/(?:application|text)\/xml/.test(sitemap.headers.get('content-type') || ''), 'sitemap.xml: XML Content-Type');
  const entries = [...sitemap.text.matchAll(/<url>([\s\S]*?)<\/url>/g)].map((match) => match[1]);
  const urls = entries.map((entry) => decode(entry.match(/<loc>(.*?)<\/loc>/s)?.[1] || ''));
  check(entries.length === expectedPages && new Set(urls).size === expectedPages, `sitemap.xml: expected ${expectedPages} unique pages, got ${entries.length}`);
  for (let index = 0; index < entries.length; index++) {
    const url = urls[index];
    if (!url.startsWith(site + '/')) { check(false, `sitemap.xml: unrelated URL ${url}`); continue; }
    const date = entries[index].match(/<lastmod>(.*?)<\/lastmod>/)?.[1];
    check(Boolean(date) && /^\d{4}-\d{2}-\d{2}$/.test(date) && Number.isFinite(Date.parse(date)) && Date.parse(date) <= Date.now(), `${url}: valid non-future lastmod`);
    check((entries[index].match(/<xhtml:link\b/g) || []).length === 3, `${url}: sitemap hreflang links`);
    const response = await pair(url);
    pageCheck(url, response.text, response.headers);
    const path = new URL(url).pathname;
    if (path !== '/') {
      for (const method of ['GET', 'HEAD']) {
        const redirected = await request(site + path.slice(0, -1) + '?utm_source=seo%20check&x=1', method);
        check(redirected.status === 308, `${path}: ${method} slash redirect must be 308`);
        check(new URL(redirected.headers.get('location') || '/', site).href === url + '?utm_source=seo%20check&x=1', `${path}: ${method} slash redirect preserves query`);
      }
    }
  }
  for (const [url, { alternates }] of pages) {
    for (const [lang, other] of Object.entries(alternates)) check(pages.has(other), `${url}: ${lang} alternate belongs to sitemap`);
  }
  for (const url of assets) {
    const response = await pair(url);
    check(response.bytes.length > 0, `${url}: nonempty static resource`);
    const cache = response.headers.get('cache-control') || '';
    check(/public/.test(cache) && /max-age=\d+/.test(cache), `${url}: explicit public cache lifetime`);
    const parsed = new URL(url);
    if (parsed.searchParams.has('v')) {
      check(parsed.searchParams.get('v') === createHash('sha256').update(response.bytes).digest('hex').slice(0, 12), `${url}: version matches resource bytes`);
      check(cache.includes('max-age=31536000') && cache.includes('immutable'), `${url}: verified version receives immutable cache`);
      const invalid = new URL(parsed);
      invalid.searchParams.set('v', 'invalid-seo-probe');
      const badVersion = await request(invalid.href, 'HEAD');
      check(!(badVersion.headers.get('cache-control') || '').includes('immutable'), `${url}: invalid version is not immutable`);
    }
    if (socialImages.has(url)) {
      const png = response.bytes.length >= 24 && response.bytes.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]));
      check(png && response.bytes.readUInt32BE(16) === 1200 && response.bytes.readUInt32BE(20) === 630, `${url}: actual PNG dimensions are 1200x630`);
    }
  }
  const unversioned = await request(site + '/static/landing.css', 'HEAD');
  check(unversioned.status === 200 && !(unversioned.headers.get('cache-control') || '').includes('immutable'), 'unversioned stylesheet: revalidatable cache');
  const icon = await pair(site + '/favicon.ico');
  check(icon.bytes.length > 6 && icon.bytes.readUInt32LE(0) === 65536, 'favicon.ico: real ICO file');
  for (const prefix of ['', '/en']) {
    const missing = await pair(site + prefix + '/seo-smoke-page-does-not-exist', 404);
    check((missing.headers.get('content-type') || '').startsWith('text/html'), `${prefix || '/'}: branded HTML 404`);
    check(tags(missing.text, 'a').some((tag) => tag.href === (prefix ? '/en/' : '/')), `${prefix || '/'}: 404 links home`);
  }
  if (app) {
    const home = await pair(app + '/');
    check(/noindex/i.test(home.headers.get('x-robots-tag') || ''), 'app: noindex response header');
    const privateResponse = await request(app + '/api/drafts');
    check(privateResponse.status === 401, 'app: private API requires authorization');
    check(/noindex/i.test(privateResponse.headers.get('x-robots-tag') || ''), 'app: private API noindex');
  }
  if (www) {
    for (const method of ['GET', 'HEAD']) {
      const result = await request(www + '/en/features/?utm_source=seo%20check&x=1', method);
      check(result.status === 308, `www: ${method} permanent 308 redirect`);
      check(result.headers.get('location') === site + '/en/features/?utm_source=seo%20check&x=1', `www: ${method} preserves path and query`);
    }
  }
}

try { await run(); } catch (error) { check(false, error.message); }
for (const failure of failures) console.error(`FAIL ${failure}`);
console.log(`SEO smoke: ${checks - failures.length}/${checks} checks, ${pages.size} pages, ${assets.size} resources; ${failures.length ? 'FAIL' : 'PASS'}`);
process.exitCode = failures.length ? 1 : 0;
