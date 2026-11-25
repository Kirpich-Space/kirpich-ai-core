# Repository Guidelines

## Структура проекта

Активный код написан на **Rust** (с локальным x86_64 inline ASM в `Studio/src/agent/asm_accel.rs` и переносимым Rust fallback) без Electron, React, TypeScript и Node.js:

- `Studio/` (`kaic-studio`, Rust edition 2024) — единый нативный бинарник:
  - `src/tui/` — нативный минималистичный терминальный интерфейс (`kaic-studio tui`, автоматический fallback без `DISPLAY`/`WAYLAND_DISPLAY`, без внешних TUI-крейтов).
  - `src/agent/` — агентный runtime: политики файлов (`openat O_NOFOLLOW`) и кода (`ExecPolicy`), провайдеры (`LocalLlama`, `OllamaLocal`, `OpenAiCompat`, `AnthropicApi`, `CodexCli`, `ClaudeCli`, `AgyCli`), адаптеры парсера и JSON-схем (`tool_parser.rs`), управление контекстом (`context.rs`), субагенты (`subagents.rs`), места (`workspace_live.rs` + `asm_accel.rs`, динамический режим выключен по умолчанию).
  - `src/ui/` — опциональный нативный графический интерфейс `eframe/egui` (`kaic-studio gui`).
  - `src/platform.rs` — POSIX-адаптеры процессов и сокетов для Linux и FreeBSD (runtime FreeBSD не тестировался).
- `kaic-project/`:
  - `kaic-backend/src/` — Rust HTTP API контрольного центра, Scheduler, Task Store, MCP и медиа-пайплайн. Конфигурация — в `config/`.
  - `kaic-tui/src/` — лёгкий терминальный CLI/REPL клиент для контрольного центра `kaic-backend`.
  - `experiments/` — самостоятельные Cargo-проекты, не модули backend.
  - `docs/` — технические контракты и архитектурные ограничения.

`kaic-control-center-reliability/` содержит отдельное дерево разработки (git worktree). Не смешивайте его изменения с активным проектом без отдельного ревью.

## Сборка, проверки и запуск

Команды для `Studio` (Rust edition 2024):

```bash
cargo build --manifest-path Studio/Cargo.toml --locked
cargo test --manifest-path Studio/Cargo.toml --locked
```

Запуск инференса `LocalLlama` требует установленного `llama-server` и проверенного локального GGUF-каталога; TUI/CLI для Ollama, OpenAI-compat, Anthropic API и внешних CLI работают без локального GGUF.

Команды для `kaic-backend` и `kaic-tui` (Rust edition 2021):

```bash
cargo build --manifest-path kaic-project/kaic-backend/Cargo.toml --locked
cargo test --manifest-path kaic-project/kaic-backend/Cargo.toml --locked
cargo test --manifest-path kaic-project/kaic-tui/Cargo.toml --locked
```

Запуск `kaic-backend` требует `KAIC_CONTROL_TOKEN` длиной минимум 16 символов (`127.0.0.1:4545`).

## Стиль и именование

Rust: четыре пробела, `snake_case` для функций и модулей, `PascalCase` для типов. Проверяйте форматирование:
- в `Studio/`: `rustfmt --edition 2024 --check src/<module>.rs`
- в `kaic-project/*/`: `rustfmt --edition 2021 --check src/<module>.rs`

## Тестирование

Используйте временные каталоги, SQLite `:memory:`, локальные loopback HTTP-мок-серверы и локальные mock CLI-скрипты. Проверяйте гонки, отмену (`Esc` / `Cancel`), повторные решения, одноразовые подтверждения (`request_id`, SHA-256 `action_digest`), восстановление JSON-вызовов и безопасность путей (`O_NOFOLLOW`). Пропущенный тест не означает успешную проверку.

## Безопасность и работа агентов

- Не сохраняйте токены, API-ключи (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`), базы, модели и runtime-журналы в Git или `state.json`.
- Подтверждение инструмента должно сохранять привязку `request_id` к задаче, инструменту и аргументам; история диалога не выдаёт полномочий.
- Режим динамического взаимодействия (мульти-курсоры и виртуальные клавиатуры) выключен по умолчанию и активируется только явным действием пользователя.
- Перед правками проверяйте `git status`. Сохраняйте чужие изменения и разделяйте проверенное на Linux x86_64 и непроверенные окружения (FreeBSD, живой GPU при занятой VRAM).
