//! CLI-first вход в KAIC. Все полномочия и исполнение остаются в backend.
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::time::Duration;

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::app::{App, Command, MAX_INPUT_BYTES};
use crate::client::{ApiClient, ApiError};
use crate::protocol::{BackendStatus, Task, TaskRecovery, TaskStatus};
use crate::text::{approval_text, sanitize};

const HELP: &str = "KAIC — терминальный клиент\n\
Запуск: kaic-cli [--backend URL] [--status | --backend-info | --models | --local-models | --tasks | --show UUID | --send ТЕКСТ]\n\
Без действия — диалог. KAIC_CONTROL_TOKEN обязателен; KAIC_API_BASE задаёт loopback URL.\n\
Текст создаёт задачу или продолжает выбранную завершённую/приостановленную.\n\
/new — новая беседа; /tasks — задачи; /use UUID — выбрать; /history — история\n\
/status — Scheduler; /models — реестр моделей; /temp МОДЕЛЬ <0..2|default> — температура; /refresh — восстановить соединение\n\
/local-models — локальные GGUF: каталог, метаданные и причины отклонения\n\
/backend — связь с inference backend, discovery и ограничения lifecycle\n\
/pause — пауза; /cancel — отмена выбранной задачи\n\
/recovery — исходы после сбоя; /ack UUID — подтвердить ознакомление с исходом\n\
/tool — точный запрос инструмента; /more — следующая страница\n\
/approve UUID или /deny UUID — подготовить решение; /confirm UUID — отправить\n\
/close — закрыть просмотр запроса; /help — справка; /quit или EOF — выход\n\
Решения доступны только с интерактивными stdin/stdout. Enter не одобряет.\n\
История обновляется polling; API не предоставляет token streaming, skills и субагентов.\n\
Выход и Ctrl+C не отменяют задачу backend. Токен не передавайте через argv.";

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Interactive,
    Help,
    Status,
    BackendInfo,
    Models,
    LocalModels,
    Tasks,
    Show(Uuid),
    Send(String),
}

fn parse(args: Vec<String>) -> Result<(String, Action), &'static str> {
    let mut base =
        std::env::var("KAIC_API_BASE").unwrap_or_else(|_| "http://127.0.0.1:4545".into());
    let mut action = Action::Interactive;
    let mut args = args.into_iter();
    let mut backend_seen = false;
    while let Some(arg) = args.next() {
        let next = match arg.as_str() {
            "--backend" if !backend_seen => {
                base = args.next().ok_or("После --backend нужен URL")?;
                backend_seen = true;
                continue;
            }
            "--help" | "-h" => Action::Help,
            "--status" => Action::Status,
            "--backend-info" => Action::BackendInfo,
            "--models" => Action::Models,
            "--local-models" => Action::LocalModels,
            "--tasks" => Action::Tasks,
            "--show" => Action::Show(
                args.next()
                    .and_then(|s| Uuid::parse_str(&s).ok())
                    .ok_or("После --show нужен UUID задачи")?,
            ),
            "--send" => Action::Send(args.next().ok_or("После --send нужен текст")?),
            _ => return Err("Неизвестный аргумент; используйте --help"),
        };
        if action != Action::Interactive {
            return Err("Укажите только одно действие");
        }
        action = next;
    }
    Ok((base, action))
}

pub async fn run(args: Vec<String>) -> u8 {
    let (base, action) = match parse(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return 2;
        }
    };
    if action == Action::Help {
        println!("{HELP}");
        return 0;
    }
    let token = match std::env::var("KAIC_CONTROL_TOKEN") {
        Ok(token) => token,
        Err(_) => {
            eprintln!("Задайте KAIC_CONTROL_TOKEN длиной минимум 16 символов");
            return 1;
        }
    };
    let client = match ApiClient::new(&base, token) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    let result: Result<(), Box<dyn std::error::Error>> = async {
        match action {
            Action::Help => unreachable!(),
            Action::Interactive => Session::new(client).run().await?,
            Action::Status => print_status(&client.status().await?)?,
            Action::BackendInfo => {
                let info = client.inference_backend().await?;
                print_backend(&info)?;
                if info.connected == Some(false) {
                    return Err("inference backend недоступен; проверьте /backend".into());
                }
            }
            Action::Models => print_models(&client.list_models().await?)?,
            Action::LocalModels => print_local_models(&client.local_models().await?)?,
            Action::Tasks => print_tasks(&client.list_tasks().await?)?,
            Action::Show(id) => print_task(&client.get_task(id).await?, 0)?,
            Action::Send(text) => print_task(&client.create_task(&text).await?, 0)?,
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

fn emit(text: &str) -> io::Result<()> {
    let mut output = io::stdout().lock();
    writeln!(output, "{text}")?;
    output.flush()
}

fn print_backend(info: &crate::protocol::InferenceBackend) -> io::Result<()> {
    emit(&format!(
        "Inference backend: {}; lifecycle: {}",
        sanitize(&info.name),
        if info.externally_managed {
            "внешний процесс; load/unload недоступны"
        } else {
            "управляется адаптером"
        }
    ))?;
    if !info.discovery_supported {
        emit("Discovery не поддержан этим адаптером; связь не проверялась.")?;
    } else {
        emit(if info.connected == Some(true) {
            "Соединение: готово"
        } else {
            "Соединение: ошибка"
        })?;
    }
    for model in &info.models {
        emit(&format!("Model ID: {}", sanitize(&model.id)))?;
        for (label, value) in [("architecture", &model.architecture), ("meta", &model.meta)] {
            if let Some(value) = value {
                emit(&format!("{label}: {}", sanitize(&value.to_string())))?;
            }
        }
    }
    if let Some(error) = &info.error {
        emit(&sanitize(error))?;
    }
    Ok(())
}

fn print_models(models: &[crate::protocol::ModelStatus]) -> io::Result<()> {
    for model in models {
        let temp = model
            .temperature
            .map(|t| format!(" · temp={t:.2}"))
            .unwrap_or_default();
        emit(&format!(
            "{} — {}{}",
            sanitize(&model.model),
            if model.loaded {
                "загружена (последнее наблюдение)"
            } else {
                "не загружена"
            },
            temp
        ))?;
    }
    Ok(())
}

fn print_local_models(catalog: &crate::protocol::LocalModels) -> io::Result<()> {
    let field = |value: &str| sanitize(value).replace('\n', "\\u{000a}");
    emit(&format!("Каталог локальных GGUF: {}", field(&catalog.root)))?;
    if catalog.models.is_empty() {
        emit("Подходящих GGUF-файлов нет.")?;
    }
    for model in &catalog.models {
        emit(&format!(
            "{} — {}; {} байт; GGUF v{}; тензоров: {}; architecture: {}; file_type: {}",
            field(&model.id),
            field(&model.path),
            model.size_bytes,
            model.gguf_version,
            model.tensor_count,
            model
                .architecture
                .as_deref()
                .map(field)
                .unwrap_or_else(|| "не указана".into()),
            model
                .file_type
                .map(|value| value.to_string())
                .unwrap_or_else(|| "не указан".into()),
        ))?;
    }
    for rejected in &catalog.rejected {
        emit(&format!(
            "Отклонён: {} — {}",
            field(&rejected.path),
            field(&rejected.error)
        ))?;
    }
    Ok(())
}

fn status_name(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Running => "выполняется",
        TaskStatus::WaitingForHuman => "ожидает человека",
        TaskStatus::Paused => "пауза",
        TaskStatus::Done => "готово",
        TaskStatus::Failed => "ошибка",
        TaskStatus::Cancelled => "отменена",
    }
}

fn print_tasks(tasks: &[Task]) -> io::Result<()> {
    if tasks.is_empty() {
        return emit("Задач пока нет. Введите текст, чтобы начать.");
    }
    for task in tasks {
        emit(&format!(
            "{}  {}  {}",
            task.id,
            status_name(task.status),
            sanitize(&task.category)
        ))?;
        if let Some(recovery) = &task.recovery {
            emit(&format!(
                "  Восстановление: {}; неразрешённых действий: {}. /use {}, затем /recovery",
                sanitize(&recovery.kind),
                recovery.unresolved_tools.len(),
                task.id
            ))?;
        }
    }
    Ok(())
}

fn print_task(task: &Task, from: usize) -> io::Result<()> {
    emit(&format!(
        "\nЗадача {} — {}",
        task.id,
        status_name(task.status)
    ))?;
    for entry in task.context.iter().skip(from) {
        emit(&format!(
            "[{}] {}",
            sanitize(&entry.role),
            sanitize(&entry.content)
        ))?;
    }
    if let Some(snapshot) = &task.pending_tool_approval {
        emit(&format!(
            "Запрос инструмента {}. Для просмотра: /tool",
            snapshot.request_id
        ))?;
    }
    if let Some(recovery) = &task.recovery {
        emit(&format!(
            "Восстановление ({}): {}\nНеразрешённых действий: {}. Точные снимки: /recovery",
            sanitize(&recovery.kind),
            sanitize(&recovery.message),
            recovery.unresolved_tools.len()
        ))?;
    }
    Ok(())
}

fn print_status(status: &BackendStatus) -> io::Result<()> {
    emit(&format!(
        "Задач: {}/{}; бюджет памяти моделей: {}/{} МиБ",
        status.running_tasks,
        status.max_concurrent_tasks,
        status.used_model_memory_mb,
        status.total_model_memory_mb
    ))?;
    for busy in &status.busy_models {
        emit(&format!(
            "{}: генераций {}",
            sanitize(&busy.model),
            busy.generations
        ))?;
    }
    if let Some(operation) = &status.active_operation {
        emit(&format!(
            "{}: {} ({} мс)",
            sanitize(&operation.kind),
            sanitize(&operation.model),
            operation.elapsed_ms
        ))?;
    }
    for warning in &status.recovery_warnings {
        emit(&format!("Восстановление: {}", sanitize(warning)))?;
    }
    Ok(())
}

/// Ограничивает выделение памяти, дочитывая слишком длинную строку без накопления.
fn input_line<R: BufRead>(reader: &mut R) -> io::Result<Option<Result<String, &'static str>>> {
    let mut bytes = Vec::new();
    if Read::by_ref(reader)
        .take((MAX_INPUT_BYTES + 2) as u64)
        .read_until(b'\n', &mut bytes)?
        == 0
    {
        return Ok(None);
    }
    if bytes.len() == MAX_INPUT_BYTES + 2 && !bytes.ends_with(b"\n") {
        loop {
            let buffer = reader.fill_buf()?;
            if buffer.is_empty() {
                break;
            }
            let end = buffer.iter().position(|b| *b == b'\n');
            let consumed = end.map_or(buffer.len(), |i| i + 1);
            reader.consume(consumed);
            if end.is_some() {
                break;
            }
        }
        return Ok(Some(Err("Строка длиннее 32768 байт")));
    }
    if bytes.ends_with(b"\n") {
        bytes.pop();
    }
    if bytes.ends_with(b"\r") {
        bytes.pop();
    }
    if bytes.len() > MAX_INPUT_BYTES || bytes.contains(&0) {
        return Ok(Some(Err("Строка длиннее 32768 байт или содержит NUL")));
    }
    Ok(Some(
        String::from_utf8(bytes).map_err(|_| "Нужен корректный UTF-8"),
    ))
}

fn pages(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for line in text.split('\n') {
        let characters: Vec<char> = line.chars().collect();
        if characters.is_empty() {
            lines.push(String::new());
        }
        for part in characters.chunks(40) {
            lines.push(part.iter().collect());
        }
    }
    lines.chunks(12).map(|part| part.join("\n")).collect()
}

struct Session {
    client: ApiClient,
    app: App,
    interactive: bool,
    pages: Vec<String>,
    page: usize,
    printed: Option<(Uuid, String, usize)>,
    recovery_displayed: Option<(Uuid, TaskRecovery)>,
}

impl Session {
    fn new(client: ApiClient) -> Self {
        let mut app = App::default();
        app.new_session(); // Новая строка не должна неожиданно продолжить старую задачу.
        Self {
            client,
            app,
            interactive: io::stdin().is_terminal() && io::stdout().is_terminal(),
            pages: vec![],
            page: 0,
            printed: None,
            recovery_displayed: None,
        }
    }

    async fn refresh(&mut self) -> Result<(), ApiError> {
        let tasks = self.client.list_tasks().await?;
        let status = self.client.status().await?;
        let models = self.client.list_models().await?;
        self.app.receive_snapshot(tasks, status, models);
        Ok(())
    }

    fn failed(&mut self, error: ApiError) -> io::Result<()> {
        self.app.fail(error.to_string());
        emit(&format!("{error} Для восстановления связи: /refresh"))
    }

    async fn poll(&mut self, full: bool) -> io::Result<()> {
        let Some(id) = self.app.selected_task_id else {
            return Ok(());
        };
        match self.client.get_task(id).await {
            Ok(task) => {
                let previous = self.printed.as_ref();
                let unchanged = previous.is_some_and(|(old_id, at, count)| {
                    *old_id == id && *at == task.updated_at && *count == task.context.len()
                });
                if full || !unchanged {
                    let from = if full {
                        0
                    } else {
                        previous
                            .filter(|(old_id, _, count)| {
                                *old_id == id && *count <= task.context.len()
                            })
                            .map_or(0, |(_, _, count)| *count)
                    };
                    print_task(&task, from)?;
                    self.printed = Some((id, task.updated_at.clone(), task.context.len()));
                }
                self.app.receive_task(task);
            }
            Err(error) => self.failed(error)?,
        }
        Ok(())
    }

    async fn execute(&mut self, command: Command) -> io::Result<()> {
        let result = match &command {
            Command::Create(text) => self.client.create_task(text).await.map(Some),
            Command::Continue { id, text } => {
                self.client.continue_task(*id, text).await.map(|_| None)
            }
            Command::Pause(id) => self.client.pause_task(*id).await.map(|_| None),
            Command::Cancel(id) => self.client.cancel_task(*id).await.map(|_| None),
            Command::Decide { snapshot, approve } => {
                self.client.decide(snapshot, *approve).await.map(|_| None)
            }
            Command::Refresh => self.refresh().await.map(|_| None),
        };
        match result {
            Ok(task) => {
                self.app.success(task);
                emit("Команда принята backend.")?;
                self.poll(false).await?;
            }
            Err(error) => self.failed(error)?,
        }
        Ok(())
    }

    fn show_page(&mut self) -> io::Result<()> {
        let Some(dialog) = &self.app.approval else {
            return emit("Сначала откройте /tool");
        };
        if dialog.stale {
            return emit("Запрос устарел. /close, затем свежий /tool.");
        }
        let Some(page) = self.pages.get(self.page) else {
            return emit("Весь снимок уже выведен.");
        };
        emit(&format!(
            "Запрос {} — страница {}/{}\n{}",
            dialog.snapshot.request_id,
            self.page + 1,
            self.pages.len(),
            page
        ))?;
        self.page += 1;
        if self.page == self.pages.len() {
            self.app.mark_approval_reviewed();
            emit("Снимок выведен полностью. /approve UUID или /deny UUID; затем /confirm UUID.")
        } else {
            emit("Следующая страница: /more. Переносы — только отображение; управляющие символы раскрыты.")
        }
    }

    fn decide(&mut self, verb: &str, id: &str) -> io::Result<Option<Command>> {
        if !self.interactive {
            emit("Решения инструментов запрещены без интерактивных stdin и stdout.")?;
            return Ok(None);
        }
        let matches = Uuid::parse_str(id).ok().is_some_and(|id| {
            self.app
                .approval
                .as_ref()
                .is_some_and(|d| d.snapshot.request_id == id)
        });
        if !matches {
            emit("UUID не совпадает с открытым запросом; решение не отправлено.")?;
            return Ok(None);
        }
        if verb == "/confirm" {
            let command = self.app.confirm_approval();
            if command.is_none() {
                emit("Нет подготовленного актуального решения; сначала проверьте /tool.")?;
            }
            return Ok(command);
        }
        if self.app.prepare_approval(verb == "/approve") {
            let dialog = self
                .app
                .approval
                .as_ref()
                .expect("подготовлен существующий диалог");
            emit(&format!(
                "{}: задача {}, инструмент {}. Отправить: /confirm {}",
                if verb == "/approve" {
                    "Разрешение"
                } else {
                    "Отказ"
                },
                dialog.snapshot.task_id,
                sanitize(&dialog.snapshot.tool_name),
                dialog.snapshot.request_id
            ))?;
        } else {
            emit("Решение не подготовлено: запрос устарел или просмотрены не все страницы.")?;
        }
        Ok(None)
    }

    async fn line(&mut self, line: String) -> io::Result<bool> {
        if !line.starts_with('/') {
            if line.trim().is_empty() {
                return Ok(true);
            }
            self.app.input = line;
            if let Some(command) = self.app.submit() {
                self.execute(command).await?;
            } else {
                emit(self.app.error.as_deref().unwrap_or(
                    "Команда недоступна; проверьте связь и состояние задачи, либо /new.",
                ))?;
            }
            return Ok(true);
        }
        let (verb, argument) = line.split_once(' ').unwrap_or((&line, ""));
        let argument = argument.trim();
        match verb {
            "/backend" => match self.client.inference_backend().await {
                Ok(info) => print_backend(&info)?,
                Err(error) => self.failed(error)?,
            },
            "/ack" => {
                if !self.interactive {
                    emit("Ознакомление с исходом подтверждается только с интерактивными stdin и stdout.")?;
                } else if let Some((task_id, recovery, request_id)) =
                    Uuid::parse_str(argument).ok().and_then(|id| {
                        let (task_id, recovery) = self.recovery_displayed.as_ref()?;
                        (Some(*task_id) == self.app.selected_task_id
                            && (recovery.acknowledgement_id == Some(id)
                                || recovery.unresolved_tools.iter().any(|snapshot| {
                                    snapshot.request_id == id && snapshot.task_id == *task_id
                                })))
                        .then(|| (*task_id, recovery.clone(), id))
                    })
                {
                    // После попытки, включая неоднозначную сетевую ошибку, нужен новый просмотр.
                    self.recovery_displayed = None;
                    let result = if recovery.acknowledgement_id == Some(request_id) {
                        self.client
                            .acknowledge_legacy_recovery(task_id, &recovery)
                            .await
                    } else {
                        let snapshot = recovery
                            .unresolved_tools
                            .iter()
                            .find(|snapshot| snapshot.request_id == request_id)
                            .expect("проверенный снимок");
                        self.client.acknowledge_recovery(snapshot).await
                    };
                    match result {
                        Ok(()) => {
                            emit("Ознакомление сохранено. Исходный запрос не повторяется; для новой попытки нужен новый запрос.")?;
                            self.poll(true).await?;
                        }
                        Err(error) => self.failed(error)?,
                    }
                } else {
                    emit("UUID не совпадает с просмотренным исходом выбранной задачи. Сначала /recovery.")?;
                }
            }
            "/approve" | "/deny" | "/confirm" => {
                if let Some(command) = self.decide(verb, argument)? {
                    self.execute(command).await?;
                }
            }
            "/use" => {
                if let Ok(id) = Uuid::parse_str(argument) {
                    match self.client.get_task(id).await {
                        Ok(task) => {
                            self.recovery_displayed = None;
                            self.app.receive_task(task);
                            self.app.select_task(id);
                            self.printed = None;
                            self.poll(true).await?;
                        }
                        Err(error) => self.failed(error)?,
                    }
                } else {
                    emit("Нужен UUID задачи: /use UUID")?;
                }
            }
            _ if !matches!(
                verb,
                "/use" | "/approve" | "/deny" | "/confirm" | "/ack" | "/temp"
            ) && !argument.is_empty() =>
            {
                emit("Лишние аргументы или неизвестная команда; /help")?
            }
            "/quit" => return Ok(false),
            "/help" => emit(HELP)?,
            "/new" => {
                self.recovery_displayed = None;
                self.app.new_session();
                self.printed = None;
                emit("Новая беседа. Введите задачу.")?;
            }
            "/close" => {
                self.app.close_approval();
                self.pages.clear();
                self.page = 0;
            }
            "/more" => self.show_page()?,
            "/history" => self.poll(true).await?,
            "/recovery" => {
                self.recovery_displayed = None;
                if let Some(id) = self.app.selected_task_id {
                    match self.client.get_task(id).await {
                        Ok(task) => {
                            if let Some(recovery) = &task.recovery {
                                emit(&format!(
                                    "Восстановление ({}): {}",
                                    sanitize(&recovery.kind),
                                    sanitize(&recovery.message)
                                ))?;
                                for snapshot in &recovery.unresolved_tools {
                                    emit(&approval_text(snapshot))?;
                                }
                                if let Some(receipt) = recovery.acknowledgement_id {
                                    emit(&format!("Квитанция восстановления задачи {}: {}. Старый вызов не имеет полного долговечного снимка инструмента; проверьте историю и внешний результат.", task.id, receipt))?;
                                }
                                if recovery.kind == "unknown_outcome" {
                                    self.recovery_displayed = Some((task.id, recovery.clone()));
                                    emit("Проверьте внешний результат самостоятельно. /ack UUID фиксирует ознакомление и не повторяет инструмент.")?;
                                }
                            } else {
                                emit("Нет проблем восстановления у выбранной задачи.")?;
                            }
                            self.app.receive_task(task);
                        }
                        Err(error) => self.failed(error)?,
                    }
                } else {
                    emit("Сначала выберите задачу: /use UUID")?;
                }
            }
            "/tasks" | "/refresh" => match self.refresh().await {
                Ok(()) => print_tasks(&self.app.tasks)?,
                Err(error) => self.failed(error)?,
            },
            "/status" => match self.client.status().await {
                Ok(status) => print_status(&status)?,
                Err(error) => self.failed(error)?,
            },
            "/models" => match self.client.list_models().await {
                Ok(models) => print_models(&models)?,
                Err(error) => self.failed(error)?,
            },
            "/temp" => {
                let mut parts = argument.split_whitespace();
                match (parts.next(), parts.next(), parts.next()) {
                    (Some(model), Some(raw_temp), None) => {
                        let parsed = if matches!(raw_temp, "default" | "null" | "-") {
                            Ok(None)
                        } else {
                            raw_temp.parse::<f32>().map(Some)
                        };
                        match parsed {
                            Ok(temp) => {
                                match self.client.set_model_temperature(model, temp).await {
                                    Ok(models) => print_models(&models)?,
                                    Err(error) => self.failed(error)?,
                                }
                            }
                            Err(_) => emit("Использование: /temp МОДЕЛЬ <0.0..2.0|default>")?,
                        }
                    }
                    _ => emit("Использование: /temp МОДЕЛЬ <0.0..2.0|default>")?,
                }
            }
            "/local-models" => match self.client.local_models().await {
                Ok(catalog) => print_local_models(&catalog)?,
                Err(error) => self.failed(error)?,
            },
            "/pause" | "/cancel" => {
                let command = if verb == "/pause" {
                    self.app.pause()
                } else {
                    self.app.cancel()
                };
                if let Some(command) = command {
                    self.execute(command).await?;
                } else {
                    emit("Нет выбранной задачи в подходящем состоянии.")?;
                }
            }
            "/tool" => {
                self.poll(false).await?;
                if self.app.open_approval() {
                    let text =
                        approval_text(&self.app.approval.as_ref().expect("диалог открыт").snapshot);
                    if text.len() > 65_536 {
                        self.app.close_approval();
                        emit("Снимок больше 64 КиБ: этот CLI не может разрешить его исполнение.")?;
                    } else {
                        self.pages = pages(&text);
                        self.page = 0;
                        self.show_page()?;
                    }
                } else {
                    emit(
                        "Нет нового запроса; ранее открытый снимок сначала закройте через /close.",
                    )?;
                }
            }
            _ => emit("Неизвестная команда; /help")?,
        }
        Ok(true)
    }

    async fn run(mut self) -> io::Result<()> {
        emit(
            "KAIC CLI. /help — команды. Введите задачу; /tasks и /use UUID — существующие беседы.",
        )?;
        if let Err(error) = self.refresh().await {
            self.failed(error)?;
        }
        let (sender, mut input) = mpsc::channel(8);
        std::thread::spawn(move || {
            let stdin = io::stdin();
            let mut reader = stdin.lock();
            loop {
                match input_line(&mut reader) {
                    Ok(Some(line)) => {
                        if sender.blocking_send(line).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        let _ = sender.blocking_send(Err("Не удалось прочитать stdin"));
                        break;
                    }
                }
            }
        });
        let mut poll = tokio::time::interval(Duration::from_secs(2));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                line = input.recv() => match line {
                    Some(Ok(line)) => if !self.line(line).await? { break; },
                    Some(Err(error)) => emit(error)?,
                    None => break,
                },
                _ = poll.tick(), if self.app.connected && self.app.selected_task_id.is_some() => self.poll(false).await?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_input_recovers_after_long_line_and_validates_utf8() {
        let mut input = vec![b'x'; MAX_INPUT_BYTES + 100];
        input.extend_from_slice(b"\nnext\n\xff\n");
        let mut reader = io::Cursor::new(input);
        assert!(input_line(&mut reader).unwrap().unwrap().is_err());
        assert_eq!(input_line(&mut reader).unwrap().unwrap().unwrap(), "next");
        assert!(input_line(&mut reader).unwrap().unwrap().is_err());
        assert!(input_line(&mut reader).unwrap().is_none());
    }

    #[test]
    fn pagination_preserves_all_unicode_characters() {
        let text = "Аргументы:🧱\\u{001b}".repeat(200);
        let pages = pages(&text);
        assert!(pages.len() > 1);
        assert_eq!(pages.join("").replace('\n', ""), text);
    }
}
