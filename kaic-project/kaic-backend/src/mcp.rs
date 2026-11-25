//! Клиент MCP-серверов поверх официального SDK `rmcp`, транспорт stdio.
//!
//! Что этот слой делает: поднимает MCP-сервер подпроцессом, проводит
//! handshake, спрашивает список инструментов и вызывает их. Чего он НЕ
//! делает: не решает, какой инструмент вызвать (это дело модели) и не знает
//! ни про Scheduler, ни про Task Store. Наружу отдаются нейтральные
//! `ToolSpec` из `model_backend`, а типы `rmcp` за границу модуля не уходят —
//! ровно по той же причине, по которой формы LM Studio не уходят за границу
//! `model_backend`: смена SDK не должна задевать ничего выше.
//!
//! Транспорт только stdio. Streamable HTTP и SSE в SDK есть, но не включены
//! (см. features в Cargo.toml): пока сервер локальный и запускается нами,
//! сетевой транспорт добавляет поверхность отказа, ничего не давая.

use anyhow::{Context, Result};
use rmcp::{
    model::{CallToolRequestParams, PaginatedRequestParams},
    service::{RoleClient, RunningService, RunningServiceCancellationToken, ServiceError},
    transport::{ConfigureCommandExt, TokioChildProcess},
    ServiceExt,
};
use std::{
    collections::HashSet,
    fmt,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

#[cfg(test)]
use crate::model_backend::ToolCall;
use crate::model_backend::ToolSpec;

/// Переменные, которыми глушится телеметрия сервера.
///
/// Это НЕ перестраховка. `blender-mcp` 1.8.0 по умолчанию отправляет на
/// сторонний Supabase текст промпта и скриншоты вьюпорта. Имён три, потому
/// что сервер проверяет три разных — какое именно сработает, зависит от
/// версии, и полагаться на одно значило бы полагаться на угадывание.
///
/// Сервер без них не запускается вовсе: см. `spawn`, где отсутствие
/// глушения — ошибка, а не предупреждение.
const TELEMETRY_KILL_SWITCHES: &[&str] = &[
    "DISABLE_TELEMETRY",
    "BLENDER_MCP_DISABLE_TELEMETRY",
    "MCP_DISABLE_TELEMETRY",
];

/// Все сроки — абсолютные: progress-уведомления не продлевают вызов.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpDeadlines {
    pub handshake_ms: u64,
    pub list_ms: u64,
    pub call_ms: u64,
    pub shutdown_ms: u64,
    pub max_list_pages: usize,
}

impl Default for McpDeadlines {
    fn default() -> Self {
        Self {
            handshake_ms: 10_000,
            list_ms: 10_000,
            call_ms: 120_000,
            shutdown_ms: 5_000,
            max_list_pages: 128,
        }
    }
}

impl McpDeadlines {
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("handshake_ms", self.handshake_ms),
            ("list_ms", self.list_ms),
            ("call_ms", self.call_ms),
            ("shutdown_ms", self.shutdown_ms),
        ] {
            anyhow::ensure!(
                (1..=3_600_000).contains(&value),
                "MCP deadlines.{name} должен быть в пределах 1..=3600000 мс"
            );
        }
        anyhow::ensure!(
            (1..=1024).contains(&self.max_list_pages),
            "MCP deadlines.max_list_pages должен быть в пределах 1..=1024"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpFailureKind {
    Timeout,
    Cancelled,
    Protocol,
    Transport,
}

/// Слой исполнения может отличить отказ от обычного результата is_error.
#[derive(Debug)]
pub struct McpExecutionError {
    pub kind: McpFailureKind,
    pub operation: &'static str,
    detail: String,
}

impl McpExecutionError {
    pub fn outcome_may_be_unknown(&self) -> bool {
        // Даже RPC error не доказывает отсутствия побочного эффекта.
        self.operation == "tools/call"
    }

    fn timeout(operation: &'static str, timeout_ms: u64) -> Self {
        Self {
            kind: McpFailureKind::Timeout,
            operation,
            detail: format!("превышен срок {timeout_ms} мс"),
        }
    }

    fn from_service(operation: &'static str, error: ServiceError) -> Self {
        let kind = match &error {
            ServiceError::Timeout { .. } => McpFailureKind::Timeout,
            ServiceError::Cancelled { .. } => McpFailureKind::Cancelled,
            ServiceError::McpError(_) | ServiceError::UnexpectedResponse => {
                McpFailureKind::Protocol
            }
            _ => McpFailureKind::Transport,
        };
        Self {
            kind,
            operation,
            detail: error.to_string(),
        }
    }
}

impl fmt::Display for McpExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "MCP {} ({:?}): {}",
            self.operation, self.kind, self.detail
        )
    }
}

impl std::error::Error for McpExecutionError {}

/// Drop вызванного future закрывает всю сессию, включая транспорт/подпроцесс.
/// Отмена не означает, что сервер успел предотвратить внешний эффект.
struct RequestGuard<'a> {
    token: Option<RunningServiceCancellationToken>,
    unusable: &'a AtomicBool,
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            // Сначала закрываем локальный admission, затем просим SDK
            // завершить транспорт: его sender может ещё оставаться живым.
            self.unusable.store(true, Ordering::Release);
            token.cancel();
        }
    }
}

/// Как запускать MCP-сервер.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct McpServerConfig {
    /// Исполняемый файл сервера.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,

    /// Имена инструментов, которые разрешено вызывать БЕЗ подтверждения.
    ///
    /// По умолчанию пуст, и это умолчание принципиальное: система, которая
    /// по умолчанию исполняет, однажды исполнит не то. Список составляет
    /// человек и только человек — ни аннотации сервера, ни вид имени
    /// основанием не являются (см. `tool_gate`).
    #[serde(default)]
    pub auto_approve: Vec<String>,
    #[serde(default)]
    pub deadlines: McpDeadlines,
}

impl McpServerConfig {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            auto_approve: Vec::new(),
            deadlines: McpDeadlines::default(),
        }
    }

    /// Читает конфиг сервера, если он есть.
    ///
    /// `None` при отсутствии файла — это не ошибка, а рабочее состояние:
    /// KAIC обязан работать без инструментов ровно так, как работал до них.
    /// Тот же приём, что у `TelegramConfig::load_or_default`.
    ///
    /// А вот битый файл — ошибка: он означает, что инструменты настроить
    /// ХОТЕЛИ, и тихо работать без них значило бы соврать.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Option<Self>> {
        let path = path.as_ref();
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err).with_context(|| format!("не удалось прочитать {}", path.display()))
            }
        };
        let config: Self = serde_yaml::from_str(&raw)
            .with_context(|| format!("не удалось разобрать {}", path.display()))?;
        config.deadlines.validate()?;
        Ok(Some(config))
    }
}

/// Живое подключение к одному MCP-серверу.
pub struct McpClient {
    service: RunningService<RoleClient, ()>,
    /// Имя сервера из его же `serverInfo` — для сообщений об ошибках,
    /// чтобы «инструмент не найден» называл, у кого именно его нет.
    server_name: String,
    identity: crate::task_store::ToolServerIdentity,
    deadlines: McpDeadlines,
    unusable: AtomicBool,
}

impl McpClient {
    /// Поднимает сервер подпроцессом и проводит handshake
    /// (`initialize` → `notifications/initialized`).
    ///
    /// Телеметрия глушится ДО запуска: переменные ставятся на команду, а не
    /// на процесс KAIC — глобальный `set_var` задел бы и всё остальное, что
    /// мы запускаем, и был бы гонкой в многопоточной программе.
    pub async fn connect(config: &McpServerConfig) -> Result<Self> {
        config.deadlines.validate()?;
        let command = tokio::process::Command::new(&config.command).configure(|cmd| {
            cmd.args(&config.args);
            cmd.kill_on_drop(true);
            for key in TELEMETRY_KILL_SWITCHES {
                cmd.env(key, "1");
            }
        });

        let transport = TokioChildProcess::new(command)
            .with_context(|| format!("не удалось запустить MCP-сервер '{}'", config.command))?;
        let service = tokio::time::timeout(
            Duration::from_millis(config.deadlines.handshake_ms),
            ().serve(transport),
        )
        .await
        .map_err(|_| McpExecutionError::timeout("initialize", config.deadlines.handshake_ms))?
        .with_context(|| format!("handshake с MCP-сервером '{}' не удался", config.command))?;

        let server_name = service
            .peer_info()
            .and_then(|info| info.server_info.as_ref().map(|i| i.name.to_string()))
            .unwrap_or_else(|| config.command.clone());

        Ok(Self {
            identity: crate::task_store::ToolServerIdentity {
                command: config.command.clone(),
                args: config.args.clone(),
                peer_name: server_name.clone(),
            },
            service,
            server_name,
            deadlines: config.deadlines.clone(),
            unusable: AtomicBool::new(false),
        })
    }

    /// Имя сервера, как он сам себя назвал в `serverInfo`.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    pub fn identity(&self) -> &crate::task_store::ToolServerIdentity {
        &self.identity
    }

    fn request_guard(&self, operation: &'static str) -> Result<RequestGuard<'_>> {
        if self.unusable.load(Ordering::Acquire) || self.service.is_closed() {
            return Err(McpExecutionError {
                kind: McpFailureKind::Transport,
                operation,
                detail: "сессия закрыта после отмены или отказа; повтор запрещён".into(),
            }
            .into());
        }
        Ok(RequestGuard {
            token: Some(self.service.cancellation_token()),
            unusable: &self.unusable,
        })
    }

    /// Полный список инструментов сервера.
    ///
    /// Страницы вычитываются до конца (`nextCursor`), а не только первая:
    /// разведка видела `nextCursor: None` на blender-mcp, но это свойство
    /// того сервера, а не протокола — следующий может отдать список частями,
    /// и модель тогда просто не узнала бы о половине инструментов.
    pub async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        let mut guard = self.request_guard("tools/list")?;
        let result = tokio::time::timeout(
            Duration::from_millis(self.deadlines.list_ms),
            self.list_tools_inner(),
        )
        .await
        .map_err(|_| McpExecutionError::timeout("tools/list", self.deadlines.list_ms))??;
        guard.token.take();
        Ok(result)
    }

    async fn list_tools_inner(&self) -> Result<Vec<ToolSpec>> {
        let mut specs = Vec::new();
        let mut cursor = None;
        let mut seen = HashSet::new();
        for _ in 0..self.deadlines.max_list_pages {
            // Структуры протокола в SDK помечены `#[non_exhaustive]`, поэтому
            // собираются от `default()`, а не литералом: так добавление поля
            // в новой версии SDK не сломает сборку.
            let mut params = PaginatedRequestParams::default();
            params.cursor = cursor;
            let page = self
                .service
                .list_tools(Some(params))
                .await
                .map_err(|error| McpExecutionError::from_service("tools/list", error))?;

            for tool in page.tools {
                // Переносятся ровно три поля. `tool.annotations` НЕ читается
                // намеренно: спецификация MCP называет подсказки сервера
                // (`readOnlyHint`, `destructiveHint` и прочие) недоверенными.
                // Сервер, пометивший свой инструмент безопасным, не является
                // основанием его разрешить — и чтобы это правило нельзя было
                // случайно нарушить выше, поле не доходит до `ToolSpec`
                // вообще.
                specs.push(ToolSpec {
                    name: tool.name.to_string(),
                    description: tool.description.map(|d| d.to_string()).unwrap_or_default(),
                    input_schema: serde_json::Value::Object((*tool.input_schema).clone()),
                });
            }

            match page.next_cursor {
                Some(next) => {
                    anyhow::ensure!(seen.insert(next.clone()), "MCP tools/list повторил cursor");
                    cursor = Some(next);
                }
                None => return Ok(specs),
            }
        }
        anyhow::bail!(
            "MCP tools/list превысил лимит страниц {}",
            self.deadlines.max_list_pages
        )
    }

    /// Вызывает инструмент и возвращает его результат текстом.
    ///
    /// Текстом, а не структурой, потому что результат идёт обратно модели
    /// сообщением роли `tool`, а туда всё равно уходит строка. Не-текстовые
    /// части (изображения, ресурсы) сворачиваются в пометку: молча их
    /// выбросить значило бы соврать модели о том, что вернул сервер.
    pub async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<String> {
        let arguments = match arguments {
            serde_json::Value::Object(map) => Some(map),
            serde_json::Value::Null => None,
            other => {
                anyhow::bail!("аргументы инструмента '{name}' не объект JSON, а {other}")
            }
        };

        let mut params = CallToolRequestParams::default();
        params.name = name.to_string().into();
        params.arguments = arguments;

        let mut guard = self.request_guard("tools/call")?;
        let result = tokio::time::timeout(
            Duration::from_millis(self.deadlines.call_ms),
            self.service.call_tool(params),
        )
        .await
        .map_err(|_| McpExecutionError::timeout("tools/call", self.deadlines.call_ms))?
        .map_err(|error| McpExecutionError::from_service("tools/call", error))?;
        guard.token.take();

        let mut parts: Vec<String> = Vec::new();
        for content in result.content.iter() {
            match content.as_text() {
                Some(text) => parts.push(text.text.clone()),
                None => parts.push("[не-текстовая часть ответа инструмента опущена]".to_string()),
            }
        }
        if parts.is_empty() {
            parts.push("[инструмент вернул пустой результат]".to_string());
        }

        let joined = parts.join("\n");

        // `is_error` — штатный способ сервера сказать «инструмент отработал,
        // но неудачно». Это НЕ ошибка транспорта: модель должна увидеть текст
        // и решить, что делать дальше, поэтому он возвращается как результат,
        // а не как `Err`. Пометка обязательна — без неё модель приняла бы
        // сообщение об ошибке за успешный ответ.
        if result.is_error.unwrap_or(false) {
            return Ok(format!("ОШИБКА ИНСТРУМЕНТА: {joined}"));
        }
        Ok(joined)
    }

    /// Корректно завершает сервер: `cancel` закрывает сессию и дожидается
    /// выхода подпроцесса. Без этого подпроцесс пережил бы KAIC.
    pub async fn shutdown(mut self) -> Result<()> {
        let closed = self
            .service
            .close_with_timeout(Duration::from_millis(self.deadlines.shutdown_ms))
            .await
            .context("не удалось корректно завершить MCP-сервер")?;
        if closed.is_none() {
            return Err(McpExecutionError::timeout("shutdown", self.deadlines.shutdown_ms).into());
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "mcp_deadline_tests.rs"]
mod deadline_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Живой прогон против настоящего blender-mcp.
    ///
    /// `#[ignore]` не потому, что тест необязательный, а потому, что он
    /// требует установленного сервера: в обычном `cargo test` он бы падал на
    /// машине, где его нет, и через неделю его бы отключили насовсем.
    /// Запускается явно:
    ///
    /// ```text
    /// cargo test -- --ignored live_blender_mcp_lists_its_tools --nocapture
    /// ```
    ///
    /// Путь к серверу берётся из `KAIC_MCP_COMMAND`, умолчание — установка
    /// через `uv tool install`, то есть ВНЕ репозитория.
    #[tokio::test]
    #[ignore = "требует установленного blender-mcp; запускать явно"]
    async fn live_blender_mcp_lists_its_tools() {
        let command = std::env::var("KAIC_MCP_COMMAND").unwrap_or_else(|_| {
            let home = std::env::var("USERPROFILE").expect("USERPROFILE");
            format!("{home}\\.local\\bin\\blender-mcp.exe")
        });

        let started = std::time::Instant::now();
        let client = McpClient::connect(&McpServerConfig::new(command))
            .await
            .expect("сервер поднялся и handshake прошёл");
        eprintln!(
            "handshake с '{}' за {} мс",
            client.server_name(),
            started.elapsed().as_millis()
        );

        let tools = client.list_tools().await.expect("tools/list");
        eprintln!("инструментов: {}", tools.len());
        for tool in &tools {
            eprintln!(
                "  {} — {}",
                tool.name,
                tool.description.lines().next().unwrap_or("")
            );
        }

        assert!(
            !tools.is_empty(),
            "сервер обязан отдать хотя бы один инструмент"
        );
        // Схема нужна модели: инструмент без неё она вызвать не сможет.
        for tool in &tools {
            assert!(
                tool.input_schema.is_object(),
                "у '{}' нет объектной inputSchema",
                tool.name
            );
        }

        client
            .shutdown()
            .await
            .expect("подпроцесс завершился корректно");
    }

    #[test]
    fn server_annotations_never_reach_the_decision() {
        // Спецификация MCP называет подсказки сервера недоверенными: сервер,
        // пометивший инструмент безопасным, не является основанием его
        // разрешить. Гарантия держится не дисциплиной, а формой типа —
        // `ToolSpec` физически некуда положить аннотацию, поэтому ни один
        // слой выше не может на неё опереться даже случайно.
        let spec = ToolSpec {
            name: "execute_blender_code".to_string(),
            description: "readOnlyHint: true, destructiveHint: false".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        // Что `ToolSpec` не несёт аннотаций, обеспечивает не этот тест, а
        // компилятор: поля ровно три, и добавить обращение к четвёртому
        // нельзя — его нет. Деструктуризация ниже сломается, если поле
        // когда-нибудь появится, и тогда решение придётся принимать заново,
        // а не унаследовать молча.
        let ToolSpec {
            name,
            description: _,
            input_schema: _,
        } = spec;

        // Сервер кричит о своей безопасности в описании — решение это не
        // задевает: разрешает только явный список.
        assert!(matches!(
            crate::tool_gate::decide(
                &[],
                ToolCall {
                    id: "c1".to_string(),
                    name: name.clone(),
                    arguments: "{}".to_string(),
                }
            ),
            crate::tool_gate::Decision::NeedsHuman(_)
        ));
        // И даже присутствие ДРУГОГО имени в списке его не разрешает.
        assert!(matches!(
            crate::tool_gate::decide(
                &["get_scene_info".to_string()],
                ToolCall {
                    id: "c2".to_string(),
                    name,
                    arguments: "{}".to_string()
                }
            ),
            crate::tool_gate::Decision::NeedsHuman(_)
        ));
    }

    #[test]
    fn the_allowlist_defaults_to_empty_when_the_config_omits_it() {
        let dir = std::env::temp_dir().join(format!("kaic-mcpcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.yaml");
        std::fs::write(&path, "command: some-server\n").unwrap();

        let config = McpServerConfig::load(&path)
            .unwrap()
            .expect("конфиг прочитан");
        assert!(
            config.auto_approve.is_empty(),
            "умолчание обязано быть пустым: иначе система однажды исполнит не то"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn every_known_telemetry_switch_is_covered() {
        // Сервер 1.8.0 читает три разных имени. Пропустить одно значит
        // отправить текст задачи пользователя на чужой Supabase.
        assert!(TELEMETRY_KILL_SWITCHES.contains(&"DISABLE_TELEMETRY"));
        assert!(TELEMETRY_KILL_SWITCHES.contains(&"BLENDER_MCP_DISABLE_TELEMETRY"));
        assert!(TELEMETRY_KILL_SWITCHES.contains(&"MCP_DISABLE_TELEMETRY"));
    }
}
