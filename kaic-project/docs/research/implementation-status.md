# KAIC: результат исследования и подготовленного этапа TUI

**Продолжение:** после этого библиотечного этапа добавлен исполняемый CLI;
основной интерфейс проекта — CLI. Актуальный запуск, проверки и ограничения:
[CLI-first](../cli-first.md). Ниже сохранены результаты первоначального этапа.

Дата: 2026-10-18. Область — активное дерево `kaic-project`; существующие файлы
не менялись. **Полный первый milestone TUI не завершён:** Ratatui/Crossterm
предложены пользователю и ожидают согласия по пункту 12 задания. Подготовлена
собираемая библиотека протокола, HTTP-клиента и состояния; готового UI нет.

## Исследование и решение

- [OpenCode: архитектура и матрица A/B/C/D](opencode-reuse.md).
- [OpenCode/OpenTUI: закреплённые SHA, лицензии и хеши](provenance-opencode.md).
- [Claude Code: только публичные концепции и интерфейсы](claude-public-features.md).
- [Решение о нативном UI и отвергнутые альтернативы](native-tui-design.md).
- [Спецификация интеграции](kaic-tui-integration.md).

Выбрано независимое состояние/renderer на Rust поверх KAIC HTTP API. OpenTUI
не включён: TypeScript/Solid/Zig увеличивают toolchain и стоимость интеграции.
OpenCode API пока не нужен: создаёт второй оркестратор. Непосредственное ручное
управление ANSI не выбрано из-за Unicode, portability и terminal lifecycle.

**Фактическое повторное использование OpenCode/OpenTUI: отсутствует.** MIT
проверена для закреплённых материалов, но весь транзитивный граф не аудирован.
Claude Code предоставил только разрешённые публичные design references;
собственническая реализация, ресурсы и брендинг не использовались.

## Что реализовано и проверено в коде

Новый отдельный crate `kaic-project/kaic-tui`, `publish = false`:

| Файл | Назначение |
| --- | --- |
| `Cargo.toml`, `Cargo.lock`, `.gitignore`, `src/lib.rs` | Самостоятельная библиотека без Electron/native inference и UI-framework |
| `src/protocol.rs` | DTO реальных Task/Model/Status API, строгий approval DTO |
| `src/client.rs` | GET/POST существующих маршрутов, loopback-only, limits, безопасные ошибки, без retries |
| `src/app.rs` | Выбор/новый draft, команды, late response protection, immutable approval/stale latch |
| `src/text.rs` | Escape terminal controls/Bidi, точное полное представление approval, оригинальный ASCII Кич |
| `tests/http_contract.rs` | Независимые HTTP-контрактные проверки публичного клиента |
| `README.md` | Команды и границы реализованного ядра |

Весь новый Rust-код — собственная реализация KAIC. Renderer не существует;
наличие Кича в константе не означает интеграцию в работающий welcome screen.
История читается из backend; собственная БД, исполнитель файлов и модельный
runtime не добавлялись. Новый токен/credentials в Git не сохранялись.

## Безопасность и независимое ревью

`ToolApproval` захватывается целиком, не подменяется при переключении задачи.
Свежий GET перед POST сравнивает каждое поле и статус; решение отправляет только
исходный request ID. Окончательная защита от гонки остаётся серверным claim.
Отмена/изменение/потеря связи помечают диалог устаревшим, повтор блокирует busy.
Ответы/ввод/время ограничены, proxy/redirect/retry выключены, Bearer не выводится.

Независимое чтение четырёх core-модулей не выявило конкретных P0/P1 bypass.
Это **не** заключение о безопасности готового интерфейса. Остаются обязательства:
полнота видимого approval, отсутствие скрытого confirm, keyboard/paste/repeat
handling, сериализация refresh/POST, корреляция ошибок и восстановление терминала.
Нельзя вызывать `App::fail` для чужого refresh при ещё живом POST.

## Проверки этого этапа

Среда: Artix Linux, Rust 1.99.0, fish 4.9.3. Глобальные пакеты не менялись.
Cargo использовал существующий cache; модели и облачные провайдеры не нужны.

| Проверка | Фактический результат |
| --- | --- |
| `cargo test --locked --offline` нового core | 26 unit tests прошли, 0 failed, 0 ignored |
| Итоговый общий прогон нового crate | 26 unit + 3 независимых HTTP integration = 29 passed, 0 failed, 0 ignored; doc-tests: 0 |
| Первоначальный sandbox-прогон core | 21 passed, 5 failed на loopback bind EPERM; это ограничение среды, повтор после разрешения прошёл |
| Первоначальный sandbox-прогон integration | 3 bind EPERM; разрешённый повтор 3 passed, затем подтверждено общим прогоном |
| `cargo build --locked --offline` нового crate | Успешно; это библиотека, не TUI executable |
| `cargo fmt --check` | Успешно для новых Rust-модулей |
| `cargo clippy --locked --offline --all-targets -- -D warnings` | Успешно, включая новые integration tests |
| P0 backend: `cargo test --locked --offline ... approval_regression_tests` | 17 passed, 0 failed, 1 ignored, 161 filtered out; 3 существующих warning |
| Неизменность исходных файлов | SHA-256 проверка 276 файлов baseline: изменений/исчезновений нет |
| Полная cross-platform metadata offline | Не получена: отсутствовал неиспользуемый host-целью bumpalo; host-filtered metadata получена |

Логи текущего запуска: `/tmp/kaic-tui-m1/client-final-tests.log` и
`/tmp/kaic-tui-m1/backend-p0-tests.log` (временные артефакты, не часть Git).
Независимый reviewer проверил также финальные manifest/lockfile/lib, три
integration tests и документацию; тесты сам повторно не запускал.

Core покрывает approval identity, замену каждого поля, stale/duplicate/cancel,
deny, поддельную историю, восстановление связи/черновика, UTF-8 byte limit,
переключение задач, поздний Create, malformed JSON, 401/409, redirect и размер
ответа. HTTP-дубли — тестовые данные, не демонстрация работающих моделей.

Не выполнялись: настоящий терминальный startup/shutdown/resize/PTY, panic/SIGTERM,
Unicode column/grapheme layout, token streaming, subagent execution, file sandbox,
восстановление живого исполнителя после аварии, Wayland emulator smoke, live
LM Studio/MCP. Причины: renderer/actor отсутствуют либо API такого не имеет.
StdIO fixture P0 в этом этапе явно не запускался; ignored не считается успехом.

## Лицензии зависимостей и выпуск

Lockfile фиксирует 92 внешних registry-пакета с checksum; Linux-filtered metadata
содержит 79. У всех этих 79 есть объявленная лицензия; missing-license нет.
Ratatui/Crossterm в графе отсутствуют. Это декларации manifest, не полный аудит
license/NOTICE текстов и ресурсов. 13 платформенных пакетов требуют отдельной
проверки. Для публикации нужен полный набор уведомлений, включая Unicode-3.0
(18 пакетов и дополнительное условие `unicode-ident`).

`sync_wrapper 1.0.2` — Apache-2.0-only; локально проверены manifest и LICENSE.
Корневой `README.md:192` заявляет GPL-2.0 и ссылается на отсутствующий LICENSE.
GPL-2.0-only и GPL-2.0-or-later здесь не различены. Для объединённого
распространения это блокер решения о лицензии: Apache-2.0 несовместима с
GPLv2-only; наличие разрешения на последующие GPL меняет варианты совместимости.
Основание: [FSF о совместимости](https://www.gnu.org/licenses/license-compatibility.en.html).
Лицензия KAIC самостоятельно не изменялась; решение остаётся за правообладателем.

## Фактическая работа агентов и навыков

Native Codex delegation действительно выполнялась:

| Агент | Делегированная работа и результат |
| --- | --- |
| `opencode_research` | Первичные материалы OpenCode/OpenTUI, SHA/лицензии, матрица; два документа |
| `claude_public_research` | Только официальные публичные документы, правовые границы; один документ |
| `native_tui_design` | ADR/UX/API gaps; затем read-only проверка лицензий Cargo-графа |
| `tui_http_client` | Только новые protocol/client; 7 тестов также отдельно проверены во временном crate |
| `tui_state` | Только новые app/text; состояние, sanitizer, тесты |
| `tui_security_review` | Запущен, но код не проверил: роль запрещала commands, file-read API отсутствовал; результат не засчитывается как ревью |
| `tui_independent_review` | Реальное read-only ревью четырёх модулей с разрешённым чтением; описанные выше выводы и ограничения |
| `tui_contract_tests` | Независимые HTTP-контрактные тесты в отдельном новом файле |

Ведущий интегрировал crate, спецификацию, проверки и отчёт. Файлы распределялись
по владельцам; параллельной записи одного файла не было. `kirpich-agent` в этом
этапе не участвовал; частные исходники внешним провайдерам не передавались.

Применены выбранные установленные навыки: `kirpich-research`, `kirpich-architecture`,
`kirpich-requirements`, `kirpich-rust`, `kirpich-api`, `kirpich-dependencies`,
`kirpich-security`, `kirpich-skill-audit`, `kirpich-test-strategy`, `kirpich-review`,
`kirpich-documentation`, `kirpich-orchestrate`, `kirpich-git`. Сторонние skill/plugin
scripts не исполнялись. Навыки использовались для проверки первичных источников,
контрактов, угроз, границ изменений и независимого воспроизводимого тестирования.

## Приоритеты продолжения

1. Получить ответ на согласование Ratatui/Crossterm; до него не добавлять эти
   зависимости. Затем реализовать renderer/actor/terminal RAII и завершить M1
   с независимым ревью и PTY-проверками.
2. Сохранить P0 exact approval на всей UI-цепочке, включая resize, scrolling,
   paste/repeat, disconnect и ambiguous POST. Не добавлять auto-approve.
3. Завершить P1 cancellation/crash reconciliation backend отдельной задачей,
   затем пагинацию истории и versioned stream events.
4. После реальных контрактов добавить parent/child agents, skills provenance,
   workspace/diff и измеренную телеметрию. Kirpich MIND — будущий потребитель
   контрактов, его репозиторий и EmbedIDE не изменялись.
5. Перед выпуском разрешить лицензионную неоднозначность и полный notice audit;
   commit/push/publish без разрешения пользователя не выполняются.
