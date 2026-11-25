# KAIC с headless llama.cpp

Основной inference backend — upstream open-source `llama-server`, интерфейс
пользователя — `kaic-cli`. Основной процесс не запускает старый Telegram bridge. LM Studio не требуется. Старый LM Studio adapter
остаётся доступен через `KAIC_MODEL_BACKEND=lm-studio`. `EmbeddedBackend` по-прежнему
является отдельным незавершённым каркасом; этот путь его не использует.

## Контракт и границы

Этот адаптер поддерживает **single-model mode**: `/health`, `/v1/models`,
`/v1/chat/completions`. Точный `id`, `architecture` и `meta` discovery сохраняются;
capabilities для Router задаёт оператор в YAML. Discovery не означает, что
модель проверена на программировании или других категориях.

Загрузка GGUF, GPU offload, размер контекста и выгрузка принадлежат внешнему
процессу `llama-server`. KAIC не вызывает фиктивные `load/unload`, не останавливает
этот процесс и не считает его память освобождённой. Scheduler учитывает полный
заданный footprint обслуживаемой модели, запрещает её вытеснение, проверяет
endpoint перед работой и сохраняет ограничения одновременных задач.
Это учёт по конфигурации, а не измерение RAM/VRAM.

Upstream также имеет отдельный router mode с `/models/load` и `/models/unload`.
Он **не поддержан** этим этапом: его autoload и auto eviction могут нарушить
учёт Scheduler. Несколько моделей или router status в discovery отклоняются;
inference отправляется с `autoload=false`. Не включайте sleep/idle unload
для этого single-model endpoint. Поля `loaded` в `/models` и `/status` отражают
последнее успешное наблюдение, а `/backend` выполняет свежую проверку связи.

Проверенный источник API: [upstream server README](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md),
разделы Health, OpenAI-compatible endpoints и Router mode; проверено 2026-10-20.
Версию своего `llama-server` сохраняйте вместе с результатами живого прогона.

## Запуск

Модель не скачивается автоматически. Установите upstream llama.cpp отдельно и
выберите уже имеющийся GGUF с chat template. Пример запуска без графического
интерфейса и без GPU:

```bash
llama-server --version
llama-server -m /absolute/path/model.gguf --alias local-model \
  --host 127.0.0.1 --port 8080 --no-webui --jinja -ngl 0 -c 4096 -n 256
```

Для GPU задайте параметры самого llama.cpp согласно своему оборудованию.
Встроенные инструменты/agents llama-server не включайте: MCP и подтверждения
исполнения принадлежат KAIC.

Из `kaic-project/kaic-backend`:

```bash
cp config/llama-resource.example.yaml config/llama-resource.yaml
cp config/llama-capability.example.yaml config/llama-capability.yaml
```

В `llama-resource.yaml` **обязательно** задайте `vram_mb`: полный footprint
модели плюс KV cache в МиБ. Нулевое значение в примере намеренно запрещает
исполнение до настройки. Сверьте `model_id` с `--alias` и discovery. Это
нейтральное имя YAML-поля; прежнее `lm_studio_key` также читается.
В capability YAML оставьте только категории, на которых выбранная модель
проверена. Ярлык `Local` — имя KAIC, `local-model` — точный ID провайдера.

```bash
# KAIC_CONTROL_TOKEN уже должен содержать ваш локальный секрет (>=16 символов).
export KAIC_MODEL_BACKEND=llama-server
export KAIC_INFERENCE_URL=http://127.0.0.1:8080
export KAIC_RESOURCE_REGISTRY=config/llama-resource.yaml
export KAIC_CAPABILITY_REGISTRY=config/llama-capability.yaml
export KAIC_PREFERRED_MODEL=Local
# Задайте свой бюджет памяти под модели в МиБ, с резервом для ОС.
export KAIC_MODEL_MEMORY_MB=8192
cargo run --locked --offline
```

8192 выше — пример бюджета, а не автоматическая оценка вашей машины.
`KAIC_PREFERRED_MODEL` выбирает ярлык из уже зарегистрированных primary/fallback
цепочек; новые capabilities и разрешения `manual_only` при этом не создаются.
Для смены GGUF остановите работу задач и согласованно измените внешний сервер,
`model_id` и capabilities. Горячее переключение моделей этим этапом не добавлено.

Backend по умолчанию выбирает llama-server на `127.0.0.1:8080`; пути реестров
без переопределения остаются прежними. Для нового развёртывания используйте
явные пути выше, чтобы не подменять существующую конфигурацию LM Studio.
Опциональный `KAIC_INFERENCE_TOKEN` передаётся только заголовком Bearer.
Для llama-server разрешён только loopback HTTP без URL credentials, proxy и retries.
Диагностические HTTP-запросы ограничены двумя секундами каждый, генерация — 30 минутами.
Если inference недоступен, KAIC сохраняет доступность своего API для диагностики;
задачи получают ошибку, а не успешный пустой ответ.

## Работа через CLI

В другом терминале с тем же `KAIC_CONTROL_TOKEN`, из корня репозитория:

```bash
cargo run --locked --offline --manifest-path kaic-project/kaic-tui/Cargo.toml --bin kaic-cli -- --backend-info
cargo run --locked --offline --manifest-path kaic-project/kaic-tui/Cargo.toml --bin kaic-cli -- --models
cargo run --locked --offline --manifest-path kaic-project/kaic-tui/Cargo.toml --bin kaic-cli
```

В интерактивном CLI: `/backend` проверяет связь и discovery; `/models` показывает
реестр и последнее наблюдение загрузки. Введите простой вопрос и дождитесь
статуса «готово» и ответа через polling. `/status` показывает Scheduler.
Одноразовый `--send "Сколько будет 2 + 2?"` возвращает UUID без ожидания ответа;
получите результат через `--show UUID` или `/use UUID` в интерактивном CLI.
`--backend-info` возвращает код 1 при недоступном inference backend.

Это инструкция воспроизведения **живого** прогона. Автотесты используют HTTP-дубли
и не доказывают качество chat template, tool calling либо реальный footprint GGUF.

## Task Store после восстановления сборки

Старая SQLite сохраняется: миграция добавляет только недостающие
`acknowledgement_id` и `orphan_requests` в существующую `task_recovery`.
`/recovery` показывает legacy-квитанцию, `/ack UUID` подтверждает ознакомление.
Receipt и ledger unknown закрываются отдельно. Повторный ACK не выдаёт разрешения,
старый запрос не исполняется повторно. Незавершённые ответы получают явную пометку
неизвестного исхода; повреждённые записи при подготовке provider transcript
отображаются как системные заметки. История и подтверждения не превращаются
в автоматическое возобновление после аварии.
