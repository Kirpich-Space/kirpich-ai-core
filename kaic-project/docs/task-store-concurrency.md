# Атомарное добавление контекста Task Store

Дата проверки: 2026-10-21. Активное дерево: `kaic-project/`.

## Фактическое состояние перед изменением

Корневого Cargo workspace нет. Backend и CLI — отдельные crates:
`kaic-backend/Cargo.toml` и `kaic-tui/Cargo.toml`. Эксперименты также имеют
собственные manifests. Компонентов Zig в обследованном дереве не найдено.
Рабочее дерево уже содержало многочисленные изменения; они сохранены.
Отдельное дерево `kaic-control-center-reliability/` не использовалось.

| Подсистема | Обнаруженная реализация и границы |
| --- | --- |
| Router, Capability Registry, Resource Registry | Реализованы; маршрутизация по категориям и ресурсам из YAML |
| Scheduler | Резидентность, primary/fallback, LRU, ограничение параллельных сессий, RAII резерваций и сверка прерванных load/unload |
| Task Store | SQLite, история YAML, ledger подтверждений, startup recovery, legacy receipt и отдельные acknowledgement |
| ModelBackend | Общий интерфейс; llama-server single-model по умолчанию, LM Studio — опциональный adapter |
| MCP/HITL | stdio, сроки операций, неизменяемые запросы, одноразовое исполнение и unknown outcome |
| CLI | Существующий `kaic-cli`: диагностика backend/models, задачи, polling, история, approvals и recovery |
| Агентное исполнение | Цикл генерации/инструментов в `control_center_api.rs`; самостоятельной развитой системы агентов нет |
| EmbeddedBackend | Частичный каркас с `todo!()`, не рабочий inference backend |
| Долговременная память | Отдельная подсистема не реализована; история задач её не заменяет |

Исторические ошибки `uncovered_requests`, `recovery_receipt`,
`migrate_recovery_schema`, `acknowledgement_id` не воспроизвелись. До правки
полный backend собирался: **253 passed, 0 failed, 2 ignored**.
Решения CLI-first, single-model llama-server и контракт approvals прочитаны;
их границы сохранены. GUI и новые зависимости не добавлялись.

## Выбранный этап и причина

Этап: атомарность добавления истории при конкурирующих SQLite-соединениях.
Он относится к приоритетной стабилизации Task Store и сохранности recovery.

`append_context` выполнял SELECT → изменение YAML → UPDATE без транзакции.
Mutex защищает одно соединение, но другой TaskStore/SQLite writer может
зафиксировать контекст между SELECT и UPDATE. Последний UPDATE затирал
новую историю. Детерминированный тест воспроизвёл потерю системной заметки
реального `mark_unknown_in_transaction`, пока таблица recovery и ledger уже
содержали неизвестный исход.

Второй тест с SQLite AFTER-trigger и `RAISE(FAIL)` показал сохранённую запись
несмотря на ошибку append: ошибка SQL не всегда откатывает всё statement.
До исправления новые проверки дали **2 passed, 2 failed**, exit 101.

## Изменение и гарантии

`append_context` теперь открывает `BEGIN IMMEDIATE` **до чтения** задачи,
добавляет запись и фиксирует `context` вместе с `updated_at`. При ошибке
транзакция откатывается. Использован уже существующий механизм транзакций.
Схема, YAML, публичные сигнатуры, recovery/approval transitions не менялись.
Миграций и изменений пользовательских баз не выполнялось.

Новые четыре теста проверяют:

- конкуренцию с записью recovery в DELETE и WAL, сохранность истории после
  повторного открытия, исходного unknown snapshot и запрета старого dispatch;
- откат контекста и timestamp после ошибки SQL, освобождение writer lock;
- явную ошибку повреждённого YAML с сохранением исходных байтов и timestamp,
  возможность записи другой задачи после ошибки;
- ошибку SQLITE_BUSY без записи и последующий успешный повтор append.

Гонка синхронизирована callback SQLite busy handler и каналами. Внешняя
транзакция фиксируется именно во время конфликта записи; sleeps не нужны.
Используются временные базы и локальные дубли, без GPU и моделей.

Изменены `kaic-backend/src/task_store.rs`, добавлены
`kaic-backend/src/task_store_concurrency_tests.rs` и этот документ.
Проверка diff относительно снимка файла до этапа подтвердила ограниченную
правку `append_context` и подключение нового тестового модуля.

## Выполненные проверки

Host: Artix Linux, rustc 1.99.0, Cargo 1.99.0, обычные features backend.
Использованы существующие cache/target, без обновления lockfiles:

```bash
export CARGO_HOME=/tmp/kaic-audit-krrrb9ap/cargo-home
# Для backend:
export CARGO_TARGET_DIR=/tmp/kaic-audit-krrrb9ap/kaic-backend/target
# Для отдельного CLI crate:
# export CARGO_TARGET_DIR=/tmp/kaic-tui-client-check/target
```

Команды backend выполнены из `kaic-backend/`, команда CLI — из `kaic-tui/`.

| Команда | Точный итог |
| --- | --- |
| `cargo build --workspace --locked --offline` | exit 0, полный backend собран; 11 существующих предупреждений |
| `cargo test --workspace --locked --offline task_store::` | exit 0; 34 passed, 0 failed, 0 ignored, 225 filtered |
| `cargo test --workspace --locked --offline` (backend) | exit 0; 257 passed, 0 failed, 2 ignored |
| `cargo clippy --workspace --all-targets --locked --offline` | exit 0; прежние 24 предупреждения bin и 50 test (14 duplicates); диагностик нового тестового файла нет |
| `cargo fmt --all -- --check` | exit 1; вывод полностью совпадает с результатом до правки; существующее форматирование других модулей не менялось |
| `rustfmt --edition 2021 --check src/task_store.rs` | exit 0; включает подключённые тестовые модули |
| `cargo test --workspace --locked --offline` (CLI) | exit 0; 33 unit + 8 process + 3 HTTP = 44 passed; 0 failed, 0 ignored |
| Отдельный synthetic stdio approval-тест | exit 0; 1 passed, 0 failed, 258 filtered |
| `git diff --check -- kaic-project/kaic-backend/src/task_store.rs` из корня | exit 0 |

Первый полный прогон в sandbox: 236 passed, 17 failed, 2 ignored — все
17 отказов связаны с EPERM на loopback bind. Указанные успешные полные
прогоны backend и CLI выполнены после разрешённого повтора вне sandbox.

Stdio-тест запущен непосредственно из изолированного временного каталога
по [контракту approvals](tool-approval-contract.md), с аргументами
`--ignored --exact control_center_api::approval_regression_tests::local_stdio_dispatch_executes_only_the_exact_once_approved_invocation --nocapture`.
Второй ignored-тест требует настоящего blender-mcp и не выполнялся.
Журналы проверок находятся в `/tmp/kaic-core-stabilization/`, не в Git.

## Ограничения и следующий этап

Это гарантия сериализации данного read/modify/write пути SQLite. Она не
разрешает запуск нескольких исполнителей одной задачи и не обеспечивает
ровно один внешний эффект. Старый request_id после recovery не переисполняется;
ACK не означает новое разрешение. Эти существующие проверки остаются зелёными.
Повреждённый YAML не ремонтируется автоматически: get возвращает ошибку,
а startup reconciliation такой базы может остановить запуск целиком.

Scheduler-регрессии на невозможный primary, сохранение resident/fallback,
занятость, LRU и неудачные/прерванные загрузки проходят. При этом статическое
обследование обнаружило оставшийся риск: `always_loaded_vram_mb()` суммирует
`u32` через `.sum()` без проверки общего переполнения; `fits_in_budget` защищает
только последующее сложение. Этот крайний случай отдельным тестом в данном
этапе не воспроизводился и не исправлялся. Следующий ограниченный этап —
регрессия и проверяемый отказ для переполнения суммарного footprint резидентов
до lifecycle-вызовов. Затем — живой smoke CLI → backend → llama-server на уже
имеющейся модели.

Учёт памяти остаётся оценкой из конфигурации, а не измерением VRAM.
Реальные llama-server, GGUF/chat templates, CUDA, optional embedded features,
PTY и самостоятельные экспериментальные crates этим этапом не проверялись.
Router mode llama-server, завершённый EmbeddedBackend и долговременная память
остаются дальнейшей работой. Модели не скачивались; исходники внешним
провайдерам не передавались.
