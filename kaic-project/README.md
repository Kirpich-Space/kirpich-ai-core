# KAIC — первый вертикальный срез

Архив содержит три независимых проекта:

- `kaic-backend/` — Rust-ядро (Router, Scheduler, Task Store, Control Center API).
- `kaic-ui/` — Electron + React + TypeScript интерфейс поверх него.
- `experiments/llama_cpp_smoke/` — полностью автономный проект для низкоуровневой
  проверки `llama-cpp-4`.
- `kaic-backend/src/embedded_backend.rs` — штатная опциональная реализация
  `ModelBackend` через llama.cpp; smoke-проект остаётся отдельным диагностическим
  инструментом, а не production execution path.

## Порядок запуска: kaic-backend + kaic-ui (через LM Studio)

### 1. LM Studio

Запусти LM Studio локально. В настройках (Developer / Local Server) обязательно
отключи:

- **JIT Loading** — выключено;
- **Auto Evict / Idle TTL** — выключено.

Иначе LM Studio будет управлять загрузкой/выгрузкой моделей параллельно с
нашим Scheduler'ом, и их решения будут конфликтовать друг с другом.

Загрузи (скачай, не обязательно загружай в память вручную — этим займётся
Scheduler) те модели, которые перечислены в
`kaic-backend/config/resource_registry.yaml`.

### 1a. Секрет Control Center — обязателен только для server mode

Control Center API слушает `127.0.0.1:4545` и требует предъявить секрет. Он
читается из переменной окружения — той же конвенцией, что `TELOXIDE_TOKEN`
у Telegram Bridge, и по той же причине: секреты не живут в файлах конфигурации.

```powershell
$env:KAIC_CONTROL_TOKEN = "<не менее 16 символов>"
```

В Linux задай одно runtime-значение в shell, из которого запускаются
оба процесса:

```bash
export KAIC_CONTROL_TOKEN='<сгенерируйте секрет длиной не менее 16 символов>'
```

Не записывай это значение в `.env`, YAML или команду в Git. `cargo run` и
`npm run electron:dev`, запущенные из этого shell (или его дочерних shell),
наследуют один и тот же секрет. Backend проверяет его, Electron main
добавляет `Authorization: Bearer ***` к каждому запросу. Renderer получает
через context-isolated preload только фиксированные типизированные операции — не сам
секрет, URL или произвольный HTTP-запрос. Если переменная не задана, backend
не стартует, а UI показывает причину без какого-либо значения секрета.

Без переменной server mode не поднимается вовсе, и это не строгость ради строгости.
Terminal mode (`--terminal`) не открывает HTTP-порт и этого секрета не требует.
«Только localhost» не защищает от браузера: страница, открытая пользователем,
работает на его же машине и обращается к `127.0.0.1` как к соседу. Пока доступ
был анонимным, любая такая страница могла поставить задачу от имени
пользователя — а `CorsLayer::permissive()` дополнительно позволял ей прочитать
очередь задач. Анонимного режима нет, чтобы дыру нельзя было вернуть
забывчивостью.

Каждый запрос к API должен нести заголовок:

```
Authorization: Bearer <KAIC_CONTROL_TOKEN>
```

Браузеру ответы отдаются только для origin из явного списка: `5173` (Vite
dev-сервер kaic-ui) и `4545`, в обеих формах записи петлевого адреса.

### 2. Проверка конфигов — обязательный ручной шаг

Открой:

- `kaic-backend/config/capability_registry.yaml`
- `kaic-backend/config/resource_registry.yaml`

Имена моделей там (`Fable9B`, `GLM47`, `GPTOSS20`, `Gemma27`, `GLM51`,
`Qwen40B`, `Nemotron`) — мнемоники, придуманные при проектировании. Они
должны **точно** (посимвольно) совпадать с тем, как LM Studio называет эти
модели у тебя (обычно вида `publisher/model-name`). Без этого `Scheduler`
не сможет их найти — `GET /models` и создание задач будут падать с ошибкой
"не удалось подобрать модель".

Также проверь `vram_mb`/`ram_offload_mb` под свою реальную конфигурацию
(в файле сейчас — ориентировочные цифры под RTX 4060 8GB).

Канонический каталог локальных файлов моделей для этой машины: `/mnt/storage/Models`.
Это только соглашение для конфигурации/операторов: KAIC не перемещает, копирует и
не удаляет файлы. Пути конкретных GGUF по-прежнему задаются провайдером
(LM Studio) или `KAIC_MODEL_PATH` для embedded smoke-пути.

### 3. Backend

```bash
cd kaic-backend
# KAIC_CONTROL_TOKEN уже export'нут в шаге 1a
cargo run
```

Слушает `127.0.0.1:4545`. При старте подгружает `always_loaded` модели
(сейчас — `Nemotron`, `Fable9B`) — если тут упадёт с ошибкой про нехватку
VRAM или "не удалось загрузить модель", значит имена/размеры в
`resource_registry.yaml` не совпадают с реальностью (см. шаг 2).

### 3a. Минимальный terminal agent

Тот же Rust binary запускает REPL без Control Center token и без HTTP-порта:

```bash
cd kaic-backend
cargo run -- --terminal
# эквивалентно: cargo run -- terminal
```

Terminal input проходит через штатные `Router → Scheduler → ModelBackend →
agent/tool loop → TaskStore`, а не через отдельный текстовый parser. Последующие
реплики продолжают ту же сохранённую задачу. `exit`, `quit` и EOF завершают
REPL штатно; Ctrl+C на prompt завершает REPL, а во время хода сохраняет задачу
как `Cancelled`, дожидается cooperative cleanup pipeline и продолжает с новой
terminal-сессией. Уже начавшийся внешний MCP-вызов
может иметь необратимый побочный эффект — отмена не является rollback внешней
системы. Ошибки model/backend отображаются и не убивают следующий ход.
Интерфейс использует цвет только в настоящем TTY; pipe/redirect остаются без
ANSI-кодов. Красно-оранжевая KAIC-палитра включается только в interactive TTY.
Во время хода показывается 80-мс spinner без очистки соседних строк. В TTY Tab
дополняет slash-команды и реальные model labels из текущего `Resource Registry`;
UTF-8 ввод, Backspace, Ctrl+C и Ctrl+D обрабатываются локальным line editor без
shell-команд и без новой crate-зависимости.
Доступны локальные команды `/help`, `/models`, `/status`, `/clear`, `/exit`. `/models` показывает модели из текущего
`Resource Registry`; это список настроенных ресурсов, а не обещание, что provider
сейчас запущен. Фактическую доступность, память, LRU и загрузку проверяет штатный
Scheduler при ходе. Выбранная модель действительно передаётся в agent session и
фиксируется в execution metadata. Смена модели (включая возврат в `auto`) начинает
новую persisted terminal-сессию, чтобы не смешивать контекст разных моделей.

Tool calls используют provider-native `GenerateResponse::ToolCalls`. Инструменты,
перечисленные в `config/mcp.yaml` в `auto_approve`, выполняются тем же MCP/Tool
Gate путём, что API-задачи. Первый terminal slice пока не спрашивает разрешение
интерактивно: вызов вне `auto_approve` сохраняется в `WaitingForHuman`, и этот
terminal-диалог блокируется без изменения pending-вызова. Terminal mode сам не
поднимает Control Center API; для решения нужно завершить REPL и отдельно
запустить server mode с Control Center-клиентом. Прямое подтверждение в REPL —
следующий незавершённый milestone.

### 4. Electron UI

```bash
cd kaic-ui
npm install
# запускай из shell, где export'нут тот же KAIC_CONTROL_TOKEN
npm run electron:dev
```

Откроется окно с интерфейсом. Renderer вызывает preload-операции, а Electron
main обращается к `http://127.0.0.1:4545` с Bearer-заголовком.

## Smoke-test llama-cpp-4 (не зависит от остального проекта)

Проверяет саму библиотеку в изоляции: реально ли собирается, реально ли
работает CUDA, реально ли открывается GGUF, реально ли автоматически
применяется chat template, проходит ли генерация без сюрпризов.

```bash
cd experiments/llama_cpp_smoke

# CPU-сборка
cargo run -- /путь/к/модели.gguf
cargo run -- /путь/к/модели.gguf "свой вопрос модели"

# CUDA-сборка (требует установленный CUDA Toolkit для компиляции)
cargo run --features cuda -- /путь/к/модели.gguf
```

Программа печатает: время загрузки модели, промпт после `apply_chat_template`
(проверь глазами, что формат под твою модель — Gemma/Qwen/GLM — выглядит
правильно), сам сгенерированный ответ, число токенов и токенов/сек.

Три вещи в коде помечены явно как непроверенные компиляцией (собраны по
фрагментам документации `crates.io`, не через реальную сборку) — если что-то
не соберётся с первого раза, вероятная причина — расхождение имён
методов/типов между версией `llama-cpp-4`, зафиксированной в `Cargo.toml`
(`0.4.2`), и той, что реально подтянется.

## Локальный EmbeddedBackend (llama.cpp)

После сборки с feature `embedded-backend` backend можно переключить с LM Studio
на локальный GGUF без изменения Scheduler/API:

```bash
export KAIC_CONTROL_TOKEN='локальный секрет длиной не менее 16 символов'
export KAIC_BACKEND=embedded
export KAIC_EMBEDDED_MODEL_PATH='/путь/к/model.gguf'
export KAIC_EMBEDDED_GPU_LAYERS=0
cargo run --release --features embedded-backend
```

`KAIC_EMBEDDED_GPU_LAYERS=0` означает CPU. Положительное значение включает
оффлоад указанного числа слоёв на CUDA при наличии CUDA Toolkit в окружении
сборки. По умолчанию `KAIC_BACKEND=lmstudio`, поэтому старый режим не меняется.

Если у модели в `config/resource_registry.yaml` заполнено поле `path`, Scheduler
передаст этот путь EmbeddedBackend при загрузке. `KAIC_EMBEDDED_MODEL_PATH`
остаётся fallback для smoke и конфигурации с одной моделью.

`/status` дополнительно показывает фактический backend (`lmstudio` или `embedded`),
его health/initialized, имена и количество resident-моделей, настроенное число GPU
layers и, если llama.cpp уже предоставляет это для загруженной модели, безопасную
сводку устройств/offload. Для embedded конфигурация также содержит путь модели.
Поле `capabilities.tool_calling` сообщает, может ли выбранный backend принимать
инструменты: сейчас LM Studio сообщает `supported`, а EmbeddedBackend —
`unsupported`. При передаче инструментов EmbeddedBackend возвращает структурированную
ошибку, а не пытается угадать формат tool call. Метрики не вызывают дополнительных
native API вне безопасных методов llama-cpp.

Ответы Task API также содержат необязательное поле `execution: { backend, model }`.
Оно вычисляется из сохранённого execution metadata и отсутствует у старых или ещё
не запускавшихся задач; схема SQLite при этом не меняется.

### CUDA-вариант

Для CUDA нужен тот же локальный Toolkit и feature `embedded-backend-cuda`:

```bash
export KAIC_BACKEND=embedded
export KAIC_EMBEDDED_MODEL_PATH='/путь/к/model.gguf'
export KAIC_EMBEDDED_GPU_LAYERS=35
cargo run --release --features embedded-backend-cuda
```

## Известные ограничения первого среза (не баги, осознанные упрощения)

- Между статусом в списке задач и в панели деталей возможна рассинхронизация
  до ~1 секунды (независимый поллинг).
- `DetailsPanel` может не отображать `execution` в старом UI-сценарии, хотя
  backend уже отдаёт фактические `backend` и `model` в Task API.
- Ошибка сети при обновлении открытой задачи не показывается явно — просто
  остаются последние известные данные.
- `Scheduler` сериализует весь цикл (подбор модели + генерация) под одной
  блокировкой — пока одна задача генерирует ответ, остальные ждут, даже
  если могли бы использовать другую, уже загруженную модель.
- В `TaskStore` нет механизма миграции схемы SQL — при изменении схемы
  Task (например, будущее поле `parent_task_id` для Mission) существующий
  `kaic.db` не обновится автоматически.
