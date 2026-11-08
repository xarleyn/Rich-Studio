// Сборка вендоренного WidgetSandbox в app/static/vendor/widget; запускается через
// scripts/vendor-widget-runtime.mjs (npm run vendor:widget). Пин тега v0.6.1:
// пакеты не опубликованы в npm, исходники — TypeScript, поэтому checkout берётся
// из git, зависимости ставит pnpm, а два in-page бандла собирают их собственные
// esbuild-опции (packages/core/scripts/sandbox-build.ts).
import { build } from 'esbuild';
import type { Plugin } from 'esbuild';
import { mkdirSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';

// Их esbuild-опции для двух in-page бандлов; раннер собирает этот файл вместе с
// модулем из checkout (алиас widget-sandbox-build), чтобы node не стриппил TS
// под node_modules.
import { rendererBuildOptions, sandboxBuildOptions } from 'widget-sandbox-build';

const TAG = 'v0.6.1';
const UPSTREAM = 'https://github.com/TelegramMessenger/widget-sandbox';

const LICENSE_NOTE = `WidgetSandbox runtime, vendored from ${UPSTREAM} at tag ${TAG}.

ВНИМАНИЕ: у upstream-репозитория нет файла LICENSE (на 2026-11-08), поле license
в packages/core/package.json отсутствует; packages/quickjs-wasm (движок QuickJS)
помечен MIT. Бандл закоммичен по решению владельца репозитория от 2026-11-08
(docs/decisions/0025-widgets.md). Если правообладатель попросит удалить код —
удалить app/static/vendor/widget/ и отпустить пин.

Не редактируйте файлы в этой папке руками: они пересобираются
npm run vendor:widget из пиннутого checkout.`;

/** Собирает рантайм из checkout в выходной каталог; вызывается раннером (repoRoot — корень репозитория). */
export async function buildAll(checkout: string, output: string, repoRoot: string): Promise<string[]> {
  const pkgRoot = path.join(checkout, 'packages', 'core');
  mkdirSync(output, { recursive: true });

  // Два in-page бандла из их собственных esbuild-опций: QuickJS-worker (sandbox)
  // и renderer для iframe-режима (их продуктовый renderer).
  const sandbox = await build(sandboxBuildOptions(pkgRoot));
  writeFileSync(path.join(output, 'sandbox.js'), sandbox.outputFiles[0].text);
  const renderer = await build(rendererBuildOptions(pkgRoot));
  writeFileSync(path.join(output, 'renderer.js'), renderer.outputFiles[0].text);

  // Host-обвязка: WidgetHost + наш entry; virtual:*-модули отдаём из готовых файлов,
  // как их же Vite-плагин (packages/core/vite.ts).
  const virtuals: Plugin = {
    name: 'widget-vendor-virtuals',
    setup(builder) {
      builder.onResolve({ filter: /^virtual:(sandbox|renderer)-bundle$/ }, (args) => ({
        path: args.path,
        namespace: 'widget-vendor',
      }));
      builder.onLoad({ filter: /.*/, namespace: 'widget-vendor' }, (args) => {
        const file = args.path === 'virtual:sandbox-bundle' ? 'sandbox.js' : 'renderer.js';
        const bundle = readFileSync(path.join(output, file), 'utf8');
        // Как их Vite-плагин (packages/core/vite.ts): бандл уезжает СТРОКОЙ, которую
        // worker/iframe вычислит в песочнице, а не кодом на странице.
        return { contents: `export default ${JSON.stringify(bundle)}`, loader: 'js' };
      });
      // `wasm?url` из worker.ts: файл-ассет рядом с чанком worker'а.
      builder.onResolve({ filter: /^@telegram\/widget-sandbox-quickjs\/wasm\?url$/ }, () => ({
        path: path.join(checkout, 'packages', 'quickjs-wasm', 'dist', 'emscripten-module.wasm'),
      }));
    },
  };
  // esbuild не собирает `new Worker(new URL(...))` сам: worker.ts — явный второй
  // вход, а два места в исходниках, создающие worker, получают blob-обёртку.
  // Blob-обёртка нужна потому, что в части окружений Chromium (headless, Windows)
  // прямой `new Worker(http-URL, {type:'module'})` не стартует, а worker из blob
  // с `import '<abs url>'` — работает (проверено e2e-пробой 2026-11-08).
  const workerUrlFix: Plugin = {
    name: 'widget-vendor-worker-url',
    setup(builder) {
      builder.onLoad({ filter: /(widget-host|worker-pool)\.ts$/ }, async (args) => {
        const contents = readFileSync(args.path, 'utf8');
        if (!contents.includes("new URL('./worker.ts', import.meta.url)")) return undefined;
        const replacement =
          "new Worker(URL.createObjectURL(new Blob([`import '${new URL('./worker.js', import.meta.url).href}';`], { type: 'text/javascript' })), { type: 'module' })";
        return {
          contents: contents
            .split("new Worker(new URL('./worker.ts', import.meta.url), { type: 'module' })")
            .join(replacement),
          loader: 'ts',
        };
      });
    },
  };
  await build({
    entryPoints: [
      { in: path.join(repoRoot, 'scripts', 'widget-host-entry.mjs'), out: 'host' },
      { in: path.join(pkgRoot, 'src', 'host', 'worker.ts'), out: 'worker' },
    ],
    bundle: true,
    format: 'esm',
    splitting: true,
    platform: 'browser',
    target: 'es2022',
    minify: true,
    logLevel: 'warning',
    outdir: output,
    chunkNames: 'chunk-[name]-[hash]',
    loader: { '.wasm': 'file' },
    nodePaths: [pkgRoot],
    alias: { '@telegram/widget-sandbox': path.join(pkgRoot, 'src') },
    plugins: [virtuals, workerUrlFix],
  });
  const files = readdirSync(output, { recursive: true, withFileTypes: true })
    .filter((entry) => entry.isFile())
    .map((entry) => path.join(entry.parentPath ?? entry.path, entry.name));
  if (!files.some((file) => file.endsWith('.wasm'))) {
    throw new Error('emscripten-module.wasm не попал в вывод host-бандла');
  }
  // Ассет wasm уезжает относительным путём; в blob-обёртке worker'а относительный
  // URL неразрешим, поэтому в worker.js строку ассета делаем абсолютной через
  // import.meta.url самого worker.js (его URL — файл на сервере).
  const wasmAsset = files
    .map((file) => path.relative(output, file).replaceAll('\\', '/'))
    .find((file) => file.endsWith('.wasm'));
  const workerFile = path.join(output, 'worker.js');
  const workerText = readFileSync(workerFile, 'utf8');
  const assetReference = `"./${wasmAsset}"`;
  if (!workerText.includes(assetReference)) {
    throw new Error(`в worker.js нет ссылки на ${assetReference}`);
  }
  writeFileSync(workerFile, workerText.replace(assetReference, `new URL("${assetReference.slice(1, -1)}", import.meta.url).href`));

  writeFileSync(path.join(output, 'LICENSE-NOTE.txt'), LICENSE_NOTE);
  writeFileSync(path.join(output, 'VERSION.json'), `${JSON.stringify({ source: UPSTREAM, tag: TAG })}\n`);
  return files
    .map((file) => path.relative(output, file))
    .concat(['sandbox.js', 'renderer.js', 'LICENSE-NOTE.txt', 'VERSION.json']);
}
