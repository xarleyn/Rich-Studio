# 0025. HTML-виджеты (sendWidget) как отдельный kind

Дата: 2026-11-08

Telegram открыл исходники WidgetSandbox и опубликовал описание методов Bot API 10.4 для виджетов
(`sendWidget`, `sendSharedWidget`, `answerWidgetQuery`, `editMessageWidgetData`, `editSharedWidgetData`),
при этом в bundled-спеке (`app/bundled/api.json`) всё ещё Bot API 10.3 — методы появятся через
`make spec-update` в день релиза 10.4. Виджет — отдельный тип сообщения: HTML со встроенными CSS и JS
(лимиты: исходник 32 768 байт, после сжатия zlib level 9 — 4 096, иначе сервер отвечает
`INPUT_MEDIA_WIDGET_TOO_LARGE`; `chat_data`/`public_data` — по 1 024 байта). HTML после отправки
неизменяем, каналы не входят в прямую отправку (только личный чат, обычные группы и супергруппы).

**Решение.** Виджет — запись в реестре kinds (`src/services/kinds.rs`, ADR 0013): `method: "sendWidget"`,
`key: "widget"`, `editable: false`. Отправка идёт через общий `send_kind` (`/api/widget`), как poll и
медиа-посты; правка данных — через `EditingService::widget_data` (`/api/posts/{id}/widget-data`),
который для отправленных вызывает `editMessageWidgetData`, а отложенному виджету пишет данные в payload
(они уйдут вместе с `sendWidget`). Валидация — `src/services/widgets.rs`: точный zlib level 9 (flate2
стал прямой зависимостью с тем же `zlib-rs` бэкендом, что уже был в дереве через `zip` — нового кода в
графе нет), лимиты размеров и скан запрещённых возможностей профиля (сеть, ввод, видео, WebGL) —
предупреждениями, не ошибками: клиенты их вырезают, остальной код виджета работает. Запрет каналов
проверяется по сохранённому типу чата до отправки; неизвестные чаты пропускаем — ограничение
дублируется на сервере Telegram. Контракт `widget: {html, caption?, chat_data?, public_data?, allow_queries?}`
предварительный: точные имена полей Bot API 10.4 в спеке не опубликованы, payload уходит как есть
(правило 1), выравнивание — через `make spec-update`.

**Предпросмотр.** Официальный runtime WidgetSandbox вендорен в `app/static/vendor/widget/` (пин тега
`v0.6.1` — QuickJS-фиксы безопасности от 2026-11-06): сборка через их собственные esbuild-опции
(`packages/core/scripts/sandbox-build.ts`), host-обвязка — `scripts/widget-host-entry.mjs` + esbuild-плагины
в `scripts/widget-vendor-build.ts` (`npm run vendor:widget`, `--check` встроен в `npm run test:source`).
Отклонённая альтернатива — собственная заглушка SDK v1: она не воспроизводит ни QuickJS-лимиты, ни
валидатор CSS; вендоренный движок считает размеры и рендерит так же, как клиент Telegram.

**Границы и риски.** У upstream-репозитория нет файла LICENSE (на 2026-11-08), поле `license` в
`packages/core/package.json` отсутствует; движок `packages/quickjs-wasm` помечен MIT, а основной код —
без лицензии. Бандл закоммичен по явному решению владельца репозитория от 2026-11-08; заметка в
`app/static/vendor/widget/LICENSE-NOTE.txt` предписывает удалить папку по первому требованию
правообладателя. Если появится готовый браузерный бандл или лицензия — пин обновляется одной командой.

Предпросмотр живёт в отдельном окне (`/render/widget`, CSP `WIDGET_PREVIEW_CSP` в `src/security.rs`):
рантайму нужен SharedArrayBuffer, доступный только в изолированном top-level документе (COOP/COEP), —
iframe студии не подходит. Сеть в окне закрыта (`connect-src 'self'` — только наш сервер ради wasm),
JS виджета исполняется в worker'е QuickJS и до DOM страницы не доходит — это их собственная модель
безопасности (валидатор + host), CSP лишь закрывает сеть. Worker создаётся из blob-обёртки
(`import '<абсолютный URL>worker.js'`), а не напрямую: в части окружений Chromium (headless, Windows)
прямой `new Worker(http-URL, {type:'module'})` не стартует, blob-вариант работает везде (проверено
e2e-пробой); ассет wasm в worker.js делается абсолютным через `import.meta.url` по той же причине.
CSP студии получил `form-action 'self'` (форма предпросмотра отправляет исходник) и
`worker-src 'self' blob:` (окна-предпросмотры создают QuickJS-worker; Chromium в части окружений
проверяет создание по политике открывшего окна).

**Не решено здесь.** `sendSharedWidget`/`editSharedWidgetData` (общие виджеты по id), ответы на
`answerWidgetQuery` из getUpdates (событие `widget_query` ещё не приходило в тестах), и точная высота
виджета в редакторе — предпросмотр показывает её в статусе окна.
