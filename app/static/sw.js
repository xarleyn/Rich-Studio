// Only this public, account-independent page belongs in Cache Storage. No editor shell,
// drafts, API responses, credentials, uploads or outgoing requests are stored here.
// No skipWaiting/forced reload: an update waits until the previous app windows close.
(() => {
  const CACHE = 'richpost-offline-v1';
  const OFFLINE = '/static/offline.html';
  /** @typedef {Event & {waitUntil(task: Promise<unknown>): void}} LifetimeEvent */
  /** @typedef {Event & {request: Request, respondWith(response: Promise<Response>): void}} FetchEvent */

  self.addEventListener('install', (event) => {
    /** @type {LifetimeEvent} */ (event).waitUntil(caches.open(CACHE).then((cache) =>
      cache.add(new Request(OFFLINE, { cache: 'reload', credentials: 'omit' }))));
  });

  self.addEventListener('activate', (event) => {
    /** @type {LifetimeEvent} */ (event).waitUntil(caches.keys().then((keys) =>
      Promise.all(keys.filter((key) => key.startsWith('richpost-offline-') && key !== CACHE)
        .map((key) => caches.delete(key)))));
  });

  self.addEventListener('fetch', (event) => {
    const e = /** @type {FetchEvent} */ (event);
    const url = new URL(e.request.url);
    // Only launching the app has a fallback. Auth, render, media and API URLs remain
    // ordinary network requests, including when opened directly in the address bar.
    if (e.request.method !== 'GET' || e.request.mode !== 'navigate'
      || url.origin !== self.location.origin || url.pathname !== '/') return;
    e.respondWith(fetch(e.request, { cache: 'no-store' }).catch(async () =>
      await caches.match(OFFLINE, { cacheName: CACHE })
      || new Response('RichPost: нет подключения / Offline', { status: 503,
        headers: { 'Content-Type': 'text/plain; charset=utf-8', 'Cache-Control': 'no-store' } })));
  });
})();
