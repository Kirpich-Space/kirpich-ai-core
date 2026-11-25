//! Headless llama-server в single-model mode. Модель запускается оператором;
//! адаптер не выдаёт проверку доступности за загрузку или выгрузку.
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::model_backend::{
    generate_openai, DiscoveredModel, GenerateRequest, GenerateResponse, ModelBackend,
};

pub struct LlamaServerBackend {
    base: reqwest::Url,
    client: reqwest::Client,
    token: Option<String>,
    diagnostic_timeout: Duration,
}

impl LlamaServerBackend {
    pub fn new(base: &str, token: Option<String>) -> Result<Self> {
        Self::with_timeouts(
            base,
            token,
            Duration::from_secs(2),
            Duration::from_secs(1800),
        )
    }

    pub fn with_timeouts(
        base: &str,
        token: Option<String>,
        diagnostic_timeout: Duration,
        generation_timeout: Duration,
    ) -> Result<Self> {
        let base = reqwest::Url::parse(base).context("неверный URL llama-server")?;
        let local = matches!(
            base.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        );
        ensure!(
            base.scheme() == "http"
                && local
                && base.username().is_empty()
                && base.password().is_none()
                && base.query().is_none()
                && base.fragment().is_none()
                && base.path() == "/",
            "llama-server URL должен быть loopback HTTP без credentials, пути и query"
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(diagnostic_timeout)
            .timeout(generation_timeout)
            .build()?;
        Ok(Self {
            base,
            client,
            token,
            diagnostic_timeout,
        })
    }

    fn auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(token) = &self.token {
            builder.bearer_auth(token)
        } else {
            builder
        }
    }

    async fn ready(&self) -> Result<()> {
        #[derive(Deserialize)]
        struct Health {
            status: String,
        }
        let health: Health = self
            .auth(self.client.get(self.base.join("health")?))
            .timeout(self.diagnostic_timeout)
            .send()
            .await
            .context("нет соединения с llama-server /health")?
            .error_for_status()
            .context("llama-server ещё не готов или недоступен")?
            .json()
            .await
            .context("неверный ответ llama-server /health")?;
        ensure!(health.status == "ok", "llama-server не готов: status != ok");
        Ok(())
    }
}

#[derive(Deserialize)]
struct Models {
    data: Vec<DiscoveredModel>,
}

#[async_trait]
impl ModelBackend for LlamaServerBackend {
    fn externally_managed(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "llama-server"
    }

    fn supports_discovery(&self) -> bool {
        true
    }

    async fn discover_models(&self) -> Result<Vec<DiscoveredModel>> {
        self.ready().await?;
        let models: Models = self
            .auth(self.client.get(self.base.join("v1/models")?))
            .timeout(self.diagnostic_timeout)
            .send()
            .await
            .context("не удалось запросить модели llama-server")?
            .error_for_status()
            .context("llama-server отказал в discovery")?
            .json()
            .await
            .context("неверный ответ llama-server /v1/models")?;
        ensure!(
            models.data.len() == 1,
            "ожидалась одна модель single-model llama-server; router mode не поддержан"
        );
        let model = &models.data[0];
        ensure!(
            !model.id.trim().is_empty(),
            "llama-server вернул пустой model ID"
        );
        ensure!(
            model.status.is_none(),
            "router mode llama-server не поддержан этим адаптером"
        );
        Ok(models.data)
    }

    async fn is_loaded(&self, model: &str) -> Result<bool> {
        Ok(self
            .discover_models()
            .await?
            .iter()
            .any(|entry| entry.id == model))
    }

    async fn load(&self, _: &str) -> Result<()> {
        anyhow::bail!("загрузка не поддержана: запустите llama-server с нужной GGUF-моделью")
    }

    async fn unload(&self, _: &str) -> Result<()> {
        anyhow::bail!("выгрузка не поддержана: моделью владеет внешний llama-server")
    }

    async fn generate(&self, model: &str, request: GenerateRequest) -> Result<GenerateResponse> {
        ensure!(
            self.is_loaded(model).await?,
            "модель '{model}' не обслуживается llama-server; проверьте model_id и --alias"
        );
        // autoload=false дополнительно исключает скрытую загрузку, если endpoint
        // был заменён router-сервером между discovery и генерацией.
        let mut url = self.base.join("v1/chat/completions")?;
        url.query_pairs_mut().append_pair("autoload", "false");
        generate_openai(self.auth(self.client.post(url)), model, request).await
    }
}

#[cfg(test)]
#[path = "llama_server_tests.rs"]
mod tests;
