//! Model Backend — единая точка входа для работы с LLM.
//!
//! Ни Router, ни Capability Registry, ни Scheduler не вызывают модели
//! напрямую — все обращения идут через trait `ModelBackend`. Сегодня
//! единственная реализация — LM Studio (через его REST API). Завтра
//! это может быть llama.cpp server, ONNX Runtime или Candle — ничего
//! выше этого слоя не заметит разницы.
//!
//! Trait умышленно минимален: Scheduler'у нужно знать, загружена ли
//! модель и уметь её загрузить/выгрузить; Agent'у нужно получить ответ
//! модели на накопленную историю сообщений задачи.

use std::collections::HashMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Роль сообщения в истории диалога.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    /// Результат вызова инструмента, возвращаемый модели.
    Tool,
}

impl Role {
    fn as_wire_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// Одно сообщение в истории диалога с моделью.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Заполняется ТОЛЬКО у роли `Tool` — идентификатор вызова, на который
    /// это сообщение отвечает. У всех остальных ролей `None`, и тогда поле
    /// не уходит провайдеру вовсе.
    pub tool_call_id: Option<String>,
    /// Заполняется ТОЛЬКО у роли `Assistant`, когда модель попросила вызвать
    /// инструменты.
    ///
    /// Без этого сообщения история невалидна: схема требует, чтобы перед
    /// результатами роли `tool` стояла реплика ассистента, эти вызовы
    /// запросившая. Вернуть результат, не вернув запрос, — всё равно что
    /// отдать ответ на вопрос, которого в переписке нет.
    pub tool_calls: Vec<ToolCall>,
}

impl Message {
    /// Обычное сообщение диалога — без привязки к вызову инструмента.
    /// Существует, чтобы девятнадцать прежних мест не обрастали
    /// `tool_call_id: None`, который к ним не относится.
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    /// Реплика ассистента, запросившая вызов инструментов.
    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_calls: calls,
        }
    }

    /// Результат инструмента, возвращаемый модели.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: Vec::new(),
        }
    }
}

/// Запрос на генерацию ответа модели.
///
/// Содержит полную историю сообщений, а не одну реплику — это позволяет
/// Agent'у передавать накопленный контекст задачи (см. Task Store) без
/// изменения этого интерфейса, когда HITL-цикл продолжает работу после
/// паузы на решение пользователя.

/// Описание одного инструмента, как оно уходит провайдеру.
///
/// Нейтрально к источнику: сюда сводятся и инструменты MCP-сервера, и любые
/// будущие встроенные. Типы `rmcp` за границу `mcp.rs` не выходят — смена SDK
/// не должна задевать ни этот слой, ни всё, что выше.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema аргументов — отдаётся провайдеру как есть.
    pub input_schema: serde_json::Value,
}

/// Вызов инструмента, который запросила модель.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCall {
    /// Идентификатор провайдера. Возвращается обратно в сообщении роли
    /// `tool` — по нему модель сопоставляет результат с запросом, и своим
    /// его подменять нельзя.
    pub id: String,
    pub name: String,
    /// Аргументы как их прислала модель. Строка, а не разобранный JSON:
    /// провайдер отдаёт именно строку, и разбор — отдельный шаг, который
    /// имеет право провалиться, не роняя весь ответ.
    pub arguments: String,
}

pub struct GenerateRequest {
    pub messages: Vec<Message>,

    /// Инструменты, доступные модели на этот запрос.
    ///
    /// Пустой список — поле НЕ отправляется провайдеру вообще, и поведение
    /// остаётся ровно прежним, добавления инструментов не было. Это то же
    /// свойство, что у `temperature: None`, и по той же причине: старые пути
    /// не должны заметить появления новых.
    pub tools: Vec<ToolSpec>,

    /// Температура сэмплинга. `None` — поле НЕ отправляется провайдеру
    /// вообще, и действует его умолчание. Это важное свойство, а не
    /// деталь: до появления этого поля backend никогда не слал
    /// temperature, и `None` обязан сохранять ровно прежнее поведение.
    pub temperature: Option<f32>,
}

/// Ответ модели: текст ЛИБО просьба вызвать инструменты.
///
/// Сумма, а не структура с двумя опциональными полями. Причина не
/// стилистическая: провайдер возвращает ровно одно из двух (`finish_reason`
/// это и означает), и структура с двумя `Option` допускала бы четыре
/// состояния, из которых два невозможны. Разбирать невозможные состояния
/// пришлось бы в каждой из точек потребления.
pub enum GenerateResponse {
    /// Модель ответила текстом — обычное завершение.
    Text(String),
    /// Модель просит вызвать инструменты и вернуть ей результат.
    /// Список, а не один вызов: провайдер вправе запросить несколько сразу.
    ToolCalls(Vec<ToolCall>),
}

impl GenerateResponse {
    /// Текст ответа, если модель ответила текстом.
    ///
    /// Нужен точкам, которым инструменты не положены вовсе (служебное
    /// извлечение поискового термина) — им незачем расписывать `match` ради
    /// ветки, которой у них не бывает.
    pub fn text(&self) -> Option<&str> {
        match self {
            GenerateResponse::Text(content) => Some(content),
            GenerateResponse::ToolCalls(_) => None,
        }
    }
}

/// Единый интерфейс для работы с любым источником LLM.
///
/// Реализация не обязана быть быстрой или синхронной — все методы
/// асинхронные, потому что загрузка модели и генерация ответа могут
/// занимать от секунд до минут (см. Resource Registry).
#[async_trait]
pub trait ModelBackend: Send + Sync {
    /// Проверяет, загружена ли модель прямо сейчас.
    async fn is_loaded(&self, model: &str) -> Result<bool>;

    /// Загружает модель в память. Если модель уже загружена — не ошибка.
    async fn load(&self, model: &str) -> Result<()>;

    /// Выгружает модель из памяти. Если модель не была загружена — не ошибка.
    async fn unload(&self, model: &str) -> Result<()>;

    /// Отправляет модели историю сообщений и возвращает её ответ.
    /// Модель должна быть загружена заранее (см. `load`) — этот метод
    /// сам загрузку не делает, это забота Scheduler'а.
    async fn generate(&self, model: &str, request: GenerateRequest) -> Result<GenerateResponse>;

    /// Идентификаторы ВСЕХ инстансов, загруженных в backend прямо сейчас,
    /// включая те, о которых текущий процесс ничего не знает.
    ///
    /// Нужен для стартовой чистки: после аварийного завершения (Ctrl+C, kill,
    /// краш) в backend остаются загруженные модели, а наш учёт при этом
    /// начинается с нуля. Без перечисления мы не можем узнать, что осталось.
    ///
    /// По умолчанию — пустой список: backend вправе не уметь перечислять,
    /// и тогда чистка просто ничего не найдёт.
    async fn loaded_instances(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// Выгружает конкретный инстанс по его идентификатору.
    ///
    /// Отличается от `unload` тем, что оперирует идентификатором инстанса, а
    /// не именем модели: одна и та же модель может быть загружена несколько
    /// раз (`model`, `model:2`, `model:3`), и по имени модели выгрузится
    /// только первый.
    ///
    /// По умолчанию делегирует в `unload` — для backend'ов, где инстанс и
    /// модель это одно и то же.
    async fn unload_instance(&self, instance_id: &str) -> Result<()> {
        self.unload(instance_id).await
    }
}

/// Реализация `ModelBackend` поверх LM Studio.
///
/// Управление моделями (`load`/`unload`/`is_loaded`) идёт через нативный
/// LM Studio REST API v1 (`/api/v1/models*`) — только он умеет явно
/// загружать/выгружать конкретные модели.
///
/// Генерация (`generate`) идёт через OpenAI-совместимый эндпоинт
/// (`/v1/chat/completions`) — он, в отличие от нативного `/api/v1/chat`,
/// принимает полную историю сообщений с ролями (system/user/assistant),
/// что и требуется нашему `GenerateRequest`.
pub struct LmStudioBackend {
    base_url: String,
    api_token: Option<String>,
    client: reqwest::Client,
    /// LM Studio API явно предупреждает: `instance_id`, нужный для `unload`,
    /// не обязан совпадать с ключом модели ("не делайте это предположение").
    /// Поэтому запоминаем реальный `instance_id`, который LM Studio вернула
    /// при загрузке, и используем именно его при выгрузке.
    instance_ids: Mutex<HashMap<String, String>>,
}

impl LmStudioBackend {
    /// Создаёт клиент LM Studio.
    ///
    /// `base_url` — например `http://localhost:1234`.
    /// `api_token` — если в LM Studio включена авторизация; иначе `None`.
    pub fn new(base_url: impl Into<String>, api_token: Option<String>) -> Self {
        Self::with_timeouts(
            base_url,
            api_token,
            crate::http_client::CONNECT_TIMEOUT,
            crate::http_client::LM_STUDIO_TIMEOUT,
        )
    }

    /// Тот же конструктор с явно заданными таймаутами.
    ///
    /// Существует ради проверки поведения при недоступном адресате:
    /// умолчание на ответ — 30 минут, и тест, ждущий его срабатывания, был
    /// бы не тестом, а простоем. Отдельный конструктор честнее, чем
    /// сокращённое «на время тестов» умолчание в самом `new`.
    pub fn with_timeouts(
        base_url: impl Into<String>,
        api_token: Option<String>,
        connect: std::time::Duration,
        response: std::time::Duration,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_token,
            client: crate::http_client::build(connect, response),
            instance_ids: Mutex::new(HashMap::new()),
        }
    }

    fn auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }
}

#[async_trait]
impl ModelBackend for LmStudioBackend {
    async fn is_loaded(&self, model: &str) -> Result<bool> {
        let url = format!("{}/api/v1/models", self.base_url);
        let response: ModelsListResponse = self
            .auth(self.client.get(&url))
            .send()
            .await
            .context("не удалось связаться с LM Studio")?
            .error_for_status()
            .context("LM Studio вернула ошибку при списке моделей")?
            .json()
            .await
            .context("не удалось разобрать ответ LM Studio")?;

        Ok(response
            .models
            .iter()
            .any(|m| m.key == model && !m.loaded_instances.is_empty()))
    }

    async fn load(&self, model: &str) -> Result<()> {
        let url = format!("{}/api/v1/models/load", self.base_url);
        let response: LoadResponse = self
            .auth(self.client.post(&url))
            .json(&LoadRequest { model })
            .send()
            .await
            .context("не удалось отправить запрос на загрузку модели")?
            .error_for_status()
            .context("LM Studio отказала в загрузке модели")?
            .json()
            .await
            .context("не удалось разобрать ответ LM Studio при загрузке модели")?;

        self.instance_ids
            .lock()
            .await
            .insert(model.to_string(), response.instance_id);
        Ok(())
    }

    async fn loaded_instances(&self) -> Result<Vec<String>> {
        let url = format!("{}/api/v1/models", self.base_url);
        let response: ModelsListResponse = self
            .auth(self.client.get(&url))
            .send()
            .await
            .context("не удалось связаться с LM Studio")?
            .error_for_status()
            .context("LM Studio вернула ошибку при списке моделей")?
            .json()
            .await
            .context("не удалось разобрать ответ LM Studio")?;

        Ok(response
            .models
            .into_iter()
            .flat_map(|m| m.loaded_instances.into_iter().map(|i| i.id))
            .collect())
    }

    async fn unload_instance(&self, instance_id: &str) -> Result<()> {
        let url = format!("{}/api/v1/models/unload", self.base_url);
        self.auth(self.client.post(&url))
            .json(&UnloadRequest { instance_id })
            .send()
            .await
            .context("не удалось отправить запрос на выгрузку инстанса")?
            .error_for_status()
            .context("LM Studio отказала в выгрузке инстанса")?;
        Ok(())
    }

    async fn unload(&self, model: &str) -> Result<()> {
        // instance_id, который вернула LM Studio при загрузке — не обязан
        // совпадать с ключом модели (см. документацию). Если по какой-то
        // причине мы не отслеживали эту модель (например, она была
        // загружена не через этот backend), используем имя модели как
        // разумный fallback — так вело себя исходное упрощённое поведение.
        let instance_id = self
            .instance_ids
            .lock()
            .await
            .get(model)
            .cloned()
            .unwrap_or_else(|| model.to_string());

        let url = format!("{}/api/v1/models/unload", self.base_url);
        self.auth(self.client.post(&url))
            .json(&UnloadRequest {
                instance_id: &instance_id,
            })
            .send()
            .await
            .context("не удалось отправить запрос на выгрузку модели")?
            .error_for_status()
            .context("LM Studio отказала в выгрузке модели")?;

        self.instance_ids.lock().await.remove(model);
        Ok(())
    }

    async fn generate(&self, model: &str, request: GenerateRequest) -> Result<GenerateResponse> {
        let url = format!("{}/v1/chat/completions", self.base_url);

        let messages: Vec<WireMessage> = request
            .messages
            .iter()
            .map(|m| WireMessage {
                role: m.role.as_wire_str(),
                content: &m.content,
                tool_call_id: m.tool_call_id.as_deref(),
                tool_calls: if m.tool_calls.is_empty() {
                    None
                } else {
                    Some(
                        m.tool_calls
                            .iter()
                            .map(|c| WireToolCallOut {
                                id: &c.id,
                                kind: "function",
                                function: WireToolCallFunctionOut {
                                    name: &c.name,
                                    arguments: &c.arguments,
                                },
                            })
                            .collect(),
                    )
                },
            })
            .collect();

        // Пустой список инструментов означает «инструментов нет», а не
        // «инструментов ноль»: поле обязано исчезнуть из JSON целиком, иначе
        // прежние запросы перестали бы быть прежними. То же свойство и та же
        // причина, что у `temperature`.
        let tools: Vec<WireTool> = request
            .tools
            .iter()
            .map(|t| WireTool {
                kind: "function",
                function: WireFunction {
                    name: &t.name,
                    description: &t.description,
                    parameters: &t.input_schema,
                },
            })
            .collect();

        let body = ChatCompletionsRequest {
            model,
            messages,
            temperature: request.temperature,
            tools: if tools.is_empty() { None } else { Some(tools) },
        };

        let response: ChatCompletionsResponse = self
            .auth(self.client.post(&url))
            .json(&body)
            .send()
            .await
            .context("не удалось отправить запрос на генерацию")?
            .error_for_status()
            .context("LM Studio вернула ошибку при генерации ответа")?
            .json()
            .await
            .context("не удалось разобрать ответ LM Studio")?;

        // Пустой `choices` — это НЕ ответ пустой строкой, а несостоявшийся
        // ответ: провайдер обязан вернуть хотя бы один вариант. Раньше здесь
        // стоял `unwrap_or_default()`, и разница стиралась молча — задача
        // уходила в Done с пустым сообщением ассистента, а медиа-контур
        // тихо сползал на дословный текст задачи. Механизм работал, отказ
        // был не виден: тот же сюжет, что с `decode_to_string` в
        // kaic-terminal, писавшим в `String` нулевой ёмкости.
        //
        // Граница намеренная: пустая СТРОКА внутри присутствующего варианта
        // остаётся `Ok`. Это ответ, пусть и бессодержательный — провайдер
        // своё обещание выполнил, а «модель ничего не сказала» решается не
        // здесь.
        let message = response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .context("LM Studio вернула ответ без вариантов (пустой choices)")?;

        // Просьба вызвать инструменты важнее текста: при `tool_calls` модели
        // кладут в `content` либо пустоту, либо рассуждение вслух, и принять
        // его за ответ значило бы оборвать цикл на первом же шаге.
        let calls = message.tool_calls.unwrap_or_default();
        if !calls.is_empty() {
            return Ok(GenerateResponse::ToolCalls(
                calls
                    .into_iter()
                    .map(|c| ToolCall {
                        id: c.id,
                        name: c.function.name,
                        arguments: c.function.arguments,
                    })
                    .collect(),
            ));
        }

        Ok(GenerateResponse::Text(message.content.unwrap_or_default()))
    }
}

// --- Формы запросов/ответов LM Studio REST API ---
// Namespace приватный: наружу торчит только `ModelBackend`, детали
// протокола LM Studio никого выше не касаются.

#[derive(Serialize)]
struct LoadRequest<'a> {
    model: &'a str,
}

#[derive(Serialize)]
struct UnloadRequest<'a> {
    instance_id: &'a str,
}

#[derive(Deserialize)]
struct LoadResponse {
    instance_id: String,
}

#[derive(Deserialize)]
struct ModelsListResponse {
    models: Vec<ModelInfo>,
}

#[derive(Deserialize)]
struct ModelInfo {
    key: String,
    loaded_instances: Vec<LoadedInstance>,
}

#[derive(Deserialize)]
struct LoadedInstance {
    #[allow(dead_code)]
    id: String,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    content: &'a str,
    /// Присутствует только у сообщений роли `tool` — по нему провайдер
    /// сопоставляет результат с тем вызовом, который сам же и запросил.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
    /// Присутствует только у реплики ассистента, запросившей инструменты.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireToolCallOut<'a>>>,
}

#[derive(Serialize)]
struct WireToolCallOut<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
    function: WireToolCallFunctionOut<'a>,
}

#[derive(Serialize)]
struct WireToolCallFunctionOut<'a> {
    name: &'a str,
    /// Строка, а не объект — так их прислал провайдер, и так же он ждёт их
    /// обратно. Пересобирать JSON по дороге значило бы менять то, что модель
    /// считает своей же репликой.
    arguments: &'a str,
}

#[derive(Serialize)]
struct WireTool<'a> {
    /// Единственное значение, которое понимает схема — `"function"`.
    #[serde(rename = "type")]
    kind: &'a str,
    function: WireFunction<'a>,
}

#[derive(Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    description: &'a str,
    /// JSON Schema аргументов уходит провайдеру дословно: она пришла от
    /// MCP-сервера, и переписывать её по дороге значило бы врать модели о
    /// том, что инструмент принимает.
    parameters: &'a serde_json::Value,
}

#[derive(Serialize)]
struct ChatCompletionsRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    /// `skip_serializing_if` здесь не косметика: если сериализовать
    /// `null`, LM Studio получит явное «температура не задана числом» и
    /// ответит ошибкой схемы. Поле должно ИСЧЕЗАТЬ из JSON целиком.
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    /// Тот же `skip_serializing_if` и по той же причине: запрос без
    /// инструментов обязан быть побайтово прежним запросом.
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<WireTool<'a>>>,
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    fn body(temperature: Option<f32>) -> serde_json::Value {
        serde_json::to_value(ChatCompletionsRequest {
            model: "any",
            messages: vec![WireMessage {
                role: "user",
                content: "привет",
                tool_call_id: None,
                tool_calls: None,
            }],
            temperature,
            tools: None,
        })
        .expect("запрос сериализуется")
    }

    /// Тело запроса, собранное ПОЛНЫМ путём — из `GenerateRequest`, а не
    /// вручную. Только так проверяется, что пустой список инструментов
    /// действительно не доходит до провайдера.
    fn body_from_request(tools: Vec<ToolSpec>) -> serde_json::Value {
        let request = GenerateRequest {
            messages: vec![Message::new(Role::User, "привет")],
            temperature: None,
            tools,
        };
        let wire_messages: Vec<WireMessage> = request
            .messages
            .iter()
            .map(|m| WireMessage {
                role: m.role.as_wire_str(),
                content: &m.content,
                tool_call_id: m.tool_call_id.as_deref(),
                tool_calls: None,
            })
            .collect();
        let wire_tools: Vec<WireTool> = request
            .tools
            .iter()
            .map(|t| WireTool {
                kind: "function",
                function: WireFunction {
                    name: &t.name,
                    description: &t.description,
                    parameters: &t.input_schema,
                },
            })
            .collect();
        serde_json::to_value(ChatCompletionsRequest {
            model: "any",
            messages: wire_messages,
            temperature: request.temperature,
            tools: if wire_tools.is_empty() {
                None
            } else {
                Some(wire_tools)
            },
        })
        .expect("запрос сериализуется")
    }

    #[test]
    fn no_tools_means_the_request_is_byte_for_byte_the_old_one() {
        // Главное требование шага: появление инструментов не должно менять
        // НИ ОДНОГО прежнего запроса. Пустой список — это «инструментов
        // нет», а не «инструментов ноль», и ключа в JSON быть не должно.
        let json = body_from_request(Vec::new());
        assert!(
            json.get("tools").is_none(),
            "ключ tools присутствует у запроса без инструментов: {json}"
        );
        // И ни одно сообщение не обросло полями вызова инструментов.
        let message = &json["messages"][0];
        assert!(message.get("tool_call_id").is_none(), "{message}");
        assert!(message.get("tool_calls").is_none(), "{message}");
    }

    #[test]
    fn a_tool_reaches_the_provider_with_its_schema_untouched() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "code": { "type": "string" } },
            "required": ["code"]
        });
        let json = body_from_request(vec![ToolSpec {
            name: "execute_blender_code".to_string(),
            description: "Выполнить код в Blender".to_string(),
            input_schema: schema.clone(),
        }]);

        let tool = &json["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "execute_blender_code");
        // Схема уходит дословно: переписать её по дороге значило бы соврать
        // модели о том, что инструмент принимает.
        assert_eq!(tool["function"]["parameters"], schema);
    }

    #[test]
    fn a_tool_result_carries_its_call_id_back() {
        let message = Message::tool_result("call_42", "{\"ok\":true}");
        let wire = WireMessage {
            role: message.role.as_wire_str(),
            content: &message.content,
            tool_call_id: message.tool_call_id.as_deref(),
            tool_calls: None,
        };
        let json = serde_json::to_value(wire).expect("сериализуется");

        assert_eq!(json["role"], "tool");
        // Без идентификатора провайдер не сопоставит результат с вызовом и
        // отвергнет всю историю.
        assert_eq!(json["tool_call_id"], "call_42");
    }

    #[test]
    fn temperature_none_is_absent_from_request_body() {
        // Главная гарантия задачи: при `None` умолчание провайдера должно
        // сохраниться в точности, а для этого ключа не должно быть ВООБЩЕ —
        // `"temperature": null` LM Studio считает ошибкой схемы.
        // Живой прогон это подтверждал, но рефакторинг снял бы
        // `skip_serializing_if` молча — отсюда тест.
        let json = body(None);
        assert!(
            json.get("temperature").is_none(),
            "ключ temperature присутствует: {json}"
        );
    }

    #[test]
    fn temperature_some_is_present_in_request_body() {
        let json = body(Some(0.7));
        assert_eq!(
            json.get("temperature").and_then(|v| v.as_f64()),
            Some(0.7_f32 as f64)
        );
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;
    use std::time::Duration;

    /// Сервер, который принимает соединение и молчит.
    ///
    /// Именно этот отказ, а не «порт закрыт»: закрытый порт отвергает
    /// соединение мгновенно и висеть не даёт вовсе, поэтому он ничего не
    /// проверяет. Опасен ровно противоположный случай — TCP установлен,
    /// ответа нет. До таймаута он подвешивал задачу навсегда.
    async fn silent_server() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("порт для подставного сервера нашёлся");
        let addr = listener.local_addr().expect("адрес известен");
        let handle = tokio::spawn(async move {
            // Соединения удерживаются, а не закрываются: закрытие дало бы
            // клиенту ошибку сразу и снова ничего бы не проверило.
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn generation_fails_on_time_instead_of_hanging_forever() {
        let (base_url, server) = silent_server().await;
        let backend = LmStudioBackend::with_timeouts(
            base_url,
            None,
            Duration::from_secs(5),
            Duration::from_millis(300),
        );

        let started = std::time::Instant::now();
        let outcome = backend
            .generate(
                "любая",
                GenerateRequest {
                    messages: Vec::new(),
                    temperature: None,
                    tools: Vec::new(),
                },
            )
            .await;
        let elapsed = started.elapsed();

        assert!(
            outcome.is_err(),
            "молчащий сервер обязан дать ошибку, а не ответ"
        );
        // Верхняя граница — предсказуемость: без таймаута этот вызов не
        // завершается вообще, и тест не «падал» бы, а висел.
        assert!(
            elapsed < Duration::from_secs(5),
            "отказ занял {elapsed:?} — это не предсказуемое время"
        );
        // Нижняя граница — что сработал именно таймаут, а не отказ
        // соединения: иначе тест зеленел бы и с оборванным сервером.
        assert!(
            elapsed >= Duration::from_millis(300),
            "отказ пришёл раньше таймаута ({elapsed:?}) — сработало что-то другое"
        );

        server.abort();
    }

    /// Сервер, отдающий один и тот же заранее заданный JSON.
    ///
    /// Нужен, чтобы проверить разбор ответа провайдера, не поднимая
    /// LM Studio и не загружая ни одной модели.
    async fn server_answering(body: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("порт для подставного сервера нашёлся");
        let addr = listener.local_addr().expect("адрес известен");
        let handle = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    // Запрос надо вычитать, иначе клиент получит обрыв вместо
                    // ответа. Тело запроса здесь заведомо меньше буфера.
                    let mut buf = [0u8; 4096];
                    let _ = socket.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), handle)
    }

    fn empty_request() -> GenerateRequest {
        GenerateRequest {
            messages: Vec::new(),
            temperature: None,
            tools: Vec::new(),
        }
    }

    #[tokio::test]
    async fn an_answer_without_choices_is_an_error_not_an_empty_string() {
        // Провайдер обязан вернуть хотя бы один вариант. Пустой `choices` —
        // это несостоявшийся ответ, а не ответ пустой строкой, и раньше
        // `unwrap_or_default()` стирал разницу молча: задача уходила в Done
        // с пустым сообщением ассистента.
        let (base_url, server) = server_answering(r#"{"choices":[]}"#).await;
        let backend = LmStudioBackend::with_timeouts(
            base_url,
            None,
            Duration::from_secs(5),
            Duration::from_secs(5),
        );

        // `match`, а не `expect_err`: тот требует `Debug` на типе ответа, а
        // трогать объявление `GenerateResponse` эта задача не должна.
        let text = match backend.generate("любая", empty_request()).await {
            Ok(response) => panic!(
                "ответ без вариантов обязан быть отказом, а вернулось Ok({:?})",
                response.text()
            ),
            Err(err) => format!("{err:#}"),
        };
        // Текст пинуется целиком, а не подстрокой: он уходит в контекст
        // задачи как «Ошибка: {текст}» и читается человеком. Общее
        // «ошибка генерации» было бы тем же молчаливым отказом, только с
        // другим лицом.
        assert_eq!(text, "LM Studio вернула ответ без вариантов (пустой choices)");

        server.abort();
    }

    #[tokio::test]
    async fn a_well_formed_answer_still_comes_through() {
        // Парная проверка: без неё тест выше зеленел бы и от сломанной
        // оснастки — например если бы подставной сервер вообще не отвечал.
        let (base_url, server) =
            server_answering(r#"{"choices":[{"message":{"content":"горное озеро"}}]}"#).await;
        let backend = LmStudioBackend::with_timeouts(
            base_url,
            None,
            Duration::from_secs(5),
            Duration::from_secs(5),
        );

        let response = backend
            .generate("любая", empty_request())
            .await
            .expect("нормальный ответ обязан пройти");

        assert_eq!(response.text(), Some("горное озеро"));

        server.abort();
    }

    #[tokio::test]
    async fn a_tool_call_answer_becomes_tool_calls_not_text() {
        // Разбор ответа с `tool_calls` — одна из трёх точек карты, требующих
        // ветвления. Проверяется на реальной форме ответа провайдера, а не на
        // сконструированном значении: именно разбор и мог разойтись со схемой.
        let (base_url, server) = server_answering(
            r#"{"choices":[{"message":{"content":null,"tool_calls":[
                {"id":"call_1","type":"function","function":
                 {"name":"get_scene_info","arguments":"{}"}}]}}]}"#,
        )
        .await;
        let backend = LmStudioBackend::with_timeouts(
            base_url,
            None,
            Duration::from_secs(5),
            Duration::from_secs(5),
        );

        let response = backend
            .generate("любая", empty_request())
            .await
            .expect("ответ разбирается");

        match response {
            GenerateResponse::ToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_1");
                assert_eq!(calls[0].name, "get_scene_info");
                assert_eq!(calls[0].arguments, "{}");
            }
            // Если `content: null` победит `tool_calls`, цикл оборвётся на
            // первом шаге и модель никогда не получит результат инструмента.
            GenerateResponse::Text(text) => {
                panic!("вызов инструмента принят за текст: {text:?}")
            }
        }

        server.abort();
    }

    #[tokio::test]
    async fn model_management_calls_are_capped_too() {
        // Таймаут стоит на клиенте, а не на одном вызове: `load` в
        // Scheduler'е держит `gpu_lock`, и повисший `load` заблокировал бы
        // не одну задачу, а любую смену набора моделей.
        let (base_url, server) = silent_server().await;
        let backend = LmStudioBackend::with_timeouts(
            base_url,
            None,
            Duration::from_secs(5),
            Duration::from_millis(300),
        );

        let started = std::time::Instant::now();
        assert!(backend.load("любая").await.is_err());
        assert!(backend.is_loaded("любая").await.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "управляющие вызовы не уложились в предсказуемое время"
        );

        server.abort();
    }
}

#[derive(Deserialize)]
struct ChatCompletionsResponse {
    choices: Vec<ChatCompletionsChoice>,
}

#[derive(Deserialize)]
struct ChatCompletionsChoice {
    message: ChatCompletionsMessage,
}

#[derive(Deserialize)]
struct ChatCompletionsMessage {
    /// При `tool_calls` провайдер вправе не прислать `content` вовсе —
    /// поэтому `Option`, а не `String` с умолчанием.
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<WireToolCall>>,
}

#[derive(Deserialize)]
struct WireToolCall {
    id: String,
    function: WireToolCallFunction,
}

#[derive(Deserialize)]
struct WireToolCallFunction {
    name: String,
    /// Строка с JSON, а не разобранный объект: провайдер отдаёт именно
    /// строку, и разбирать её здесь значило бы уронить весь ответ из-за
    /// одного кривого аргумента.
    arguments: String,
}
