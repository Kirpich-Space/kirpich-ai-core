# KAIC Project — Rust Control Center & Standalone CLI/Experiments

> Миграция на нативный Rust завершена: устаревший каталог `kaic-ui/` (Electron / React / TypeScript / Node.js) удалён. Основной объединённый бинарник с нативным TUI, агентным runtime (7 провайдеров) и опциональным GUI находится в [`../Studio/`](../Studio/).

## Активные компоненты

- [`kaic-backend/`](kaic-backend/) — Rust-ядро Control Center (`127.0.0.1:4545`, Router, реестры, Scheduler, Task Store, MCP и медиа-пайплайн).
- [`kaic-tui/`](kaic-tui/) — лёгкий нативный Rust CLI/REPL-клиент для взаимодействия с Control Center API (`KAIC_CONTROL_TOKEN`, `/temp`, управление задачами и ресурсами).
- [`docs/`](docs/) — технические контракты, включая [`dynamic-interaction-architecture.md`](docs/dynamic-interaction-architecture.md) и [`tool-approval-contract.md`](docs/tool-approval-contract.md).
- [`experiments/`](experiments/) — автономные экспериментальные Cargo-крейты (`kaic-terminal`, `llama_cpp_smoke`).

## Быстрая проверка и запуск (Linux)

```bash
# Сборка и тесты Control Center backend и TUI-клиента
cargo test --manifest-path kaic-backend/Cargo.toml --locked
cargo test --manifest-path kaic-tui/Cargo.toml --locked

# Запуск Control Center API (требуется токен не короче 16 символов)
export KAIC_CONTROL_TOKEN="<секрет_минимум_16_символов>"
cargo run --manifest-path kaic-backend/Cargo.toml --locked
```

Для интерактивной работы с локальными (`llama.cpp`, Ollama), OpenAI-совместимыми, Anthropic и внешними CLI-агентами (`codex`, `claude`, `agy`) используйте [`kaic-studio tui`](../Studio/README.md).
