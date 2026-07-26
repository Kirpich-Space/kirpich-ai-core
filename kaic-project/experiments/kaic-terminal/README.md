# KAIC Terminal v0.2

Локальный AI CLI: открывает проект, понимает его структуру, позволяет
общаться с моделью в контексте проекта. Inference-движок — прямое
продолжение `experiments/llama_cpp_smoke`, логика генерации не менялась.

## Сборка (Windows)

Из твоей уже пройденной диагностики `llama_cpp_smoke` известно, что нужно
собирать через генератор Ninja, а не Visual Studio/MSBuild (у последнего
баг с флагом `-j16` в крейте `cmake`). Открой **Developer Command Prompt
for VS 2022** и выполни:

```cmd
cd kaic-terminal
set CMAKE_GENERATOR=Ninja
set LIBCLANG_PATH=C:\Program Files\LLVM\bin
copy kaic.toml.example kaic.toml
```

Поправь `model_path` в `kaic.toml` под свою модель, затем:

```cmd
cargo run -- .
```

Для CUDA:

```cmd
cargo run --features cuda -- .
```

## Использование

```
kaic .                  — открыть текущую директорию как проект
kaic /путь/к/проекту    — открыть проект по указанному пути
kaic --help             — справка
kaic --version          — версия
```

Внутри REPL: `/help`, `/clear`, `/model`, `/temp N`, `/history`, `/read F`,
`/ls [D]`, `project`, `/exit`.

## Что сознательно НЕ реализовано в v0.2

Planner/Coder/Reviewer, выполнение shell-команд, автоматическое изменение
файлов, git-интеграция, память между сессиями, EmbedIDE, MCP, автономный
вызов инструментов моделью (read_file/list_directory вызываются только
вручную пользователем через `/read` и `/ls`).
