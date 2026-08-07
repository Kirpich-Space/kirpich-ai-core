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

/// Помещается ли модель в бюджет памяти.
///
/// ЕДИНСТВЕННОЕ место, где живёт это правило. Его вызывают двое: реальное
/// размещение (`fit_and_load`) и стартовая диагностика (`reachability`).
/// Диагностика обязана отвечать ровно то же, что решит Scheduler, — иначе
/// она начнёт врать, а врущая диагностика хуже её отсутствия.
fn fits_in_budget(used_mb: u32, needed_mb: u32, budget_mb: u32) -> bool {
    used_mb + needed_mb <= budget_mb
}

/// Что система реально может при текущем бюджете и реестре.
///
/// Считается один раз при старте. Все выводы получены через
/// `fits_in_budget` — той же функцией, которой Scheduler принимает решения.
pub struct ReachabilityReport {
    pub budget_mb: u32,
    /// Сколько бюджета занято резидентами постоянно: они исключены из
    /// вытеснения, поэтому это пол, ниже которого свободная память не растёт.
    pub resident_mb: u32,
    /// Модели, которые не помещаются в бюджет даже поверх одних резидентов,
    /// то есть не могут быть выбраны никогда. Имя и его `vram_mb`.
    pub unreachable: Vec<(String, u32)>,
    /// Категории, у которых ни один кандидат из цепочки не проходит.
    pub empty_categories: Vec<String>,
    /// Категории, которые обслуживает не primary, а fallback:
    /// (категория, назначенная модель, фактическая).
    pub served_by_fallback: Vec<(String, String, String)>,
}

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

    /// Память (в мегабайтах), доступная моделям: оперативная плюс
    /// видеопамять, за вычетом резерва под ОС. Не видеопамять — см.
    /// обоснование у константы в main.rs.
    total_model_memory_mb: u32,

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
    /// `total_model_memory_mb` — сколько памяти машины отдано моделям
    /// (RAM + VRAM минус резерв под ОС), а НЕ объём видеопамяти.
    pub fn new(
        capability_registry: Arc<CapabilityRegistry>,
        resource_registry: Arc<ResourceRegistry>,
        backend: Arc<dyn ModelBackend>,
        total_model_memory_mb: u32,
    ) -> Self {
        Self {
            capability_registry,
            resource_registry,
            backend,
            total_model_memory_mb,
            gpu_lock: Mutex::new(()),
            loaded: Mutex::new(HashMap::new()),
        }
    }

    /// Бюджет памяти под модели, известный Scheduler'у
    /// (для отображения в Control Center).
    pub fn total_model_memory_mb(&self) -> u32 {
        self.total_model_memory_mb
    }

    /// Что система может при текущем бюджете: какие модели недостижимы,
    /// какие категории пусты, какие обслуживаются не своей моделью.
    ///
    /// Ничего не меняет и ни на что не влияет — только читает реестры.
    /// Выяснение этих двух фактов вручную занимало по отдельной задаче
    /// каждый раз; система знает их сама и обязана сказать.
    ///
    /// Проверка вместимости идёт через `fits_in_budget` — ту же функцию,
    /// которой пользуется `fit_and_load`, а не через её копию.
    pub fn reachability(&self) -> ReachabilityReport {
        let resident_mb = self.resource_registry.always_loaded_vram_mb();

        // Достижима ли модель в принципе: резиденты вытеснению не подлежат,
        // поэтому лучший возможный случай для любой модели — она одна поверх
        // резидентов. Сами резиденты меряются относительно остальных
        // резидентов, иначе модель считалась бы конкурентом самой себе.
        let is_reachable = |name: &str, entry: &crate::resource_registry::ResourceEntry| {
            let floor = if entry.always_loaded {
                resident_mb.saturating_sub(entry.vram_mb)
            } else {
                resident_mb
            };
            let _ = name;
            fits_in_budget(floor, entry.vram_mb, self.total_model_memory_mb)
        };

        let mut unreachable: Vec<(String, u32)> = self
            .resource_registry
            .all()
            .filter(|(name, entry)| !is_reachable(name, entry))
            .map(|(name, entry)| (name.clone(), entry.vram_mb))
            .collect();
        unreachable.sort_by_key(|(_, mb)| std::cmp::Reverse(*mb));

        let mut empty_categories = Vec::new();
        let mut served_by_fallback = Vec::new();

        let mut categories = self.capability_registry.categories();
        categories.sort_unstable();

        for category in categories {
            let candidates = self.capability_registry.candidates(category);
            let primary = candidates.first().map(|s| s.to_string());

            let winner = candidates.iter().find(|candidate| {
                self.resource_registry
                    .get(candidate)
                    .is_some_and(|entry| is_reachable(candidate, entry))
            });

            match (winner, primary) {
                (None, _) => empty_categories.push(category.to_string()),
                (Some(actual), Some(primary)) if **actual != primary => {
                    served_by_fallback.push((category.to_string(), primary, actual.to_string()));
                }
                _ => {}
            }
        }

        ReachabilityReport {
            budget_mb: self.total_model_memory_mb,
            resident_mb,
            unreachable,
            empty_categories,
            served_by_fallback,
        }
    }

    /// Печатает отчёт о достижимости в лог запуска.
    ///
    /// Только сообщение: старт не блокируется, поведение подбора модели
    /// не меняется. Если всё достижимо — одна строка, а не тишина: тишину
    /// нельзя отличить от «диагностика не отработала».
    pub fn log_reachability(&self) {
        let report = self.reachability();

        tracing::info!(
            "== достижимость == бюджет {} MB, из них {} MB заняты резидентами постоянно \
             (свободно под задачу: {} MB)",
            report.budget_mb,
            report.resident_mb,
            report.budget_mb.saturating_sub(report.resident_mb),
        );

        if report.unreachable.is_empty() {
            tracing::info!("== достижимость == все модели реестра достижимы");
        } else {
            for (name, mb) in &report.unreachable {
                tracing::warn!(
                    "== достижимость == модель '{name}' НЕДОСТИЖИМА: {mb} MB + {} MB резидентов \
                     > {} MB бюджета",
                    report.resident_mb,
                    report.budget_mb,
                );
            }
        }

        for category in &report.empty_categories {
            tracing::warn!(
                "== достижимость == категория '{category}' ПУСТА: ни один кандидат цепочки \
                 не проходит, задачи в ней будут падать"
            );
        }

        for (category, primary, actual) in &report.served_by_fallback {
            tracing::warn!(
                "== достижимость == категория '{category}' обслуживается fallback'ом \
                 '{actual}', а не назначенной '{primary}'"
            );
        }

        if report.empty_categories.is_empty() && report.served_by_fallback.is_empty() {
            tracing::info!("== достижимость == все категории обслуживаются назначенными моделями");
        }
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
    /// Выгружает всё, что осталось в backend'е от прошлых запусков.
    ///
    /// Вызывается на старте, ДО `preload_always_loaded`. Причина не в
    /// аккуратности, а в наблюдённом отказе: `preload_always_loaded` грузит
    /// always_loaded-модели при каждом запуске, парной выгрузки при
    /// завершении процесса нет, и после нескольких перезапусков в LM Studio
    /// накопилось шесть инстансов одной модели — седьмой уже не влез, и
    /// backend перестал стартовать вовсе.
    ///
    /// Чистка сделана на СТАРТЕ, а не на выходе, потому что выход бывает
    /// разный: Ctrl+C, kill из диспетчера, краш, отключение питания. Ни один
    /// из них не обязан выполнить наш код. Стартовая чистка не зависит от
    /// того, как завершился прошлый процесс, — она разбирается со
    /// следствием, а не пытается перехватить все причины.
    ///
    /// Идемпотентна: на чистом backend'е ничего не находит и молча выходит.
    ///
    /// Ошибка перечисления не считается фатальной — backend может не уметь
    /// перечислять инстансы. В этом случае мы просто теряем чистку, а не
    /// возможность работать.
    pub async fn unload_stale_instances(&self) -> Result<()> {
        let instances = match self.backend.loaded_instances().await {
            Ok(instances) => instances,
            Err(err) => {
                tracing::warn!("не удалось получить список загруженных инстансов: {err:#}");
                return Ok(());
            }
        };

        if instances.is_empty() {
            tracing::info!("стартовая чистка: посторонних инстансов нет");
            return Ok(());
        }

        tracing::info!(
            "стартовая чистка: найдено {} инстансов от прошлых запусков, выгружаю",
            instances.len()
        );
        for instance in &instances {
            match self.backend.unload_instance(instance).await {
                Ok(()) => tracing::info!("выгружен инстанс '{instance}'"),
                // Один упрямый инстанс не должен мешать старту: остальные
                // всё равно освободят память.
                Err(err) => tracing::warn!("не удалось выгрузить '{instance}': {err:#}"),
            }
        }

        // Внутренний учёт тоже обнуляем: он описывал состояние, которого
        // больше нет.
        self.loaded.lock().await.clear();
        Ok(())
    }

    pub async fn preload_always_loaded(&self) -> Result<()> {
        // Проверяем ещё до первого вызова backend.load(), что always_loaded
        // модели вообще помещаются в бюджет — иначе ошибка всплыла бы
        // только как сырой отказ от LM Studio при загрузке, а тут причина
        // видна сразу и явно.
        let required = self.resource_registry.always_loaded_vram_mb();
        if required > self.total_model_memory_mb {
            return Err(anyhow!(
                "always_loaded модели требуют {required} MB памяти, \
                 а бюджет всего {} MB — проверьте resource_registry.yaml",
                self.total_model_memory_mb
            ));
        }

        let mut loaded = self.loaded.lock().await;
        for model in self.resource_registry.always_loaded_models() {
            self.backend
                .load(self.resource_registry.provider_key(model))
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
            SchedulerResult::Ready(model_id) => {
                // Единственное место, где известно, какая модель в итоге
                // взяла задачу — значит и temperature подставлять здесь.
                let request = self.with_registry_temperature(&model_id, request);
                let provider_key = self.resource_registry.provider_key(&model_id);
                self.backend.generate(provider_key, request).await
            }
            SchedulerResult::Failed(reason) => Err(anyhow!(reason)),
        }
    }

    /// Выполняет запрос НАЗВАННОЙ моделью, минуя подбор по категории.
    ///
    /// Нужен для служебных шагов, у которых нет собственной категории и не
    /// должно быть: извлечение поискового термина для медиа-пайплайна — это
    /// не «задача пользователя», а деталь одной задачи. Заводить под неё
    /// строку в Capability Registry значило бы засорять реестр служебными
    /// сущностями.
    ///
    /// Размещение в памяти идёт по тем же правилам, что и обычный подбор
    /// (`fit_and_load` + LRU), поэтому модель не может занять VRAM в обход
    /// общего учёта.
    pub async fn run_with_model(
        &self,
        model_label: &str,
        request: GenerateRequest,
    ) -> Result<GenerateResponse> {
        let Ok(_guard) = self.gpu_lock.try_lock() else {
            return Err(anyhow!("GPU сейчас занят другой задачей, повторите позже"));
        };

        let Some(resource) = self.resource_registry.get(model_label) else {
            return Err(anyhow!(
                "модель '{model_label}' отсутствует в Resource Registry"
            ));
        };

        {
            let mut loaded = self.loaded.lock().await;
            match loaded.get_mut(model_label) {
                Some(usage) => usage.last_used = Utc::now(),
                None => {
                    if !self
                        .fit_and_load(model_label, resource.vram_mb, &mut loaded)
                        .await
                    {
                        return Err(anyhow!(
                            "не удалось разместить модель '{model_label}' в VRAM"
                        ));
                    }
                }
            }
        }

        let request = self.with_registry_temperature(model_label, request);
        let provider_key = self.resource_registry.provider_key(model_label);
        self.backend.generate(provider_key, request).await
    }

    /// Проставляет temperature из реестра, если вызывающий не задал свою.
    ///
    /// `None` в реестре означает «не задана» — поле останется `None` и
    /// не попадёт в JSON вовсе, сохраняя умолчание провайдера.
    fn with_registry_temperature(
        &self,
        model_label: &str,
        mut request: GenerateRequest,
    ) -> GenerateRequest {
        if request.temperature.is_none() {
            request.temperature = self.resource_registry.temperature_for(model_label);
        }
        request
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

    /// Пытается разместить модель `candidate` в бюджете памяти: сначала без
    /// вытеснения, затем вытесняя по LRU (никогда не трогая always_loaded),
    /// пока либо не найдётся места, либо вытеснять больше нечего.
    async fn fit_and_load(
        &self,
        candidate: &str,
        needed_memory_mb: u32,
        loaded: &mut HashMap<ModelId, ModelUsage>,
    ) -> bool {
        loop {
            let used_memory_mb: u32 = loaded
                .keys()
                .filter_map(|name| self.resource_registry.get(name))
                .map(|r| r.vram_mb)
                .sum();

            if fits_in_budget(used_memory_mb, needed_memory_mb, self.total_model_memory_mb) {
                return self.load_and_track(candidate, loaded).await;
            }

            match self.pick_eviction_victim(loaded) {
                Some(victim) => {
                    if self
                        .backend
                        .unload(self.resource_registry.provider_key(&victim))
                        .await
                        .is_err()
                    {
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
        if self
            .backend
            .load(self.resource_registry.provider_key(candidate))
            .await
            .is_err()
        {
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

#[cfg(test)]
mod budget_tests {
    //! Граница бюджета памяти.
    //!
    //! Числа здесь ПОДСТАВНЫЕ и намеренно не совпадают с реальным реестром:
    //! `vram_mb` моделей переизмеряется, и тест, привязанный к реальным
    //! значениям, ломался бы при каждом замере, ничего при этом не проверяя.
    //! Проверяется правило `used + needed <= budget` и то, что резидент
    //! занимает место безусловно, — а не конкретные мегабайты Gemma27.

    use super::*;
    use crate::model_backend::{GenerateRequest, GenerateResponse};
    use async_trait::async_trait;
    use std::path::PathBuf;

    /// Backend, который всегда соглашается. Проверяется решение Scheduler'а
    /// о размещении, а не поведение провайдера.
    struct AlwaysOkBackend;

    #[async_trait]
    impl ModelBackend for AlwaysOkBackend {
        async fn is_loaded(&self, _model: &str) -> Result<bool> {
            Ok(false)
        }
        async fn load(&self, _model: &str) -> Result<()> {
            Ok(())
        }
        async fn unload(&self, _model: &str) -> Result<()> {
            Ok(())
        }
        async fn generate(
            &self,
            _model: &str,
            _request: GenerateRequest,
        ) -> Result<GenerateResponse> {
            Ok(GenerateResponse {
                content: String::new(),
            })
        }
    }

    /// Резидент на 6000 и два кандидата: один влезает поверх него, другой нет.
    fn scheduler_with_budget(budget_mb: u32) -> (Scheduler, PathBuf) {
        // Счётчик, а не бюджет, в имени каталога: тесты идут параллельно, и
        // два вызова с одинаковым бюджетом делили бы один каталог — один
        // удалял бы файлы из-под другого.
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kaic-budget-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let resources = dir.join("resource_registry.yaml");
        std::fs::write(
            &resources,
            "Resident:
  vram_mb: 6000
  load_seconds: 1
  always_loaded: true

Fits:
  vram_mb: 18000
  load_seconds: 1

TooBig:
  vram_mb: 26000
  load_seconds: 1
",
        )
        .unwrap();

        let capabilities = dir.join("capability_registry.yaml");
        std::fs::write(
            &capabilities,
            "Heavy:
  primary: Fits

Heaviest:
  primary: TooBig
",
        )
        .unwrap();

        let scheduler = Scheduler::new(
            Arc::new(CapabilityRegistry::from_file(&capabilities).unwrap()),
            Arc::new(ResourceRegistry::from_file(&resources).unwrap()),
            Arc::new(AlwaysOkBackend),
            budget_mb,
        );
        (scheduler, dir)
    }

    /// Бюджет 24495 (реальное значение константы): 6000 резидента + 18000
    /// кандидата = 24000 <= 24495.
    #[tokio::test]
    async fn model_fitting_on_top_of_the_resident_is_selected() {
        let (scheduler, dir) = scheduler_with_budget(24495);
        scheduler.preload_always_loaded().await.unwrap();

        match scheduler.select_model("Heavy", false).await {
            SchedulerResult::Ready(model) => assert_eq!(model, "Fits"),
            SchedulerResult::Failed(why) => {
                panic!("кандидат, помещающийся поверх резидента, обязан быть выбран: {why}")
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 6000 + 26000 = 32000 > 24495. Резидент вытеснению не подлежит,
    /// поэтому освободить место не за счёт чего — это и есть вырожденный
    /// случай Qwen40B поверх Fable9B.
    #[tokio::test]
    async fn model_exceeding_the_budget_with_the_resident_is_rejected() {
        let (scheduler, dir) = scheduler_with_budget(24495);
        scheduler.preload_always_loaded().await.unwrap();

        if let SchedulerResult::Ready(model) = scheduler.select_model("Heaviest", false).await {
            panic!("кандидат '{model}' не помещается вместе с резидентом, но был выбран");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Диагностика обязана отвечать то же, что решит Scheduler. Числа
    /// подставные: реестр переизмеряется, и тест на реальных значениях
    /// ломался бы при каждом замере, ничего при этом не проверяя.
    #[tokio::test]
    async fn reachability_names_unreachable_models_and_empty_categories() {
        let (scheduler, dir) = scheduler_with_budget(24495);
        let report = scheduler.reachability();

        assert_eq!(report.budget_mb, 24495);
        assert_eq!(report.resident_mb, 6000, "резидент занимает бюджет постоянно");

        // 6000 резидента + 26000 = 32000 > 24495 — TooBig недостижим.
        // 6000 + 18000 = 24000 <= 24495 — Fits достижим.
        // Резидент меряется без себя самого: 0 + 6000 <= 24495.
        assert_eq!(
            report.unreachable,
            vec![("TooBig".to_string(), 26000)],
            "недостижим ровно тот, кто не влезает поверх резидента"
        );

        // Категория Heaviest состоит из одного TooBig — значит пуста.
        assert_eq!(report.empty_categories, vec!["Heaviest".to_string()]);
        // Heavy обслуживается своим primary, подмены нет.
        assert!(report.served_by_fallback.is_empty());

        // Тот же вопрос при бюджете размером с видеопамять: недостижимо всё,
        // кроме резидента, и обе категории пусты.
        drop(scheduler);
        let (tight, tight_dir) = scheduler_with_budget(8000);
        let tight_report = tight.reachability();
        let mut empty = tight_report.empty_categories.clone();
        empty.sort();
        assert_eq!(empty, vec!["Heaviest".to_string(), "Heavy".to_string()]);

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&tight_dir).ok();
    }

    /// Тот же кандидат при прежнем бюджете видеопамяти не проходил вообще —
    /// именно это и делало пять моделей из семи недостижимыми.
    #[tokio::test]
    async fn the_same_model_was_unreachable_under_the_old_vram_sized_budget() {
        let (scheduler, dir) = scheduler_with_budget(8000);
        scheduler.preload_always_loaded().await.unwrap();

        if let SchedulerResult::Ready(model) = scheduler.select_model("Heavy", false).await {
            panic!("при бюджете размером с видеопамять '{model}' не должен был проходить");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
