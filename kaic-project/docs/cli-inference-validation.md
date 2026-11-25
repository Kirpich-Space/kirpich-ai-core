# Проверка CLI → KAIC → llama-server

Проверено 2026-10-21. **Живой inference на GGUF: НЕ ВЫПОЛНЕН.** На проверенных
дисках не найден файл с весами. Процессные тесты ниже используют настоящий
CLI и backend KAIC, но синтетический HTTP inference-сервер. Их ответ не является
результатом локальной языковой модели.

## Фактическая среда

Artix Linux x86_64, PID 1 — runit, login shell `/usr/bin/fish`, Ryzen 7 5700X,
RAM 32007 МиБ. Установлены CUDA toolkit и драйвер NVIDIA 615.78.08.

Найден `/home/kirpich/.llama.cpp/build/bin/llama-server`, отсутствующий в PATH.
Его RUNPATH указывает на старый `/home/kirpich/llama.cpp/build/bin`; обычный
запуск завершается ошибкой загрузки `libllama-server-impl.so`. Все необходимые
библиотеки найдены рядом с бинарником. Проверенный временный обход:

```fish
env LD_LIBRARY_PATH=/home/kirpich/.llama.cpp/build/bin \
  /home/kirpich/.llama.cpp/build/bin/llama-server --version
env LD_LIBRARY_PATH=/home/kirpich/.llama.cpp/build/bin \
  /home/kirpich/.llama.cpp/build/bin/llama-server --list-devices
```

Версия: **0.7.0-dev, build 11706, commit f74e2fa6a**, GNU 16.2.1.
`CMakeCache.txt`: `GGML_CUDA=ON`, compiler `/opt/cuda/bin/nvcc`, Release.
`--list-devices` успешно перечислил CUDA0: RTX 4060 (7881 МиБ, свободно 7305).
Установка и постоянная конфигурация shell не менялись.

| Наблюдение `nvidia-smi` | Всего | Занято | Свободно |
| --- | --- | --- | --- |
| Перед тестами | 8188 МиБ | 465 МиБ | 7417 МиБ |
| После завершения | 8188 МиБ | 468 МиБ | 7415 МиБ |

Это показания драйвера для всей системы, не расход модели. Модель не
загружалась; CPU/GPU offload и её footprint не измерены. В sandbox nvidia-smi
не видел драйвер; успешные аппаратные проверки выполнены на хосте.

Работающих `llama-server`, KAIC или GPU compute-приложений перед тестом не
найдено. Порт 4545 был свободен. Поиск включал `/home/kirpich`, `/opt`, `/mnt`
и доступные runtime-каталоги, включая игнорируемые файлы. Найдено 19
`ggml-vocab-*.gguf`; чтение GGUF-заголовков подтвердило у каждого
**tensor_count=0**. Это словари без весов, непригодные для генерации.
Дополнительных смонтированных дисков нет. Несмонтированный UFS-раздел
не исследовался. Модели не скачивались, GGUF не изменялись.

## Проверенный путь исполнения

`kaic-tui/src/main.rs` → `cli::run` → `ApiClient::create_task` →
`POST /tasks` → `control_center_api::create_task` → `Router::classify` →
SQLite Task Store → фоновый executor → `Scheduler::begin_session` →
Capability Registry → Resource Registry → `LlamaServerBackend::generate` →
`generate_openai` → `POST /v1/chat/completions?autoload=false` → запись ответа
в Task Store → `kaic-cli --show UUID` или интерактивный polling.

CLI отправляет текст без принудительного `category` и без `allow_manual`.
Для использованного запроса Router выбирает **Simple**. В процессной фикстуре
Simple сопоставлен с ярлыком **Local**, provider ID — **synthetic-model**.
Scheduler проверяет discovery, configured footprint **60 МиБ** при бюджете
**100 МиБ**, выбирает уже обслуживаемую внешним backend модель и держит
generation guard до ответа. Эти числа синтетические, не оценки GGUF.

Single-model llama-server управляет загрузкой GGUF, контекстом и offload сам.
Адаптер KAIC поддерживает health/discovery/inference, отклоняет собственные
load/unload; модель не считается освобождённой при завершении задачи.
В тесте после ответа остаётся loaded=Local, used=60, running_tasks=0,
busy_models=[], active_operation=null, recovery_warnings=[].

CLI уже связан с основным executor: недостающий command family не потребовался.
LM Studio не использовался. Исходные реестры остаются рассчитанными на другие
модели; для llama-server нужны явные model ID/capabilities/footprint.
Незавершённый EmbeddedBackend и отдельная долговременная память в этот путь
не входят. MCP опционален; временный рабочий каталог теста не содержит
`config/mcp.yaml`, поэтому обычная текстовая задача не запускает инструменты.

## Исправленный дефект ответа

`generate_openai` преобразовывал отсутствующий или null `content` без
непустого `tool_calls` в `Text("")` через `unwrap_or_default`. Процессная
регрессия до исправления получила от CLI **«готово» с пустым [assistant]**;
отдельный Rust-тест также упал по поведению, exit 101.

Теперь отсутствие обоих вариантов ответа возвращает явную ошибку,
задача становится Failed, CLI показывает системную диагностику. Явная
пустая строка по прежнему контракту остаётся допустимой. При наличии
tool_calls null content также остаётся допустимым и не обрывает цикл.
Изменение относится к общему OpenAI parser, включая опциональный LM Studio.

## Сквозные проверки с дублем

Новый `kaic-backend/tests/cli_inference.py` запускает настоящие бинарники в
отдельных временных рабочих каталогах, с собственными SQLite/YAML и случайным
control token только в окружении. Inference-дубль использует свободный
динамический loopback-порт. Занятый 4545 вызывает отказ, чужие процессы не
останавливаются. Тест проверяет payload/ID/исходный текст, конечное состояние,
вывод CLI и освобождение guard; результаты можно сохранить вне Git.

```bash
python3 kaic-project/kaic-backend/tests/cli_inference.py \
  /tmp/kaic-audit-krrrb9ap/kaic-backend/target/debug/kaic \
  /tmp/kaic-tui-client-check/target/debug/kaic-cli
```

| Сценарий | Проверка |
| --- | --- |
| normal | Simple → Local → точный provider ID; один POST, русский синтетический ответ в CLI, внешняя резидентность сохраняется |
| invalid | Пустые choices → Failed; повторов генерации нет |
| null | content=null без инструментов → Failed, без ложного Done |
| http-error | Ошибка генерации HTTP 503 → Failed |
| unavailable | /health=503 → Failed до inference POST; CLI --backend-info возвращает 1 |
| timeout | Медленный /health ограничен production deadline 2 секунды; Failed, диагностический CLI возвращает 1 |
| missing | Discovery объявляет другую модель; Failed до inference POST |
| budget | Footprint 101 при бюджете 100; Failed без inference POST |
| mismatch | Simple требует необслуживаемую модель; Failed; отдельная Programming-задача успешно использует Local |

После ошибок проверяются отсутствие активных резерваций/операций и, кроме
неизменяемой бюджетной конфигурации, успешная новая задача после устранения
причины. Отказ TCP-соединения и короткие адаптерные deadlines дополнительно
покрывает существующий Rust-набор llama_server. Зависание TCP SYN в реальной
сети не моделировалось: процессный timeout проверяет срок ответа /health.

Точная CLI-команда из положительного сценария (токен задаёт harness):

```bash
/tmp/kaic-tui-client-check/target/debug/kaic-cli \
  --backend http://127.0.0.1:4545 \
  --send 'Respond with exactly one short Russian sentence explaining what KAIC is.'
/tmp/kaic-tui-client-check/target/debug/kaic-cli \
  --backend http://127.0.0.1:4545 --show 69972684-1f7c-48ba-8b7f-a7e24957b6ec
```

Наблюдённый **синтетический** результат:

```text
Задача 69972684-1f7c-48ba-8b7f-a7e24957b6ec — готово
[assistant] СИНТЕТИЧЕСКИЙ ОТВЕТ: KAIC координирует локальные модели.
```

`--send` подтверждает создание задачи, а не успешный inference. `--show`
возвращает exit 0 при успешном чтении даже Failed-задачи: нужно проверять
выведенный статус/историю. Существующие CLI-соглашения не менялись.

## Команды для будущего живого прогона (fish)

Осталось предоставить **читаемый путь GGUF с весами**, пригодный chat template
и обоснованную оценку полного footprint вместе с контекстом. Совместимость
реального discovery/chat API этой версии llama-server пока не подтверждена.
Ниже — подготовленная инструкция, не протокол выполненного GGUF-прогона.
Во втором и третьем терминале нужен одинаковый `KAIC_CONTROL_TOKEN` (>=16
символов) в окружении. Токен не следует передавать в argv или сохранять в Git.

Сначала убедитесь, что команды `ss -H -ltn 'sport = :18080'` и
`ss -H -ltn 'sport = :4545'` не показывают listeners. Если занято, не
останавливайте чужой процесс: выберите иной inference-порт и согласованно
измените URL. API-порт main KAIC пока фиксирован на 4545.

В первом терминале, выбрав настоящий путь модели:

```fish
read -P 'Абсолютный путь GGUF с весами: ' gguf_path
env LD_LIBRARY_PATH=/home/kirpich/.llama.cpp/build/bin \
  /home/kirpich/.llama.cpp/build/bin/llama-server \
  -m "$gguf_path" --alias local-model --host 127.0.0.1 --port 18080 \
  --no-webui --jinja -ngl 0 -c 2048 -n 128 -np 1
```

Команда использует CPU; GPU offload выбирается отдельно после проверки размера
модели и доступной VRAM. Флаги проверены по установленному `--help`.
Дождитесь готовности сервера; автоматическая загрузка из интернета не задаётся.

Во втором терминале (бинарники предварительно собраны командами ниже):

```fish
set -l smoke_dir (mktemp -d /tmp/kaic-live-XXXXXX)
read -P 'Полный footprint модели и KV cache в МиБ: ' footprint_mb
printf 'Local: {model_id: local-model, vram_mb: %s, load_seconds: 0, always_loaded: true, preferred_device: cpu}\n' "$footprint_mb" > "$smoke_dir/resources.yaml"
printf 'Simple: {primary: Local}\n' > "$smoke_dir/capabilities.yaml"
cd "$smoke_dir"
env -u KAIC_INFERENCE_TOKEN KAIC_MODEL_BACKEND=llama-server \
  KAIC_INFERENCE_URL=http://127.0.0.1:18080 \
  KAIC_RESOURCE_REGISTRY="$smoke_dir/resources.yaml" \
  KAIC_CAPABILITY_REGISTRY="$smoke_dir/capabilities.yaml" \
  KAIC_PREFERRED_MODEL=Local KAIC_MODEL_MEMORY_MB=8192 \
  /tmp/kaic-audit-krrrb9ap/kaic-backend/target/debug/kaic
```

8192 МиБ — пример пользовательского бюджета, не замер модели. Footprint
должен быть положительным, обоснованным и помещаться в выбранный бюджет.
Старая SQLite и исходные конфигурации проекта этим запуском не используются.

В третьем терминале с тем же control token:

```fish
set -l cli /tmp/kaic-tui-client-check/target/debug/kaic-cli
$cli --backend http://127.0.0.1:4545 --backend-info
$cli --backend http://127.0.0.1:4545 --models
$cli --backend http://127.0.0.1:4545 --send 'Respond with exactly one short Russian sentence explaining what KAIC is.'
read -P 'UUID, возвращённый --send: ' task_id
$cli --backend http://127.0.0.1:4545 --show "$task_id"
$cli --backend http://127.0.0.1:4545 --status
```

Повторяйте только `--show` до Done/Failed; не повторяйте POST для ожидания
ответа. Проверьте Simple в журнале, ID модели, ответ CLI, running=0 и busy=[].
После проверки завершите свой KAIC во втором терминале через Ctrl+C, затем
свой llama-server в первом. Проверьте порт и `nvidia-smi`. KAIC не выдаёт
отключение клиента за выгрузку модели внешнего процесса.

## Проверки сборки и границы

Rust 1.99.0, Cargo 1.99.0, Python 3.14.8. Корневого Cargo workspace нет;
backend и CLI — отдельные crates. Для команд использованы:

```bash
export CARGO_HOME=/tmp/kaic-audit-krrrb9ap/cargo-home
export CARGO_TARGET_DIR=/tmp/kaic-audit-krrrb9ap/kaic-backend/target
# Для CLI: CARGO_TARGET_DIR=/tmp/kaic-tui-client-check/target
```

| Проверка | Результат |
| --- | --- |
| `cargo build --workspace --locked --offline`, backend | exit 0, 11 прежних предупреждений |
| `cargo test --workspace --locked --offline`, backend | exit 0, 269 passed, 0 failed, 2 ignored |
| `cargo clippy --workspace --all-targets --locked --offline`, backend | exit 0; прежние 24 предупреждения bin, 50 test (14 duplicates) |
| `cargo build --workspace --locked --offline`, CLI | exit 0 |
| `cargo test --workspace --locked --offline`, CLI | exit 0, 44 passed, 0 failed, 0 ignored |
| `cargo clippy --workspace --all-targets --locked --offline`, CLI | exit 0, без предупреждений |
| `python3 tests/cli_inference.py BACKEND CLI`, из backend | exit 0, 9 процессных сценариев |
| Python `py_compile` с cache вне проекта | exit 0 |
| `fish --no-execute` для четырёх блоков инструкции | exit 0; проверка синтаксиса, не живой запуск |

Loopback-тесты выполнены с разрешённым доступом к локальным портам вне sandbox.
Два существующих ignored-теста (Blender и отдельный stdio approval) данным
этапом не запускались. Общие старые различия rustfmt не исправлялись;
производственная правка ограничена parser, его CRLF сохранён.

Журналы и отдельные SQLite/result.json — `/tmp/kaic-inference-validation/`,
без токенов. После тестов на хосте не осталось listener 4545 и процессов
`kaic`, `kaic-cli`, `llama-server`. Сквозной тест не является частью стандартного
Cargo discovery: он запускается отдельно, как существующий PTY-тест CLI.

Изменены `src/model_backend.rs`, `src/llama_server_tests.rs`, добавлены
`tests/cli_inference.py` и этот документ. Task Store, Scheduler, включая
защиту VRAM-арифметики, и зависимости не менялись. Исходные пользовательские
правки сохранены. Следующий этап — выполнить описанный живой прогон после
предоставления GGUF и проверить реальную совместимость API/chat template,
резидентность и наблюдаемый расход RAM/VRAM.
