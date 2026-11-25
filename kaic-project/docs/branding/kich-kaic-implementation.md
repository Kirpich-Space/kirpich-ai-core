# Кич в KAIC: внедрение и проверка

Дата: 2026-10-18. Основание — явное разрешение владельца на интеграцию в пустой
Workspace. Ветка `media-pipeline-and-scheduler-work`, HEAD `28f064e` не изменялись.
Kirpich MIND и исторический worktree не редактировались; commit/push не выполнялись.

## Реализованное поведение

При `task === null` существующий `Workspace` показывает оригинальный Кич,
заголовок «Начните работу с KAIC» и подсказку создать задачу вверху или выбрать
её в списке. Изображение 192×192 CSS px на desktop, квадратное, с `max-width: 100%`,
`height: auto`, `object-fit: contain` и `image-rendering: pixelated`. При ширине
до 480 px размер уменьшается; исходный PNG не обрезается и не перекрашивается.

Кич расположен внутри именованного `section` существующего `main`. Alt:
«Кич — маскот KAIC и Kirpich MIND, монитор с символом >_». Нет focus/tabindex,
анимации и live region для декоративной смены состояния. Светлый фон оригинала
сохранён; отдельная тёмная тема приложения не добавлялась.

При выборе любой задачи используется прежняя ветка истории: Кич отсутствует
даже у выбранной задачи без сообщений. Создание, polling, продолжение, детали
и действия backend не переписаны. На узкой ширине фиксированные боковые колонки
раньше вытесняли Workspace; теперь до 760 px все существующие панели идут в одну
колонку, список задач прокручивается, форма создания переносится. Desktop-сетка
сохраняется. Навигация и панели не скрываются.

## Точные файлы

Относительно корня KAIC:

| Файл | Изменение |
| --- | --- |
| `kaic-project/kaic-ui/src/assets/kich/Kich.png` | Новый; побайтовая копия канона |
| `kaic-project/kaic-ui/src/components/Kich.tsx` | Новый; небольшой img-компонент |
| `kaic-project/kaic-ui/src/components/Kich.css` | Новый; локальные стили маскота и пустого состояния |
| `kaic-project/kaic-ui/src/components/Workspace.tsx` | Импорт и замена только пустого состояния |
| `kaic-project/kaic-ui/src/App.css` | Media query для узкого окна |
| `kaic-project/docs/branding/kich-identity.md` | Ссылка на актуальный результат внедрения |
| `kaic-project/docs/branding/kich-integration-proposals.md` | KAIC отмечен реализованным; MIND остаётся предложением |
| `kaic-project/docs/branding/kich-asset-inventory.json` | Указана каноническая копия приложения и статус интеграции |
| `kaic-project/docs/branding/kich-kaic-implementation.md` | Этот отчёт |

Существующие изменения `src/api/controlCenter.ts` и `src/components/StatusBar.tsx`
сохранены побайтово. `Header`, `TaskList`, `DetailsPanel`, Electron main, backend,
P0-контракт подтверждений, package.json и package-lock.json не изменялись.

## Ассет и production URL

Источник: `/home/kirpich/Documents/Кирпич Space Company/Kich.png`.
SHA-256 источника, UI-копии и build-копии одинаков:
`df5d727a5c713484162de833e9f0dfb46121918492963eb35fbcefc748b93f2a`.
Также сохранён побайтовый дубликат в `/home/kirpich/Загрузки/Kich.png`.

Vite создал `dist/assets/Kich-DV4Uqh31.png` размером 1 410 852 байта. В JS
сгенерирован `new URL("Kich-DV4Uqh31.png", import.meta.url).href`: путь относителен
к JS в assets и подходит существующему `base: './'`. Браузер реально загрузил
этот PNG из production bundle и получил natural size 1254×1254. Локальное
разрешение такого URL для Electron `file://` проверено; сам Electron не запускался.

## Восстановление зависимостей без обновления версий

Исходный build сначала не находил `@rollup/rollup-linux-x64-gnu`. После его
восстановления обнаружился установленный Windows-бинарник esbuild вместо Linux.
Восстановлены только уже предусмотренные lockfile пакеты:

- `@rollup/rollup-linux-x64-gnu` **4.62.4** — соответствует установленному Rollup.
- `@esbuild/linux-x64` **0.21.5** — соответствует установленному esbuild.

Каждый пакет получен через npm в отдельный каталог `/tmp` с точной версией,
`--ignore-scripts --no-save --package-lock=false`, затем скопирован в отсутствующий
каталог `node_modules`. npm-cache tarball каждого пакета проверен по SHA-512
`integrity` исходного lockfile. Lockfile/package.json сохранили свои SHA-256;
глобальная установка, смена registry и массовая переустановка не выполнялись.

## Проверки и результаты

В `kaic-project/kaic-ui`:

```bash
./node_modules/.bin/tsc --noEmit --incremental false
npm run build
```

Обе команды завершились с кодом 0 после восстановления native dependencies.
Сборка: Vite 5.4.21, 41 модуль; JS около 156,23 kB, CSS 1,56 kB, PNG 1,41 MB.
Осталось существующее предупреждение Vite о deprecated CJS Node API.

Браузерная проверка выполнена в Firefox headless через локальный production HTTP
server и отдельный временный профиль, без изменения настроек основного браузера.
Реальные API-соединения блокировались тестовым CSP `connect-src 'none'`; для
проверки выбора/создания подставлялись явно тестовые ответы fetch. Реальные
модели, инструменты и внешние провайдеры не вызывались.

| Сценарий | Результат |
| --- | --- |
| Desktop, ширина 1280 CSS px | Изображение 192×192, natural size 1254×1254, alt корректен, pixelated |
| Узкий viewport 360 CSS px | Около 172,8×172,8, без горизонтального overflow, обе панели присутствуют |
| Очень узкий viewport 240 CSS px | 128×128, без горизонтального overflow, обе панели присутствуют |
| Выбор задачи на тестовых данных | Кич исчез, история и детали отображаются |
| Создание задачи на тестовых данных | Ровно один POST /tasks, введённый текст сохранён, кнопка разблокирована |
| Канонические файлы и output PNG | Полное совпадение байтов и хеша |
| Контроль области изменений | Из ранее существовавших UI-файлов изменены только App.css и Workspace.tsx |
| Whitespace изменённых файлов | Scoped git diff --check и проверка новых текстовых файлов прошли |

Firefox WebDriver ограничил обычное окно минимумом 500 px в первой попытке.
Поэтому 360/240 px проверены в same-origin iframe с настоящим production-приложением,
а не имитацией UI. Все пять сценариев затем прошли. Скриншоты desktop, узкого
пустого Workspace и активной задачи получены и визуально просмотрены.

Временные артефакты:

- `/tmp/kich-kaic-implementation-z97gkdm1/browser-results.json` — измерения/результаты.
- `/tmp/kich-kaic-implementation-z97gkdm1/desktop-empty.png` — настоящий empty state.
- `/tmp/kich-kaic-implementation-z97gkdm1/narrow-empty.png` — узкий viewport.
- `/tmp/kich-kaic-implementation-z97gkdm1/small-empty.png` — viewport 240 px.
- `/tmp/kich-kaic-implementation-z97gkdm1/desktop-active-fixture.png` — активная
  задача с явно тестовой историей; это не демонстрация inference.
- `/tmp/kich-kaic-implementation-z97gkdm1/browser_check.py` — проверочный стенд.

## Границы и оставшиеся ограничения

У активного KAIC UI нет npm test script; новый framework и зеркальные markup-tests
не добавлялись. Вместо них проверены реальные DOM/ресурсы/действия production bundle.
Screen reader вручную не запускался. Live backend/MCP, Rust tests и Linux Electron
runtime не проверялись в этом UI-изменении. Сохранность функций подтверждена в
описанной области, не всеми возможными путями приложения.

Оригинальный PNG занимает около 1,35 МиБ: оптимизация/перекодирование не делались.
Дальнейшая интеграция MIND, анимация и прозрачная версия остаются вне разрешения.
Применены `kirpich-typescript`, `kirpich-dependencies`, `kirpich-web` и ранее
прочитанные `kirpich-documentation`/`kirpich-git`. Новые subagents не запускались,
частный код внешним coding providers не передавался.
