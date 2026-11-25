# Границы безопасности и архитектура агента KAIC Studio

Этот документ описывает модель безопасности, унифицированный runtime, провайдеры, нативный TUI и честные платформенные границы `kaic-studio` (GPL-3.0-only, 100% нативный Rust без Electron/Chromium/Node.js).

## 1. Разделение политик: файловые инструменты и исполнение кода

Файловая политика (`AgentPolicy`) и политика исполнения кода (`ExecPolicy`) **независимы**. Разрешение записи файлов в workspace (`workspace-write`) **не** даёт молчаливого права на запуск пользовательского кода.

### 1.1. Политика файловых инструментов (`src/agent/policy.rs`, `src/agent/tools.rs`)
- Режимы: `read-only`, `approval-required` (по умолчанию), `workspace-write`.
- Инструменты `list_files`, `read_file`, `search_text`, `write_file`, `edit_file`, `apply_patch`, `list_skills`, `read_skill` работают строго внутри канонического `workspace` пошагово через `openat(..., O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)`, `fstat`/`fstatat(AT_SYMLINK_NOFOLLOW)` и `renameat`, исключая TOCTOU-подмену symlink.
- Защита секретов: `.env`, `.env.*`, `*.pem`, `*.key`, `id_rsa*`, `id_ed25519*`, `auth.json`, `.ssh/`, `.gnupg/`, `.codex/`, `.claude/`, `.git/config` и запись в `.git/` всегда заблокированы.

### 1.2. Политика исполнения кода (`ExecPolicy`) и классификация изоляции
- **Запрет команд не выдаётся за OS-sandbox.** Для сборочных/тестовых команд (`CommandClass::UserCodeExecution`) явно указывается `IsolationLevel::NoneUserPrivileges` («НЕТ OS-SANDBOX: права текущего пользователя»).
- Реальные сборки/тесты (`cargo`, `make`, `cmake`, `ctest`, `ninja`, `pytest`, `python3 -m unittest|pytest`, `npm`/`pnpm`/`yarn`) управляются `ExecPolicy`:
  - `disabled`, `safe-inspect`, `confirm-user-code` (по умолчанию), `allow-builds`.
- Прямые оболочки (`sh`, `bash`, `zsh`), повышение прав (`sudo`, `su`, `doas`), сетевые/деструктивные утилиты (`curl`, `wget`, `rm`, `dd`, `ssh`, `git push|reset`) и подмена конфига `git -c` запрещены политикой.

### 1.3. Одноразовые подтверждения
- Каждое подтверждение привязано к `(request_id, SHA-256(task_id || 0 || tool || 0 || canonical_args_json))` и потребляется одноразово (`ApprovalGate::verify_and_consume`).
- В нативном TUI нажатие `Enter` без явного `y`/`n`/`Esc` игнорируется и никогда не подтверждает инструмент.

## 2. Отмена и граница процессных групп (`setsid`)
- Дочерние процессы запускаются в собственной процессной группе (`setpgid(0, 0)` + `crate::platform::set_parent_death_signal()`).
- При `Esc` или таймауте `kill(-pgid, SIGKILL)` завершает дочерний процесс и потомков в группе.
- **Документированное ограничение (`detached_setsid_descendant_exposes_process_group_boundary`)**: отделившийся через `setsid()` потомок не закрывается сигналом `kill(-pgid, SIGKILL)`; для этого в будущем требуется Linux cgroup v2 (`cgroup.kill`) или PID namespace.

## 3. Мультиагентность и версионная верификация (`verified`)
- До 2 подзадач на делегирование с правами не выше родителя (`cap`), непересекающимися `owned_paths`, последовательным использованием локального `llama-server` и исходным `verified: false` с фиксацией `SHA-256` артефактов (`artifact_hashes`).
- Переход в `verified: true` требует совпадения `SHA-256`, повторного чтения файлов оркестратором и целевого успешного `run_check`. Любая последующая правка переводит статус в `invalidated_after_edit`.

## 4. Единая архитектура 7 провайдеров и ОС

В коде строго разделены:
1. **`KAIC native` (`ExecutionOrigin::KaicNativeAgent`)** — работают через собственный агентный цикл KAIC, парсер/ремонт вызовов (`tool_parser.rs`), компакцию контекста (`context.rs`), контроль циклов и пооперационное подтверждение:
   - `LocalLlama` (`llama-server` loopback `127.0.0.1`, опциональная GBNF-грамматика);
   - `OllamaLocal` (строго loopback `http://127.0.0.1:11434/api/chat`);
   - `OpenAiCompat` (`/v1/chat/completions`, loopback HTTP или удалённый HTTPS только с явным согласием);
   - `AnthropicApi` (`/v1/messages` SSE, API-ключ только из `ANTHROPIC_API_KEY`/`KAIC_ANTHROPIC_API_KEY` в памяти).
2. **`External CLI` (`ExecutionOrigin::ExternallyManagedCli`)** — запускают официальный локально авторизованный CLI с явным согласием передачи контекста, подтверждением запуска и sandbox-флагами самого CLI (без `--dangerously-skip-permissions` и без чтения чужих баз токенов):
   - `CodexCli` (`codex exec --json --sandbox <mode> --ephemeral`);
   - `ClaudeCli` (`claude -p --output-format stream-json --no-session-persistence --permission-mode <plan|acceptEdits>` + `--restricted` при read-only);
   - `AgyCli` (`agy --print --output-format stream-json --sandbox --mode <plan|accept-edits>`).
- **Linux (x86_64)**: целевая платформа сборки и CPU-тестов.
- **FreeBSD**: POSIX-код (`procctl`, `sockstat`, `termios`) добавлен в `src/platform.rs`, но **не проверялся на живом хосте FreeBSD** и честно считается непроверенным (`unverified`).

## 5. Динамическое взаимодействие (выключено по умолчанию)

Подробная спецификация: [`../kaic-project/docs/dynamic-interaction-architecture.md`](../kaic-project/docs/dynamic-interaction-architecture.md).

- `SeatManager`, `AtomicSeatGate` (`x86_64` inline ASM `lock cmpxchg`/`lock and`/`xchg` + SSE2 + переносимый Rust fallback), `VirtualPointer`, `VirtualKeyboard` и `LiveCanvas` сохранены для явной работы по требованию пользователя.
- Во всех режимах (TUI, CLI, GUI) режим динамического взаимодействия **выключен по умолчанию** (`dynamic_interaction_enabled = false`) и требует явного включения (`/dynamic on`, `--dynamic-interaction` или чекбокс в GUI).
- Планируемые системные/TTY адаптеры (`linux_uinput`, `wayland_virtual_input`, `x11_xinput2_master`, `freebsd_platform_input`, `tmux_control_mode` / `tmux -CC`) имеют статус **`planned / unavailable`** и не выполняют скрытого ввода.

## 6. Нативный терминальный интерфейс (`src/tui/`)
- Не использует внешние TUI-крейты (работает на `libc` + `tokio`), не содержит Node.js/Electron/WebView.
- Санитизирует ANSI/OSC-последовательности из недоверенного вывода моделей и инструментов (`sanitize_terminal_text`).
- Поддерживает TrueColor / ANSI-16 / `NO_COLOR`, SSH, голую TTY, многострочный ввод, автодополнение `/`-команд, сворачиваемые tool-блоки со встроенным unified diff, подсветку синтаксиса, историю, возобновление сессий и мгновенную отмену по `Esc`.
