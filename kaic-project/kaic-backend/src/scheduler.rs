//! Scheduler — диспетчер ресурсов GPU.
//!
//! Scheduler не знает о содержании задач и не принимает решений о том,
//! "какая модель лучше" — этим занимаются Router и Capability Registry.
//! У Scheduler ровно четыре обязанности:
//!
//! 1. Получить от Capability Registry упорядоченный список кандидатов
//!    для категории задачи.
//! 2. Проверить через Resource Registry, хватает ли VRAM для кандидата.
//! 3. При нехватке — освободить память простым LRU, никогда не трогая
//!    модели с `always_loaded == true`.
//! 4. Вызвать `ModelBackend`, чтобы получить ответ.
//!
//! Никакого DAG, приоритетов, worker pool или очереди здесь нет —
//! Tokio уже даёт асинхронный runtime, Scheduler поверх него — обычная
//! бизнес-логика.
//!
//! Параллелизм: `run()` целиком выполняется под одной блокировкой —
//! на время всего цикла (подбор модели + генерация) система обслуживает
//! только один запрос. Для одного пользователя на одной видеокарте это
//! осознанное упрощение: настоящая конкурентная работа с несколькими
//! одновременно загруженными моделями всё равно упирается в одно и то же
//! железо, а счётчик "модель сейчас используется" — это сложность, которую
//! стоит добавить только когда реальный сценарий её потребует.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use crate::capability_registry::CapabilityRegistry;
use crate::model_backend::{GenerateRequest, GenerateResponse, ModelBackend};
use crate::resource_registry::ResourceRegistry;

/// Имя модели, используемое как идентификатор в Model Backend.
pub type ModelId = String;

/// Результат подбора модели для категории задачи.
enum SchedulerResult {
    /// Модель выбрана и загружена, можно генерировать ответ.
    Ready(ModelId),
    /// Не удалось подобрать модель: нет кандидатов, не хватает VRAM
    /// у всех кандидатов даже после вытеснения, категория помечена
    /// `manual_only` без разрешения, или backend отказал.
    Failed(String),
}

/// Учёт использования одной загруженной модели — нужен только для LRU.
///
/// Вынесен в отдельную структуру, чтобы политику вытеснения можно было
/// заменить позже (например, на учёт частоты использования), не трогая
/// остальной Scheduler.
struct ModelUsage {
    last_used: DateTime<Utc>,
}

/// Диспетчер ресурсов GPU.
pub struct Scheduler {
    capability_registry: Arc<CapabilityRegistry>,
    resource_registry: Arc<ResourceRegistry>,
    backend: Arc<dyn ModelBackend>,

    /// Общий объём VRAM (в мегабайтах), доступный системе.
    total_vram_mb: u32,

    /// Единственная блокировка на весь цикл `run()`: подбор модели
    /// (включая возможную загрузку/вытеснение) и сама генерация —
    /// всё это выполняется под ней, поэтому одновременно система
    /// обслуживает только один запрос. См. модульный комментарий.
    gpu_lock: Mutex<()>,

    /// Модели, которые Scheduler считает загруженными прямо сейчас.
    /// Это единственный источник правды о состоянии VRAM — поэтому
    /// LM Studio JIT-loading и Auto-Evict должны быть отключены
    /// (см. model_backend.rs), иначе это состояние разойдётся с реальностью.
    loaded: Mutex<HashMap<ModelId, ModelUsage>>,
}

impl Scheduler {
    /// Создаёт Scheduler.
    ///
    /// `total_vram_mb` — объём видеопамяти на конкретной машине
    /// (например 8000 для RTX 4060).
    pub fn new(
        capability_registry: Arc<CapabilityRegistry>,
        resource_registry: Arc<ResourceRegistry>,
        backend: Arc<dyn ModelBackend>,
        total_vram_mb: u32,
    ) -> Self {
        Self {
            capability_registry,
            resource_registry,
            backend,
            total_vram_mb,
            gpu_lock: Mutex::new(()),
            loaded: Mutex::new(HashMap::new()),
        }
    }

    /// Общий объём VRAM, известный Scheduler'у (для отображения в Control Center).
    pub fn total_vram_mb(&self) -> u32 {
        self.total_vram_mb
    }

    /// Возвращает список моделей, которые Scheduler считает загруженными
    /// прямо сейчас, вместе с временем последнего использования.
    /// Используется Control Center API для GET /models и GET /status —
    /// сам Scheduler в принятии решений эти данные наружу не отдаёт.
    pub async fn snapshot(&self) -> Vec<(ModelId, DateTime<Utc>)> {
        let loaded = self.loaded.lock().await;
        loaded
            .iter()
            .map(|(name, usage)| (name.clone(), usage.last_used))
            .collect()
    }

    /// Загружает все `always_loaded` модели при старте системы
    /// и отмечает их как резидентные. Вызывается один раз в `main.rs`.
    pub async fn preload_always_loaded(&self) -> Result<()> {
        // Проверяем ещё до первого вызова backend.load(), что always_loaded
        // модели вообще помещаются в доступную VRAM — иначе ошибка всплыла бы
        // только как сырой отказ от LM Studio при загрузке, а тут причина
        // видна сразу и явно.
        let required = self.resource_registry.always_loaded_vram_mb();
        if required > self.total_vram_mb {
            return Err(anyhow!(
                "always_loaded модели требуют {required} MB VRAM, \
                 а доступно только {} MB — проверьте resource_registry.yaml",
                self.total_vram_mb
            ));
        }

        let mut loaded = self.loaded.lock().await;
        for model in self.resource_registry.always_loaded_models() {
            self.backend
                .load(model)
                .await
                .map_err(|e| anyhow!("не удалось загрузить always_loaded модель {model}: {e}"))?;
            loaded.insert(
                model.to_string(),
                ModelUsage {
                    last_used: Utc::now(),
                },
            );
        }
        Ok(())
    }

    /// Выполняет полный цикл: подбирает модель для категории и получает
    /// от неё ответ на историю сообщений.
    ///
    /// `allow_manual` — разрешает выбор моделей с `manual_only == true`
    /// (например Qwen40B). Должно быть `true` только по явному запросу
    /// пользователя, никогда по умолчанию.
    pub async fn run(
        &self,
        category: &str,
        allow_manual: bool,
        request: GenerateRequest,
    ) -> Result<GenerateResponse> {
        // Блокировка держится на весь цикл — до завершения генерации,
        // а не только на подбор модели. Иначе другой запрос мог бы
        // выгрузить эту же модель прямо во время её использования.
        let Ok(_guard) = self.gpu_lock.try_lock() else {
            return Err(anyhow!("GPU сейчас занят другой задачей, повторите позже"));
        };

        match self.select_model(category, allow_manual).await {
            SchedulerResult::Ready(model_id) => self.backend.generate(&model_id, request).await,
            SchedulerResult::Failed(reason) => Err(anyhow!(reason)),
        }
    }

    /// Подбирает и при необходимости загружает модель для категории задачи.
    /// Отвечает за обязанности 1–3 (кандидаты, проверка ресурсов, LRU).
    ///
    /// Предполагает, что вызывающий код (`run`) уже держит `gpu_lock` —
    /// сам за блокировку не отвечает.
    async fn select_model(&self, category: &str, allow_manual: bool) -> SchedulerResult {
        let candidates = self.capability_registry.candidates(category);
        if candidates.is_empty() {
            return SchedulerResult::Failed(format!(
                "категория '{category}' не найдена в Capability Registry"
            ));
        }

        if self.capability_registry.is_manual_only(category) && !allow_manual {
            return SchedulerResult::Failed(format!(
                "категория '{category}' требует явного запроса пользователя (manual_only)"
            ));
        }

        let mut loaded = self.loaded.lock().await;

        for candidate in candidates {
            let Some(resource) = self.resource_registry.get(candidate) else {
                tracing::warn!(
                    "модель '{candidate}' есть в Capability Registry, но отсутствует \
                     в Resource Registry — пропускаю"
                );
                continue;
            };

            if let Some(usage) = loaded.get_mut(candidate) {
                usage.last_used = Utc::now();
                return SchedulerResult::Ready(candidate.to_string());
            }

            if self
                .fit_and_load(candidate, resource.vram_mb, &mut loaded)
                .await
            {
                return SchedulerResult::Ready(candidate.to_string());
            }
            // Кандидат не поместился даже после вытеснения — пробуем следующий
            // по fallback-цепочке, не прерываем весь подбор.
        }

        SchedulerResult::Failed(format!(
            "не удалось подобрать модель для категории '{category}': \
             не хватает VRAM у всех кандидатов или backend недоступен"
        ))
    }

    /// Пытается разместить модель `candidate` в VRAM: сначала без вытеснения,
    /// затем вытесняя по LRU (никогда не трогая always_loaded), пока либо
    /// не найдётся места, либо вытеснять больше нечего.
    async fn fit_and_load(
        &self,
        candidate: &str,
        needed_vram_mb: u32,
        loaded: &mut HashMap<ModelId, ModelUsage>,
    ) -> bool {
        loop {
            let used_vram_mb: u32 = loaded
                .keys()
                .filter_map(|name| self.resource_registry.get(name))
                .map(|r| r.vram_mb)
                .sum();

            if used_vram_mb + needed_vram_mb <= self.total_vram_mb {
                return self.load_and_track(candidate, loaded).await;
            }

            match self.pick_eviction_victim(loaded) {
                Some(victim) => {
                    if self.backend.unload(&victim).await.is_err() {
                        tracing::warn!("не удалось выгрузить модель '{victim}' для освобождения VRAM");
                        return false;
                    }
                    loaded.remove(&victim);
                }
                None => return false, // вытеснять больше нечего, кандидат не помещается
            }
        }
    }

    /// Выбирает модель для вытеснения: не always_loaded, дольше всего
    /// не использовавшаяся из загруженных сейчас.
    fn pick_eviction_victim(&self, loaded: &HashMap<ModelId, ModelUsage>) -> Option<ModelId> {
        loaded
            .iter()
            .filter(|(name, _)| {
                !self
                    .resource_registry
                    .get(name.as_str())
                    .map(|r| r.always_loaded)
                    .unwrap_or(false)
            })
            .min_by_key(|(_, usage)| usage.last_used)
            .map(|(name, _)| name.clone())
    }

    async fn load_and_track(&self, candidate: &str, loaded: &mut HashMap<ModelId, ModelUsage>) -> bool {
        if self.backend.load(candidate).await.is_err() {
            tracing::warn!("не удалось загрузить модель '{candidate}'");
            return false;
        }
        loaded.insert(
            candidate.to_string(),
            ModelUsage {
                last_used: Utc::now(),
            },
        );
        true
    }
}
