# Точки вызова `GenerateRequest` / `GenerateResponse`

Инвентаризация под будущую задачу о клиенте внешних инструментов. При любом
выбранном варианте клиента `GenerateResponse` перестаёт быть `{content: String}`,
и это ломающее изменение задевает все точки ниже.

Список висел долгом с ранних задач и никогда не проверялся на полноту.
Здесь он подтверждён компилятором, а не чтением.

## Как подтверждена полнота

Два временных изменения, каждое откачено сразу после снятия вывода
(`git status` чист, в истории их нет).

**Проба A — переименование поля ответа.** `GenerateResponse.content` →
`content_PROBE`, затем `cargo check --tests`. Компилятор обязан назвать
каждое место, где поле конструируется или читается:

```
src\control_center_api.rs:661:26: error[E0609]: no field `content` on type `GenerateResponse`
src\control_center_api.rs:668:43: error[E0609]: no field `content` on type `GenerateResponse`
src\control_center_api.rs:531:34: error[E0609]: no field `content` on type `GenerateResponse`
src\model_backend.rs:297:31:     error[E0560]: struct `GenerateResponse` has no field named `content`
src\scheduler.rs:1405:17:        error[E0560]: struct `GenerateResponse` has no field named `content`
src\scheduler.rs:1438:17:        error[E0560]: struct `GenerateResponse` has no field named `content`
src\scheduler.rs:1516:17:        error[E0560]: struct `GenerateResponse` has no field named `content`
error: could not compile `kaic` (bin "kaic" test) due to 7 previous errors
```

**Проба B — обязательное поле в запросе.** В `GenerateRequest` добавлено
`pub probe_field: ()`. Компилятор обязан назвать каждую конструкцию:

```
src\control_center_api.rs:651:19: error[E0063]: missing field `probe_field` in initializer of `GenerateRequest`
src\media\query.rs:128:5:         error[E0063]: missing field `probe_field` in initializer of `GenerateRequest`
src\scheduler.rs:1109:25:         error[E0063]: missing field `probe_field` in initializer of `GenerateRequest`
src\scheduler.rs:1161:9:          error[E0063]: missing field `probe_field` in initializer of `GenerateRequest`
error: could not compile `kaic` (bin "kaic" test) due to 4 previous errors
```

Чего пробы по полям НЕ ловят: функции, которые тип только пропускают через
себя, ничего в нём не читая. Они добраны отдельно поиском по сигнатурам и
помечены в таблице как «сквозная».

Номера строк ниже — после коммитов этой задачи, поэтому со строками в выводе
проб они не совпадают.

## Классы изменения

- **тривиально** — правка механическая: сменить тип, взять текстовую часть,
  дописать поле. Решений не требует.
- **ветвление** — точка обязана различить «модель ответила текстом» и
  «модель просит вызвать инструмент». Появляется `match`, которого не было.
- **цикл** — точка обязана прокрутить несколько обменов с моделью, а не один.
  Здесь же живут последствия для `GenerationGuard`, Task Store и лимитов.

## Таблица

| Файл:строка | Что делает | Класс изменения |
|---|---|---|
| `src/model_backend.rs:61` | объявление `GenerateResponse { content: String }` | **ветвление** — сам тип и становится суммой «текст \| вызовы инструментов» |
| `src/model_backend.rs:85` | метод trait `ModelBackend::generate` | тривиально — меняется только сигнатура, но ломает все реализации |
| `src/model_backend.rs:280` | `LmStudioBackend::generate`, сквозная | тривиально |
| `src/model_backend.rs:317` | собирает ответ из `choices[0].message.content` | **ветвление** — здесь разбираются `tool_calls` и `finish_reason`; сегодня пустой `choices` молча даёт пустую строку (`unwrap_or_default`), и с инструментами это перестанет быть безобидным |
| `src/model_backend.rs:452` | тест таймаута, конструирует пустой запрос | тривиально |
| `src/embedded_backend.rs:41` | `todo!()`, за feature-флагом | тривиально |
| `src/scheduler.rs:585` | `Scheduler::run`, сквозная | тривиально |
| `src/scheduler.rs:686` | `Scheduler::generate` — единственное место, где живёт `GenerationGuard` | **цикл** — `drop(guard)` стоит сразу после одного вызова backend'а; при нескольких обменах guard обязан пережить их все, иначе модель вытеснят между шагами |
| `src/scheduler.rs:712` | `run_with_model`, сквозная, служебные вызовы | тривиально — при условии, что служебным шагам инструменты не даются (см. ниже) |
| `src/scheduler.rs:783` | `with_registry_temperature` | тривиально — трогает только `temperature` |
| `src/control_center_api.rs:657` | конструирует запрос основного пайплайна | **ветвление** — здесь решается, какие инструменты доступны категории задачи |
| `src/control_center_api.rs:667` | пишет в лог длину ответа | тривиально |
| `src/control_center_api.rs:674` | кладёт ответ в Task Store с ролью `assistant` | **цикл** — главная точка: цикл «модель → инструмент → модель» живёт здесь, и он требует ролей `tool`/`tool_call_id`, то есть миграции SQLite |
| `src/control_center_api.rs:537` | `extract_search_term`, берёт `response.content` | тривиально — шаг по построению одноразовый и деградирующий: ответ не-текстом трактуется как «извлечение не удалось», дальше идёт дословный текст задачи |
| `src/media/query.rs:127` | `build_request` для извлечения термина | тривиально — инструментов этот шаг не объявляет |
| `src/scheduler.rs:1109` | тест `at_most_one_load_runs_at_a_time` | тривиально |
| `src/scheduler.rs:1160` | тестовый хелпер `empty_request` | тривиально |
| `src/scheduler.rs:1404` | подставной `AlwaysOkBackend` | тривиально |
| `src/scheduler.rs:1437` | подставной `SleepyBackend` | тривиально |
| `src/scheduler.rs:1478` | подставной `FailingBackend`, возвращает `Err` | тривиально |
| `src/scheduler.rs:1515` | подставной `SlowGenerationBackend` | тривиально |
| `src/scheduler.rs:1555` | подставной `FailingGenerationBackend`, возвращает `Err` | тривиально |

Итого 22 точки: 3 требуют ветвления, 2 требуют цикла, остальные механические.

## Что из этого следует для задачи о клиенте

1. Настоящих мест принятия решений всего два — `control_center_api.rs:674`
   (цикл и Task Store) и `model_backend.rs:317` (разбор ответа провайдера).
   Всё остальное правится механически и объёма задачи не определяет.
2. `GenerationGuard` (`scheduler.rs:686`) — не деталь: сегодня он живёт ровно
   один вызов backend'а. Цикл обяжет держать его дольше, а это напрямую
   задевает `MAX_CONCURRENT_TASKS`.
3. Служебный контур извлечения термина (`media/query.rs` + `run_with_model`)
   инструменты получать не должен — тогда он остаётся тривиальным. Если
   решение будет обратным, `run_with_model` переезжает в класс «цикл».
