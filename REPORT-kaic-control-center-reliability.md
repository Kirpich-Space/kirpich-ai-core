# KAIC Control Center reliability

Дата проверки: 2026-09-05, Linux.

## База и изоляция

- Base / remote-tracking ref: `origin/media-pipeline-and-scheduler-work` = `28f064e4b1f960df642debd4d9ee8dba9ef10b96`.
- Ветка: `codex/kaic-control-center-reliability`.
- Worktree: `/home/kirpich/Документы/Кирпич Space Company/Kirpich-IT/Kirpich_Soft/KAIC/kaic-control-center-reliability`.
- Исходный checkout не очищался, не reset/stash/checkout: после работы он по-прежнему на `media-pipeline-and-scheduler-work`, HEAD `28f064e4…`, remote ref тот же, status содержит 196 dirty entries.

## Корневые причины и исправления

1. `kaic-ui/src/api/controlCenter.ts` выполнял прямой renderer `fetch` с одним `Content-Type`; runtime `KAIC_CONTROL_TOKEN` не имел пути к запросу. Теперь токен читает только Electron main. `preload.js` публикует девять фиксированных операций, IPC принимает имя операции и данные, а main-process operation table единолично задаёт base URL, path, method и `Authorization: Bearer ...`. Renderer не получает токен, URL, заголовки или произвольный transport primitive. Пустой/whitespace token блокирует операции с безопасным сообщением. Токен редактируется даже из текста HTTP-ошибки.
2. `agent_steps` безусловно превращал любой `GenerateResponse::Text` в `LoopOutcome::Answer`; затем `record_outcome(... Done ...)` фиксировал пустой ответ. Теперь общий текстовый agent loop возвращает штатную ошибку `модель вернула пустой текст`, если `text.trim().is_empty()`. Существующий error path пишет понятную system-запись и вызывает `claim_finish(... Failed)`, поэтому уже выставленные человеком `Paused`/`Cancelled` не переписываются. Непустой текст сохраняется дословно. Media search-term extraction остаётся на отдельном деградирующем пути и не менялся.

## RED → GREEN

### Electron/UI

- RED до production-кода: `node --test electron/control-center.test.js` завершился `ERR_MODULE_NOT_FOUND: ./control-center`; main-owned authenticated boundary отсутствовала, что соответствует исходному прямому renderer `fetch` без Authorization.
- GREEN: `npm test` — 5 tests, 5 passed, 0 failed. Покрыты все существующие UI operations, правильный Bearer, missing token, 401, network error, empty body, редактирование секрета, запрет неизвестной операции/destination и Linux boundary smoke.

### Rust model output

- Тесты написаны до production-проверки и по исходному пути ожидаемо должны видеть `Done` для `""` и whitespace. Поведенческий RED/GREEN выполнить в этом Linux-сеансе не удалось: чистый Cargo cache не содержит даже `anyhow`; online run не смог соединиться с `index.crates.io`, offline run вернул `no matching package named anyhow`.
- Добавлены тесты: empty + whitespace → `Failed` без assistant entry; non-empty → `Done` с исходным текстом; pause-race сохраняет `Paused`. Существующие cancel, human follow-up и tool-flow тесты не редактировались.
- Rust frontend-проверка `rustfmt --edition 2021 --emit stdout src/control_center_api.rs` завершилась с exit 0: файл синтаксически разобран. Это не заменяет `cargo test`/`cargo build` и не выдаётся за них.

## Изменённые файлы

- `kaic-project/kaic-ui/electron/control-center.js` — фиксированные HTTP operations, auth, ошибки и IPC handler.
- `kaic-project/kaic-ui/electron/control-center.test.js` — auth/error/smoke tests.
- `kaic-project/kaic-ui/electron/preload.js` — context-isolated typed operation bridge.
- `kaic-project/kaic-ui/electron/main.js` — runtime token ownership и IPC registration.
- `kaic-project/kaic-ui/src/api/controlCenter.ts` — сохранённый публичный typed API поверх bridge.
- `kaic-project/kaic-ui/src/vite-env.d.ts` — типы preload boundary.
- `kaic-project/kaic-ui/package.json` — существующий Node test runner как `npm test`.
- `kaic-project/kaic-backend/src/control_center_api.rs` — empty-output validation и focused tests.
- `kaic-project/README.md` — Linux runbook с одним runtime secret и описание main/preload/IPC.
- `PLAN-kaic-control-center-reliability.md`, `BUDGET-kaic-control-center-reliability.txt`, этот отчёт.

## Свежая проверка

- `npm test`: 5 passed, 0 failed.
- `npm run build`: Vite 5.4.21, 38 modules transformed, exit 0.
- `tsc --noEmit`: exit 0.
- `node --check` для `control-center.js`, `preload.js`, `main.js`: exit 0.
- `git diff --check`: exit 0.
- Scan production UI/dist: нет тестовых секретов; `KAIC_CONTROL_TOKEN` читается только в Electron main, Authorization строится только main client.
- Backend full tests/build: не выполнены по описанному Cargo network/cache blocker; свежих чисел backend checks нет.

## Linux smoke

Строгий локальный HTTP double поднят на эфемерном `127.0.0.1` порту с токеном `linux-smoke-ephemeral-token`, существовавшим только в test process:

- прямой `GET /status` без Authorization → 401;
- Electron main-process read-only operation `getStatus` → Bearer предъявлен, 200, тело разобрано;
- тест прошёл и сервер закрыт.

Это проверка Electron HTTP boundary, а не полный backend smoke: Linux backend не собран из-за недоступных Cargo dependencies; LM Studio/GPU не запускались.

## Cleanup, блокеры и вне scope

- Эфемерные HTTP servers закрываются `afterEach`; token не сохранён в runtime artifacts.
- Временные symlink на локальный Linux `node_modules` удалены; `dist/` игнорируется Git.
- Не менялись Scheduler, SQLite schema/migrations, Blender/MCP config, media pipeline, tool gate, human-followup, audit или MCP cleanup.
- Не выполнялись commit, push или merge.
- Единственный блокер полной приёмки: crates.io недоступен и Linux Cargo cache пуст. Также штатный `npm ci` завис на Electron postinstall; UI build/typecheck выполнены с уже установленным Linux dependency tree совместимой Vite 5.4.x без копирования файлов из dirty checkout.
