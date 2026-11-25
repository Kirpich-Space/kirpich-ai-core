# OpenCode / OpenTUI: лицензии и происхождение

Проверено 2026-10-18. Область: небольшая выборка публичных первичных материалов. Клонирования репозиториев, импорта кода, запуска сборки и установки зависимостей не было.

## Зафиксированные ревизии

GitHub API read-only вернул:

| Проект | Ветка | SHA | Committer date UTC |
|---|---|---|---|
| anomalyco/opencode | dev | `214fb7fdbab9ad13773629839f1178382400cf97` | 2026-10-18 09:54:18 |
| anomalyco/opentui | main | `f8f608e2b9a41685d613a1156708ff06c826c3ce` | 2026-10-17 15:11:13 |

Источники разрешения ссылок: [OpenCode commit API](https://api.github.com/repos/anomalyco/opencode/commits/dev), [OpenTUI commit API](https://api.github.com/repos/anomalyco/opentui/commits/main). Moving branch/docs могут измениться. Web-кэш первоначально показывал OpenCode TUI 2.0.29; прямое чтение закреплённого файла подтвердило **2.0.32**. Для выводов о package version использована закреплённая версия.

## Проверенные лицензии

| Объект | Проверенный результат | Обязательства / пределы |
|---|---|---|
| [OpenCode root LICENSE](https://github.com/anomalyco/opencode/blob/214fb7fdbab9ad13773629839f1178382400cf97/LICENSE) | MIT; copyright `(c) 2025 opencode` | При копировании ПО или существенных частей сохраняются copyright и permission notice; действует отказ от гарантий |
| [OpenCode TUI manifest](https://github.com/anomalyco/opencode/blob/214fb7fdbab9ad13773629839f1178382400cf97/packages/tui/package.json) | `license: MIT`, `private: true`, 2.0.32 | Private означает workspace/package publishing status; не является обещанием стабильного публичного API |
| [OpenTUI root LICENSE](https://github.com/anomalyco/opentui/blob/f8f608e2b9a41685d613a1156708ff06c826c3ce/LICENSE) | MIT; copyright `(c) 2025 opentui` | Те же MIT notice obligations; отдельная лицензия от OpenCode |
| [OpenTUI Core manifest](https://github.com/anomalyco/opentui/blob/f8f608e2b9a41685d613a1156708ff06c826c3ce/packages/core/package.json) | `@opentui/core` 0.6.6, MIT | Зависимости: bun-ffi-structs 0.3.1, diff 9.0.0, marked 17.0.1, string-width 7.2.0; их лицензии не проверялись, поскольку пакет не импортируется |

Корневая MIT не заменяет лицензии зависимостей, vendored-кода, ресурсов и отдельных компонентов. Совместимость с конкретной моделью распространения KAIC должна оцениваться после определения набора переносимых файлов. В этом этапе **ничего из OpenCode/OpenTUI не скопировано в реализацию KAIC**; таблица не является полным SBOM или юридическим заключением.

## Контрольные суммы прочитанных файлов

SHA-256 вычислены по байтам, полученным с raw.githubusercontent.com для указанных SHA:

| Файл | SHA-256 |
|---|---|
| OpenCode LICENSE | `625f0f619133f89bbbb2abe37369613dfa1885eba1e50d02170deb62bb42cb6b` |
| OpenCode packages/tui/package.json | `192a3ae6a2abfe18627209e8e9004ba5161e6b5a7ade37395979d6b3e8ca0dd1` |
| OpenCode packages/tui/src/routes/session/index.tsx | `d7a7f5ba7a7714c4bdd596fd726c2091d49109d99095454379474a7f11150985` |
| OpenTUI LICENSE | `d9c397a5dc5ac77a2a08fc967ea06a65317a4b5b8de48f6f732ffcb21737e3d2` |
| OpenTUI packages/core/package.json | `aa3d9a24e9d760c3e1f75d99b5887f9ae8d55021cdbf41d0260944eabe12a792` |
| OpenTUI README.md | `58406292051765317e28854704537567789da79297c0d82d586a65e7f6e5c6f4` |

## Правила последующего reuse

Перед импортом зафиксировать commit/package version, исходный путь и хеш; проверить LICENSE/NOTICE на уровне выбранных файлов и всех поставляемых зависимостей; сохранить обязательные уведомления; отдельно проверить assets и trademark. Названия/логотипы OpenCode и Claude Code не становятся брендингом KAIC. Публичные идеи интерфейса реализуются собственным кодом с честным указанием design references.
