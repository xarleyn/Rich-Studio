# tgfake: отдельная проверка Bot API

[tgfake](https://github.com/EvilFreelancer/tgfake) — локальный fake Telegram Bot API на Go. Он отвечает реальными формами Bot API, строго повторяет часть ошибок Telegram и умеет отдавать журнал вызовов и переписку через `/sim/`.

В проекте он используется как **необязательная** интеграционная проверка `tests/rust/tgfake.rs`. Она не входит в `make check`: тест поднимает внешний процесс, поэтому ресурсоёмкий и помечен `#[ignore]`. Обычный `cargo test` его не запускает, а in-process mock-серверы остаются быстрыми для повседневной работы.

## Что проверяется

- реальный `sendRichMessage` принимается и попадает в переписку tgfake;
- повтор с тем же `Idempotency-Key` не создаёт второй вызов;
- кнопки опубликованного поста уходят через `editMessageReplyMarkup`;
- ошибка Telegram `400` превращается в `422` на HTTP-границе;
- ответы и вызовы читаются через `/sim/outbox` и `/sim/chat`.

tgfake 1.4.x не реализует опросы, Live Photo, платный контент и темы форума: эти методы остаются в обычных Rust-тестах с in-process mock. tgfake дополняет их реальной формой Bot API и строгими ошибками, а не заменяет.

## Запуск

Нужен бинарник tgfake версии 1.4.1 или новее. Возьмите архив со [страницы релизов](https://github.com/EvilFreelancer/tgfake/releases): `tgfake_<version>_windows_amd64.zip`, `tgfake_<version>_linux_amd64.tar.gz` или сборку для macOS. Внутри лежит `tgfake`/`tgfake.exe`.

```sh
export TGFAKE_BIN="$PWD/work/tgfake/tgfake"   # Windows: .../tgfake.exe
make e2e-tgfake
```

Без `make`:

```sh
TGFAKE_BIN="$PWD/work/tgfake/tgfake" \
  cargo test --locked --test tgfake -- --ignored --test-threads=1
```

Если `TGFAKE_BIN` не задан, тест останавливается с явным сообщением. The binary is started with `--addr 127.0.0.1:0`, so the port is free and the origin is read from the banner; after the test the process is killed.

## Границы

- tgfake проверяет транспорт и форму Bot API, но не заменяет проверки политики Rich Studio (идемпотентность, очередь, права, SQLite); они остаются в обычных Rust-тестах.
- Живой Telegram по-прежнему не вызывается.
- В CI отдельная job `tgfake` скачивает закреплённый бинарник с проверкой SHA-256 и запускает тест только после `rust`, `python`, `js`, `e2e` и `docker`; в обычную матрицу быстрых проверок он не входит.
- Версия tgfake фиксируется вручную; при обновлении проверьте, что он всё ещё поддерживает используемые rich-методы и строгие ошибки.
