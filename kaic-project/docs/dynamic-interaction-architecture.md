# Архитектура динамического взаимодействия KAIC и планируемые адаптеры ОС/TTY

## 1. Статус и ключевой инвариант

**Режим динамического взаимодействия (мульти-курсоры и виртуальные клавиатуры) ВЫКЛЮЧЕН ПО УМОЛЧАНИЮ** во всех точках входа:
- GUI (`eframe/egui`): `ChatOptions::dynamic_interaction_enabled = false`;
- Нативный TUI (`kaic-studio tui`): `TuiConfig::dynamic_interaction = false` (переключение только явной командой `/dynamic on`);
- CLI (`kaic-studio agent run`): `AgentLimits::default().dynamic_interaction_enabled = false` (включение только явным флагом `--dynamic-interaction`).

Пока режим выключен:
- `SeatManager::apply_live_edit` отклоняет операции редактирования;
- виртуальные указатели и клавиатуры агентов (`VirtualPointer`, `VirtualKeyboard`) находятся в неактивном состоянии (`enabled: false`);
- ни одно фоновое действие не выполняет скрытого управления вводом.

## 2. Текущая нативная подсистема (`Studio/src/agent/workspace_live.rs`, `asm_accel.rs`, `ui/live_canvas.rs`)

- **Места агентов (`MAX_AGENT_SEATS = 4`)**: `SeatManager` выделяет до четырёх изолированных мест (`agent-1..agent-4`) с непересекающимися `owned_paths`, визуальными координатами (`VirtualPointer`) и буферами ввода (`VirtualKeyboard`).
- **Аппаратный арбитраж (`AtomicSeatGate`)**:
  - На `x86_64` используется inline ASM (`lock cmpxchg`, `lock and`, `xchg` с неявным `LOCK`), SSE2 (`asm_count_byte`, `asm_common_prefix_len`) и `lfence + rdtsc`.
  - Для не-`x86_64` архитектур (включая ARM64 и прочие целевые сборки без x86_64) реализован идентичный переносимый Rust-fallback (`AtomicU32` + скалярный поиск) — подтверждено тестом `portable_fallback_matches_x86_64_asm_for_all_edge_cases`.
- **Границы файловых операций**: динамические правки (`LiveEditOp`) выполняются строго внутри канонического workspace через `openat(O_NOFOLLOW)` и `renameat`, с эксклюзивной блокировкой файла на одно место.
- **Аварийный стоп**: `Esc` в TUI, `/dynamic stop`, кнопка стопа в GUI или `SeatManager::emergency_stop()` мгновенно сбрасывают аппаратный шлюз, отключают все места и снимают файловые блокировки.

## 3. Планируемые платформенные бэкенды (`planned / unavailable`)

Ни один из перечисленных ниже системных драйверов ввода в текущей сборке **не реализован** и честно помечается `probe_os_input_backends()` как `FeatureStatus::Unavailable`. Включение их в будущем возможно только при сохранении явного согласия пользователя и мгновенного аварийного отключения:

| Бэкенд | Целевая платформа | Механизм / протокол | Статус |
|---|---|---|---|
| **Wayland Virtual Input** | Linux / FreeBSD (wlroots / портал) | `zwlr_virtual_pointer_v1`, `zwp_virtual_keyboard_v1`, `xdg-desktop-portal` (`RemoteDesktop`) | `planned / unavailable` |
| **X11 XTest / XInput2 MPX** | Linux / FreeBSD (Xorg) | `XTest` синтез ввода или отдельные Master/Slave устройства `XInput2` | `planned / unavailable` |
| **Linux `uinput`** | Linux | `/dev/uinput` (виртуальные evdev-устройства с явной проверкой прав) | `planned / unavailable` |
| **FreeBSD OS Input** | FreeBSD | Адаптеры `evdev`/`uinput`, Wayland или X11 поверх FreeBSD | `planned / unavailable` |
| **tmux Control Mode (`tmux -CC`)** | Linux / FreeBSD (TTY / SSH) | Выделенные PTY-панели для субагентов под управлением пользователя через контрольный протокол `tmux -CC` | `planned / unavailable` |

### Целевая модель интеграции `tmux Control Mode` (`tmux -CC`)

Для работы нескольких терминальных агентов в одном TTY/SSH-подключении без вмешательства в графический рабочий стол:
1. Каждому пишущему или тестовому субагенту выделяется отдельная панель в изолированной пользовательской tmux-сессии (`tmux -CC new-session`).
2. Права на запись команды в панель сопоставляются с `SeatManager` (`slot 0..3`) и требуют активного `dynamic_interaction_enabled = true`.
3. `Esc` или `/dynamic stop` немедленно отключает канал управления контрольным клиентом (`detach-client`) и убивает процессную группу активной панели.
