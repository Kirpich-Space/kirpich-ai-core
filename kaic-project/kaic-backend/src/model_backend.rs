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
}

impl Role {
    fn as_wire_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// Одно сообщение в истории диалога с моделью.
pub struct Message {
    pub role: Role,
    pub content: String,
}

/// Запрос на генерацию ответа модели.
///
/// Содержит полную историю сообщений, а не одну реплику — это позволяет
/// Agent'у передавать накопленный контекст задачи (см. Task Store) без
/// изменения этого интерфейса, когда HITL-цикл продолжает работу после
/// паузы на решение пользователя.
pub struct GenerateRequest {
    pub messages: Vec<Message>,

    /// Температура сэмплинга. `None` — поле НЕ отправляется провайдеру
    /// вообще, и действует его умолчание. Это важное свойство, а не
    /// деталь: до появления этого поля backend никогда не слал
    /// temperature, и `None` обязан сохранять ровно прежнее поведение.
    pub temperature: Option<f32>,
}

/// Ответ модели.
pub struct GenerateResponse {
    /// Текстовое содержимое ответа.
    pub content: String,
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
        Self {
            base_url: base_url.into(),
            api_token,
            client: reqwest::Client::new(),
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
            })
            .collect();

        let body = ChatCompletionsRequest {
            model,
            messages,
            temperature: request.temperature,
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

        let content = response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_default();

        Ok(GenerateResponse { content })
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
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    fn body(temperature: Option<f32>) -> serde_json::Value {
        serde_json::to_value(ChatCompletionsRequest {
            model: "any",
            messages: vec![WireMessage { role: "user", content: "привет" }],
            temperature,
        })
        .expect("запрос сериализуется")
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
    content: String,
}
