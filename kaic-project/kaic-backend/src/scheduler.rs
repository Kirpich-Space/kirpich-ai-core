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

/// Хвост строки выбора модели: почему не подошли предыдущие кандидаты.
/// Пусто, если подошёл первый же — тогда объяснять нечего.
fn describe_rejected(rejected: &[String]) -> String {
    if rejected.is_empty() {
        String::new()
    } else {
        format!("; пропущены — {}", rejected.join("; "))
    }
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
#[cfg_attr(test, derive(Debug))]
enum SchedulerResult {
    /// Модель выбрана и загружена, можно генерировать ответ.
    Ready(ModelId),
    /// Не удалось подобрать модель: нет кандидатов, не хватает VRAM
    /// у всех кандидатов даже после вытеснения, категория помечена
    /// `manual_only` без разрешения, или backend отказал.
    Failed(String),
}

#[cfg(test)]
impl SchedulerResult {
    fn is_ready(&self) -> bool {
        matches!(self, SchedulerResult::Ready(_))
    }
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

    /// ЛОК ОПЕРАЦИИ. Единственная блокировка на весь цикл `run()`: подбор
    /// модели (включая возможную загрузку/вытеснение) и сама генерация —
    /// всё это выполняется под ней, поэтому одновременно система
    /// обслуживает только один запрос. См. модульный комментарий.
    ///
    /// Он же обеспечивает инвариант «не более одной загрузки/выгрузки
    /// одновременно»: отдельный примитив для этого не заводился, потому что
    /// этот уже даёт нужную гарантию и делает её строже необходимого.
    gpu_lock: Mutex<()>,

    /// ЛОК СОСТОЯНИЯ. Модели, которые Scheduler считает загруженными прямо
    /// сейчас. Это единственный источник правды о состоянии VRAM — поэтому
    /// LM Studio JIT-loading и Auto-Evict должны быть отключены
    /// (см. model_backend.rs), иначе это состояние разойдётся с реальностью.
    ///
    /// НИКОГДА не удерживается через `await` на сетевом вызове. Раньше
    /// удерживался — и `GET /status` вставал на всё время загрузки модели
    /// (измерено: 7805 мс против 12 мс в покое). Берётся только на то, чтобы
    /// прочитать, вставить или удалить запись, и сразу отпускается.
    loaded: Mutex<HashMap<ModelId, ModelUsage>>,

    /// Что Scheduler делает прямо сейчас, если делает.
    ///
    /// Не лок, а ячейка состояния: она существует, чтобы промежуточное
    /// состояние было объяснимым. Без неё `/status` во время загрузки честно
    /// сказал бы «модель не загружена» — и это было бы правдой, которую
    /// невозможно отличить от «модели нет и не будет».
    ///
    /// `std::sync::Mutex`, а не `tokio` — держится микросекунды и через
    /// `await` не проносится никогда (см. правило порядка ниже).
    current_operation: std::sync::Mutex<Option<ActiveOperation>>,

    /// Сколько генераций идёт на каждой модели прямо сейчас.
    ///
    /// Появилась вместе с сужением `gpu_lock`. Пока генерация держала лок
    /// операции, вытеснить используемую модель было невозможно физически —
    /// второй задачи просто не существовало. Как только генерация вышла
    /// из-под лока, опасность стала реальной: задача Б выбирает жертвой
    /// модель, на которой Б в этот момент генерирует, и LM Studio выгружает
    /// её посреди запроса. Эта таблица — то, что делает такой выбор
    /// невозможным (см. `pick_eviction_victim`).
    busy: BusyTable,
}

/// Сколько задач система обслуживает одновременно.
///
/// Две, а не больше, и это потолок риска, а не производительности. Учёт
/// памяти параллелизм не моделирует: `vram_mb` — это footprint весов, а
/// контекст и KV-кэш растут сверх него у каждой идущей генерации, и растут
/// они в 8 ГБ видеопамяти. Две генерации — это ровно одна лишняя сверх
/// сегодняшнего поведения: риск ограничен, откат тривиален. Поднимать
/// потолок без модели памяти под контексты нельзя.
pub const MAX_CONCURRENT_TASKS: usize = 2;

/// Таблица «модель → сколько генераций на ней идёт прямо сейчас».
///
/// `std::sync::Mutex`, а не `tokio` — намеренно: её обязан уметь трогать
/// `Drop`, а `Drop` не бывает асинхронным. Держится микросекунды и через
/// `await` не проносится никогда.
type BusyTable = Arc<std::sync::Mutex<HashMap<ModelId, usize>>>;

/// Отметка «на этой модели идёт генерация», снимающая себя сама.
///
/// Существует ради одного свойства: счётчик уменьшается в `Drop`, то есть
/// и при обычном возврате, и при ошибке генерации, и при панике, и при
/// отмене future. Дисциплина «не забыть уменьшить» здесь не годится —
/// один незакрытый счётчик означает, что модель навсегда считается занятой
/// и её больше никогда не вытеснят.
pub struct GenerationGuard {
    model: ModelId,
    table: BusyTable,
}

/// Занятая под многошаговую работу модель.
///
/// Существует ради одного свойства: `GenerationGuard` внутри живёт столько
/// же, сколько сама сессия. Пока она жива, модель нельзя вытеснить — в том
/// числе в паузах между запросами, когда работает внешний инструмент и
/// никакой генерации не идёт. Отметка снимается в `Drop` guard'а, то есть и
/// при обычном возврате, и при ошибке, и при панике.
pub struct Session<'a> {
    scheduler: &'a Scheduler,
    model_id: ModelId,
    /// Не читается — важен только срок его жизни.
    _guard: GenerationGuard,
}

impl Session<'_> {
    /// Модель, выбранная под эту сессию.
    pub fn model(&self) -> &str {
        &self.model_id
    }

    /// Очередной запрос к той же самой модели.
    ///
    /// Подбора здесь нет и быть не должно: модель выбрана один раз при
    /// открытии сессии, и менять её посреди цикла значило бы отдать половину
    /// диалога одной модели, а половину другой.
    pub async fn generate(&self, request: GenerateRequest) -> Result<GenerateResponse> {
        self.scheduler
            .generate_under_guard(&self.model_id, request)
            .await
    }
}

impl Drop for GenerationGuard {
    fn drop(&mut self) {
        // Отравленный мьютекс тоже разбираем: иначе паника в одной задаче
        // навсегда пометила бы модель занятой.
        let mut table = match self.table.lock() {
            Ok(table) => table,
            Err(poisoned) => poisoned.into_inner(),
        };
        match table.get_mut(&self.model) {
            Some(count) if *count > 1 => *count -= 1,
            _ => {
                table.remove(&self.model);
            }
        }
    }
}

/// Операция, выполняемая Scheduler'ом прямо сейчас.
#[derive(Clone)]
pub struct ActiveOperation {
    /// Что делается: «загрузка» или «выгрузка». Строка, а не enum: она идёт
    /// прямиком в лог и в `/status`, а других потребителей у неё нет.
    pub kind: &'static str,
    /// Модель, над которой идёт операция.
    pub model: ModelId,
    /// Ради чего: категория задачи или ярлык модели при прямом вызове.
    pub reason: String,
    pub started_at: DateTime<Utc>,
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
            current_operation: std::sync::Mutex::new(None),
            busy: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    // --- ПРАВИЛО ПОРЯДКА ВЗЯТИЯ БЛОКИРОВОК ---
    //
    // Порядок строго такой, сверху вниз, и нарушать его нельзя:
    //
    //     1. gpu_lock            (лок операции: ТОЛЬКО загрузка/выгрузка)
    //     2. loaded              (лок состояния, держится микросекунды)
    //     3. busy                (счётчики генераций, держится микросекунды)
    //     4. current_operation   (ячейка маркера, держится микросекунды)
    //
    // Дедлок возникает ровно при взятии в разном порядке двумя путями, и
    // единственная защита от него — записанное и соблюдаемое правило.
    //
    // Дополнительно, и это важнее порядка:
    //   * `loaded` НИКОГДА не удерживается через `await`;
    //   * `current_operation` не берётся, пока удерживается `loaded`;
    //   * `snapshot()` и `active_operation()` берут только свой лок (2 и 4
    //     соответственно) и не берут `gpu_lock` — потому и отвечают
    //     мгновенно, пока идёт долгая операция;
    //   * `gpu_lock` НЕ удерживается через генерацию — он отпускается сразу
    //     после того, как набор загруженных моделей перестал меняться.
    //
    // Почему `busy` НЕ берётся под `gpu_lock`. Счётчик занятости живёт
    // дольше лока операции: он ставится, пока `gpu_lock` ещё удерживается
    // (чтобы между загрузкой и началом генерации никто не успел вытеснить
    // только что загруженное), и снимается через минуты после того, как
    // `gpu_lock` отпущен. Требовать `gpu_lock` для его снятия значило бы
    // ждать чужую загрузку ради уменьшения числа на единицу — и делать это
    // из `Drop`, который ждать не умеет. Поэтому `busy` — независимый
    // короткоживущий `std::sync::Mutex` ниже по порядку, а не часть
    // операции.
    //
    // `pick_eviction_victim` вызывается под `loaded` и берёт `busy` —
    // порядок 2 → 3 соблюдён.

    /// Отмечает начало операции. Вызывается ТОЛЬКО вне блокировки `loaded`.
    fn begin_operation(&self, kind: &'static str, model: &str, reason: &str) {
        if let Ok(mut slot) = self.current_operation.lock() {
            *slot = Some(ActiveOperation {
                kind,
                model: model.to_string(),
                reason: reason.to_string(),
                started_at: Utc::now(),
            });
        }
    }

    /// Снимает отметку операции. Вызывается в том числе на путях отказа —
    /// иначе `/status` навсегда застрял бы на несостоявшейся загрузке.
    fn end_operation(&self) {
        if let Ok(mut slot) = self.current_operation.lock() {
            *slot = None;
        }
    }

    /// Что выполняется прямо сейчас, если выполняется.
    /// Не берёт ни `gpu_lock`, ни `loaded` — отвечает всегда.
    pub fn active_operation(&self) -> Option<ActiveOperation> {
        self.current_operation.lock().ok().and_then(|s| s.clone())
    }

    fn busy_table(&self) -> std::sync::MutexGuard<'_, HashMap<ModelId, usize>> {
        match self.busy.lock() {
            Ok(table) => table,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Сколько генераций идёт прямо сейчас, всего по всем моделям.
    pub fn running_generations(&self) -> usize {
        self.busy_table().values().sum()
    }

    /// Модели, на которых прямо сейчас идёт генерация, и сколько на каждой.
    pub fn busy_models(&self) -> Vec<(ModelId, usize)> {
        let mut models: Vec<(ModelId, usize)> = self
            .busy_table()
            .iter()
            .map(|(name, count)| (name.clone(), *count))
            .collect();
        models.sort_by(|a, b| a.0.cmp(&b.0));
        models
    }

    /// Отмечает начало генерации на модели и возвращает guard, который
    /// снимет отметку сам — в том числе если генерация упадёт.
    ///
    /// `None` означает, что предел параллелизма исчерпан. Проверка предела
    /// и инкремент делаются под одним взятием лока: иначе две задачи могли
    /// бы одновременно увидеть «свободно» и обе пройти.
    fn try_begin_generation(&self, model: &str) -> Option<GenerationGuard> {
        let mut table = self.busy_table();
        let running: usize = table.values().sum();
        if running >= MAX_CONCURRENT_TASKS {
            return None;
        }
        *table.entry(model.to_string()).or_insert(0) += 1;
        drop(table);
        Some(GenerationGuard {
            model: model.to_string(),
            table: self.busy.clone(),
        })
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

        // Лок операции берётся и здесь, хотя на старте конкурента нет:
        // инвариант «не более одной загрузки одновременно» должен держаться
        // одним механизмом на всех путях, а не «на старте и так никого».
        let _guard = self.gpu_lock.lock().await;

        for model in self.resource_registry.always_loaded_models() {
            self.begin_operation("загрузка", model, "always_loaded");
            let outcome = self
                .backend
                .load(self.resource_registry.provider_key(model))
                .await;
            self.end_operation();
            outcome
                .map_err(|e| anyhow!("не удалось загрузить always_loaded модель {model}: {e}"))?;

            // Короткая блокировка состояния — уже после загрузки.
            self.loaded.lock().await.insert(
                model.to_string(),
                ModelUsage {
                    last_used: Utc::now(),
                },
            );
        }
        Ok(())
    }

    /// Занимает модель под многошаговую работу и держит её занятой, пока жив
    /// возвращённый `Session`.
    ///
    /// Нужен для цикла вызовов инструментов. `run` подбирает модель, делает
    /// ОДИН запрос и отпускает `GenerationGuard` — для цикла это неверно:
    /// между «модель попросила вызвать инструмент» и «мы вернули ей
    /// результат» проходит время работы инструмента, и в это окно LRU вправе
    /// выгрузить модель. Следующий шаг цикла пришёл бы к выгруженной модели.
    ///
    /// Что при этом НЕ меняется: смысл `MAX_CONCURRENT_TASKS`. Он и раньше
    /// считал задачи, занимающие модели, и продолжает считать их же — одна
    /// задача занимает один слот. Изменилась только ДЛИТЕЛЬНОСТЬ удержания:
    /// слот держится весь цикл, а не один запрос. Это и есть требуемое
    /// поведение, а не побочный эффект.
    pub async fn begin_session(&self, category: &str, allow_manual: bool) -> Result<Session<'_>> {
        let (model_id, guard) = self.acquire(category, allow_manual).await?;
        Ok(Session {
            scheduler: self,
            model_id,
            _guard: guard,
        })
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
        let (model_id, guard) = self.acquire(category, allow_manual).await?;
        self.generate(&model_id, request, guard).await
    }

    /// Подбор модели и отметка занятости — общая часть `run` и
    /// `begin_session`. Вынесена целиком, без изменений в правилах: разница
    /// между ними только в том, как долго живёт `GenerationGuard`.
    async fn acquire(
        &self,
        category: &str,
        allow_manual: bool,
    ) -> Result<(ModelId, GenerationGuard)> {
        let running = self.running_generations();

        if running >= MAX_CONCURRENT_TASKS {
            tracing::warn!(
                "категория '{category}': отказ — уже выполняется {running} задач(и), \
                 предел {MAX_CONCURRENT_TASKS}"
            );
            return Err(anyhow!(
                "система уже обслуживает {running} задач(и) — предел {MAX_CONCURRENT_TASKS}, \
                 повторите позже"
            ));
        }

        // --- ВТОРАЯ ЗАДАЧА ---
        // Допускается только если ничего менять не надо: модель уже
        // загружена, значит ни загрузки, ни выгрузки, ни вытеснения. Любая
        // задача, меняющая набор моделей, идёт исключительным путём ниже.
        //
        // Условие «уже что-то выполняется» существенно: пока система
        // свободна, подбор идёт как раньше, вплоть до загрузки primary.
        // Иначе правила выбора модели изменились бы — а их менять нельзя.
        if running > 0 {
            let Some(model_id) = self.already_loaded_candidate(category, allow_manual).await else {
                tracing::warn!(
                    "категория '{category}': отказ — система занята другой задачей, \
                     а подходящая модель не загружена (загрузка потребовала бы \
                     изменить набор моделей под работающей задачей)"
                );
                return Err(anyhow!(
                    "система занята другой задачей, а подходящая для '{category}' модель \
                     не загружена — повторите позже"
                ));
            };

            let Some(guard) = self.try_begin_generation(&model_id) else {
                return Err(anyhow!(
                    "предел одновременных задач ({MAX_CONCURRENT_TASKS}) исчерпан"
                ));
            };
            tracing::info!(
                "категория '{category}': выбрана '{model_id}' (уже загружена, \
                 параллельно с {running} выполняющейся)"
            );
            return Ok((model_id, guard));
        }

        // --- ИСКЛЮЧИТЕЛЬНЫЙ ПУТЬ ---
        // Набор моделей может измениться, поэтому берётся лок операции.
        // Отпускается он сразу после подбора: генерация под ним больше не
        // идёт, ради этого задача и делалась. Отметка занятости ставится
        // ДО отпускания лока — иначе между загрузкой и началом генерации
        // осталось бы окно, в котором модель можно было бы вытеснить.
        let (model_id, guard) = {
            let Ok(operation) = self.gpu_lock.try_lock() else {
                return Err(anyhow!("GPU сейчас занят другой задачей, повторите позже"));
            };

            let model_id = match self.select_model(category, allow_manual).await {
                SchedulerResult::Ready(model_id) => model_id,
                SchedulerResult::Failed(reason) => return Err(anyhow!(reason)),
            };
            let Some(guard) = self.try_begin_generation(&model_id) else {
                return Err(anyhow!(
                    "предел одновременных задач ({MAX_CONCURRENT_TASKS}) исчерпан"
                ));
            };
            drop(operation);
            (model_id, guard)
        };

        Ok((model_id, guard))
    }

    /// Первый кандидат категории, который уже загружен. `None` — значит для
    /// категории пришлось бы что-то грузить.
    ///
    /// Отмечает найденную модель как только что использованную: иначе LRU
    /// считал бы её давно не нужной ровно в тот момент, когда она работает.
    async fn already_loaded_candidate(&self, category: &str, allow_manual: bool) -> Option<ModelId> {
        if self.capability_registry.is_manual_only(category) && !allow_manual {
            return None;
        }
        let candidates = self.capability_registry.candidates(category);
        let mut loaded = self.loaded.lock().await;
        for candidate in candidates {
            if let Some(usage) = loaded.get_mut(candidate) {
                usage.last_used = Utc::now();
                return Some(candidate.to_string());
            }
        }
        None
    }

    /// Собственно генерация. Идёт БЕЗ `gpu_lock`: набор моделей она не
    /// меняет, а держать лок операции все её минуты — это и была причина,
    /// по которой резидентная модель простаивала.
    ///
    /// `guard` передан по значению и живёт до конца вызова: пока он жив,
    /// модель нельзя вытеснить.
    async fn generate(
        &self,
        model_id: &str,
        request: GenerateRequest,
        guard: GenerationGuard,
    ) -> Result<GenerateResponse> {
        // Единственное место, где известно, какая модель в итоге взяла
        // задачу — значит и temperature подставлять здесь.
        let outcome = self.generate_under_guard(model_id, request).await;
        drop(guard); // явно, чтобы срок жизни отметки читался, а не угадывался
        outcome
    }

    /// Один запрос к модели без всякой работы с отметкой занятости.
    ///
    /// Отметку держит вызывающий: `generate` — на один вызов, `Session` — на
    /// весь цикл. Разделение существует только ради этого различия.
    async fn generate_under_guard(
        &self,
        model_id: &str,
        request: GenerateRequest,
    ) -> Result<GenerateResponse> {
        // Единственное место, где известно, какая модель в итоге взяла
        // задачу — значит и temperature подставлять здесь.
        let request = self.with_registry_temperature(model_id, request);
        let provider_key = self.resource_registry.provider_key(model_id);
        self.backend.generate(provider_key, request).await
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
        let Some(resource) = self.resource_registry.get(model_label) else {
            return Err(anyhow!(
                "модель '{model_label}' отсутствует в Resource Registry"
            ));
        };

        // Правила допуска те же, что у run(): служебный вызов — такая же
        // генерация и так же обязан отмечаться занятым, иначе его модель
        // вытеснят из-под него.
        let running = self.running_generations();
        if running >= MAX_CONCURRENT_TASKS {
            return Err(anyhow!(
                "система уже обслуживает {running} задач(и) — предел {MAX_CONCURRENT_TASKS}, \
                 повторите позже"
            ));
        }

        // Короткая блокировка состояния, затем — размещение уже без неё.
        let already_loaded = {
            let mut loaded = self.loaded.lock().await;
            match loaded.get_mut(model_label) {
                Some(usage) => {
                    usage.last_used = Utc::now();
                    true
                }
                None => false,
            }
        };

        let guard = if already_loaded {
            self.try_begin_generation(model_label)
                .ok_or_else(|| anyhow!("предел одновременных задач ({MAX_CONCURRENT_TASKS})"))?
        } else {
            // Размещение меняет набор моделей — исключительный путь, и он
            // недопустим, пока работает другая задача.
            if running > 0 {
                return Err(anyhow!(
                    "система занята другой задачей, а модель '{model_label}' не загружена \
                     — повторите позже"
                ));
            }
            let Ok(operation) = self.gpu_lock.try_lock() else {
                return Err(anyhow!("GPU сейчас занят другой задачей, повторите позже"));
            };
            if !self
                .fit_and_load(model_label, resource.vram_mb, model_label)
                .await
            {
                return Err(anyhow!(
                    "не удалось разместить модель '{model_label}' в VRAM"
                ));
            }
            let guard = self
                .try_begin_generation(model_label)
                .ok_or_else(|| anyhow!("предел одновременных задач ({MAX_CONCURRENT_TASKS})"))?;
            drop(operation);
            guard
        };

        self.generate(model_label, request, guard).await
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

        // Почему предыдущие кандидаты не подошли — накапливаем, чтобы
        // сказать это одной строкой, а не заставлять читать лог по кускам.
        let mut rejected: Vec<String> = Vec::new();

        for (position, candidate) in candidates.iter().enumerate() {
            let role = if position == 0 { "primary" } else { "fallback" };

            let Some(resource) = self.resource_registry.get(candidate) else {
                tracing::warn!(
                    "модель '{candidate}' есть в Capability Registry, но отсутствует \
                     в Resource Registry — пропускаю"
                );
                rejected.push(format!("{candidate}: нет в Resource Registry"));
                continue;
            };

            // Короткая блокировка состояния: только проверить и отметить.
            let already_loaded = {
                let mut loaded = self.loaded.lock().await;
                match loaded.get_mut(*candidate) {
                    Some(usage) => {
                        usage.last_used = Utc::now();
                        true
                    }
                    None => false,
                }
            };
            if already_loaded {
                tracing::info!(
                    "категория '{category}': выбрана '{candidate}' ({role}, уже загружена){}",
                    describe_rejected(&rejected)
                );
                return SchedulerResult::Ready(candidate.to_string());
            }

            if self
                .fit_and_load(candidate, resource.vram_mb, category)
                .await
            {
                tracing::info!(
                    "категория '{category}': выбрана '{candidate}' ({role}, загружена сейчас){}",
                    describe_rejected(&rejected)
                );
                return SchedulerResult::Ready(candidate.to_string());
            }
            // Кандидат не поместился даже после вытеснения — пробуем следующий
            // по fallback-цепочке, не прерываем весь подбор.
            rejected.push(format!("{candidate}: не помещается ({} MB)", resource.vram_mb));
        }

        tracing::warn!(
            "категория '{category}': кандидат не подобран, перебрано {} — {}",
            candidates.len(),
            rejected.join("; ")
        );

        SchedulerResult::Failed(format!(
            "не удалось подобрать модель для категории '{category}': \
             не хватает VRAM у всех кандидатов или backend недоступен"
        ))
    }

    /// Пытается разместить модель `candidate` в бюджете памяти: сначала без
    /// вытеснения, затем вытесняя по LRU (никогда не трогая always_loaded),
    /// пока либо не найдётся места, либо вытеснять больше нечего.
    ///
    /// Лок состояния берётся здесь только на решение (посчитать занятое,
    /// выбрать жертву) и на запись результата. Сетевые вызовы `unload`/`load`
    /// идут БЕЗ него — ради этого функция и разбита на «решить» и «сделать».
    /// Гонки при этом нет: конкурента не существует, `gpu_lock` пропускает
    /// сюда только один запрос за раз.
    async fn fit_and_load(&self, candidate: &str, needed_memory_mb: u32, reason: &str) -> bool {
        loop {
            // --- решение: короткая блокировка состояния ---
            let step = {
                let loaded = self.loaded.lock().await;
                let used_memory_mb: u32 = loaded
                    .keys()
                    .filter_map(|name| self.resource_registry.get(name))
                    .map(|r| r.vram_mb)
                    .sum();

                if fits_in_budget(used_memory_mb, needed_memory_mb, self.total_model_memory_mb) {
                    Step::Load
                } else {
                    match self.pick_eviction_victim(&loaded) {
                        Some(victim) => Step::Evict(victim, used_memory_mb),
                        None => Step::GiveUp(used_memory_mb),
                    }
                }
            }; // лок состояния отпущен здесь — до всякого await

            match step {
                Step::Load => return self.load_and_track(candidate, reason).await,

                Step::Evict(victim, used_memory_mb) => {
                    let freed = self
                        .resource_registry
                        .get(&victim)
                        .map(|r| r.vram_mb)
                        .unwrap_or(0);

                    self.begin_operation("выгрузка", &victim, reason);
                    let outcome = self
                        .backend
                        .unload(self.resource_registry.provider_key(&victim))
                        .await;
                    self.end_operation();

                    if outcome.is_err() {
                        tracing::warn!("не удалось выгрузить модель '{victim}' для освобождения VRAM");
                        return false;
                    }
                    self.loaded.lock().await.remove(&victim);
                    tracing::info!(
                        "вытеснена '{victim}' ради '{candidate}': освобождено {freed} MB, \
                         требуется {needed_memory_mb} MB, было занято {used_memory_mb} MB \
                         из {} MB бюджета",
                        self.total_model_memory_mb
                    );
                }

                Step::GiveUp(used_memory_mb) => {
                    // Вытеснять нечего: остались только резиденты и модели,
                    // на которых прямо сейчас идёт генерация. Занятую
                    // выгружать нельзя — это выдернуло бы модель из-под
                    // работающего запроса.
                    let busy: Vec<String> = self
                        .busy_models()
                        .into_iter()
                        .map(|(model, count)| format!("{model}×{count}"))
                        .collect();
                    let busy_note = if busy.is_empty() {
                        "занятых генерацией нет".to_string()
                    } else {
                        format!("заняты генерацией: {}", busy.join(", "))
                    };
                    tracing::warn!(
                        "'{candidate}' не размещена: требуется {needed_memory_mb} MB, \
                         занято {used_memory_mb} MB из {} MB бюджета; вытеснять некого \
                         (кандидаты либо always_loaded, либо заняты) — {busy_note}",
                        self.total_model_memory_mb
                    );
                    return false;
                }
            }
        }
    }

    /// Выбирает модель для вытеснения: не always_loaded, НЕ ЗАНЯТАЯ
    /// генерацией, дольше всего не использовавшаяся из загруженных сейчас.
    ///
    /// Исключение занятых — не оптимизация, а условие корректности. Пока
    /// генерация держала `gpu_lock`, второй задачи не существовало и
    /// вытеснить используемую модель было нельзя физически. После сужения
    /// лока единственное, что стоит между задачей Б и выгрузкой модели
    /// из-под работающего запроса задачи А, — эта проверка.
    fn pick_eviction_victim(&self, loaded: &HashMap<ModelId, ModelUsage>) -> Option<ModelId> {
        let busy = self.busy_table();
        loaded
            .iter()
            .filter(|(name, _)| {
                !self
                    .resource_registry
                    .get(name.as_str())
                    .map(|r| r.always_loaded)
                    .unwrap_or(false)
            })
            .filter(|(name, _)| busy.get(name.as_str()).copied().unwrap_or(0) == 0)
            .min_by_key(|(_, usage)| usage.last_used)
            .map(|(name, _)| name.clone())
    }

    /// Загружает модель и вносит её в учёт.
    ///
    /// Лок состояния берётся только на вставку — после того как загрузка
    /// уже завершилась. На время самой загрузки состояние открыто для чтения,
    /// а происходящее описано маркером операции.
    async fn load_and_track(&self, candidate: &str, reason: &str) -> bool {
        let needed = self
            .resource_registry
            .get(candidate)
            .map(|r| r.vram_mb)
            .unwrap_or(0);
        tracing::info!("загрузка '{candidate}' начата ({needed} MB по реестру)");
        let started = std::time::Instant::now();

        self.begin_operation("загрузка", candidate, reason);
        let outcome = self
            .backend
            .load(self.resource_registry.provider_key(candidate))
            .await;
        self.end_operation();

        if outcome.is_err() {
            tracing::warn!(
                "не удалось загрузить модель '{candidate}' (после {:.1} с)",
                started.elapsed().as_secs_f64()
            );
            return false;
        }
        tracing::info!(
            "загрузка '{candidate}' завершена за {:.1} с",
            started.elapsed().as_secs_f64()
        );
        self.loaded.lock().await.insert(
            candidate.to_string(),
            ModelUsage {
                last_used: Utc::now(),
            },
        );
        true
    }
}

/// Следующий шаг размещения, выбранный под локом состояния и выполняемый
/// уже без него. Существует ровно затем, чтобы решение и действие можно
/// было разнести по разные стороны блокировки.
enum Step {
    /// Место есть — грузим.
    Load,
    /// Места нет, но есть кого вытеснить: (жертва, сколько было занято).
    Evict(ModelId, u32),
    /// Места нет и вытеснять нечего: (сколько было занято).
    GiveUp(u32),
}

#[cfg(test)]
mod concurrency_tests {
    //! Разделение лока состояния и лока операции.
    //!
    //! На реальных моделях это непроверяемо: загрузка идёт минуты и зависит
    //! от машины. Поэтому backend подставной, а его загрузка — это `sleep`
    //! известной длительности.

    use super::budget_tests::scheduler_with_sleepy_backend;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;

    /// Снимок состояния обязан возвращаться, пока идёт загрузка, — раньше
    /// он вставал на всё её время (измерено: 7805 мс против 12 мс в покое).
    #[tokio::test]
    async fn snapshot_answers_while_a_model_is_loading() {
        let (scheduler, dir, _) = scheduler_with_sleepy_backend(24495, 1500);
        let scheduler = StdArc::new(scheduler);

        let worker = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.select_model("Heavy", false).await })
        };

        // Дать загрузке начаться, но не завершиться.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let started = std::time::Instant::now();
        let snapshot = scheduler.snapshot().await;
        let operation = scheduler.active_operation();
        let waited = started.elapsed();

        assert!(
            waited < std::time::Duration::from_millis(300),
            "снимок ждал {waited:?} — значит лок состояния снова держится через загрузку"
        );
        // Модели ещё нет в учёте — она появится только после загрузки.
        assert!(!snapshot.iter().any(|(name, _)| name == "Fits"));
        // ...но промежуточное состояние объяснено маркером.
        let operation = operation.expect("во время загрузки операция обязана быть видна");
        assert_eq!(operation.kind, "загрузка");
        assert_eq!(operation.model, "Fits");
        assert_eq!(operation.reason, "Heavy");

        worker.await.expect("подбор завершился");
        assert!(
            scheduler.active_operation().is_none(),
            "после завершения маркер обязан сниматься"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Инвариант: одновременно выполняется не более одной загрузки/выгрузки.
    /// Обеспечивает его gpu_lock — тот самый, что был до правки; разделение
    /// локов не должно было его ослабить.
    #[tokio::test]
    async fn at_most_one_load_runs_at_a_time() {
        let (scheduler, dir, concurrent_peak) = scheduler_with_sleepy_backend(24495, 400);
        let scheduler = StdArc::new(scheduler);

        let mut running = Vec::new();
        for _ in 0..4 {
            let scheduler = scheduler.clone();
            running.push(tokio::spawn(async move {
                scheduler
                    .run(
                        "Heavy",
                        false,
                        GenerateRequest {
                            messages: Vec::new(),
                            temperature: None,
                            tools: Vec::new(),
                        },
                    )
                    .await
                    .is_ok()
            }));
        }

        let mut succeeded = 0;
        for handle in running {
            if handle.await.expect("задача не паниковала") {
                succeeded += 1;
            }
        }

        assert_eq!(
            concurrent_peak.load(Ordering::SeqCst),
            1,
            "backend увидел одновременные загрузки — инвариант нарушен"
        );
        // gpu_lock берётся через try_lock: проигравшие получают отказ, а не
        // очередь. Это прежнее поведение, и менять его задача не разрешала.
        assert!(succeeded >= 1, "хотя бы один запрос обязан пройти");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Маркер операции обязан сниматься и тогда, когда загрузка провалилась.
    /// Иначе `/status` навсегда показывал бы «идёт загрузка», а панель —
    /// вечный прогресс вместо отказа.
    #[tokio::test]
    async fn operation_marker_is_cleared_when_a_load_fails() {
        let (scheduler, dir) = super::budget_tests::scheduler_with_failing_backend(24495);

        assert!(scheduler.active_operation().is_none(), "в покое маркера нет");

        let outcome = scheduler.select_model("Heavy", false).await;
        assert!(
            matches!(outcome, SchedulerResult::Failed(_)),
            "backend отказал — подбор обязан провалиться"
        );
        assert!(
            scheduler.active_operation().is_none(),
            "после отказа маркер обязан быть снят"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Параллелизм задач и защита занятых моделей ---

    fn empty_request() -> GenerateRequest {
        GenerateRequest {
            messages: Vec::new(),
            temperature: None,
            tools: Vec::new(),
        }
    }

    /// Две задачи на УЖЕ ЗАГРУЖЕННЫХ моделях идут одновременно. Ради этого
    /// лок и сужался: резидент должен работать, пока на другой модели
    /// тянется долгая генерация.
    #[tokio::test]
    async fn two_tasks_on_loaded_models_run_at_the_same_time() {
        let (scheduler, dir, peak) = super::budget_tests::scheduler_with_slow_generation(24495, 600);
        let scheduler = StdArc::new(scheduler);
        // Обе модели уже загружены — набор менять не придётся.
        scheduler.preload_always_loaded().await.unwrap();
        assert!(scheduler.select_model("Heavy", false).await.is_ready());

        let long = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.run("Heavy", false, empty_request()).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let short = scheduler.run("Resident", false, empty_request()).await;

        assert!(
            short.is_ok(),
            "вторая задача обязана пройти, но: {}",
            short.err().map(|e| e.to_string()).unwrap_or_default()
        );
        assert!(long.await.unwrap().is_ok());
        assert_eq!(
            peak.load(Ordering::SeqCst),
            2,
            "генерации не пересеклись — параллелизма не получилось"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Третья задача при двух выполняющихся получает отказ, а не очередь.
    #[tokio::test]
    async fn a_third_task_is_refused_while_two_are_running() {
        let (scheduler, dir, _) = super::budget_tests::scheduler_with_slow_generation(24495, 600);
        let scheduler = StdArc::new(scheduler);
        scheduler.preload_always_loaded().await.unwrap();
        assert!(scheduler.select_model("Heavy", false).await.is_ready());

        let mut running = Vec::new();
        for category in ["Heavy", "Resident"] {
            let scheduler = scheduler.clone();
            running.push(tokio::spawn(async move {
                scheduler.run(category, false, empty_request()).await
            }));
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let third = scheduler.run("Resident", false, empty_request()).await;
        let message = third
            .err()
            .expect("третья задача обязана получить отказ")
            .to_string();
        assert!(
            message.contains("предел") || message.contains("2"),
            "отказ обязан называть причину: {message}"
        );

        for handle in running {
            handle.await.unwrap().expect("первые две обязаны пройти");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Задача, требующая загрузки, параллельно не идёт: она меняет набор
    /// моделей, а менять его под работающей задачей нельзя.
    #[tokio::test]
    async fn a_task_needing_a_load_does_not_run_in_parallel() {
        let (scheduler, dir, _) = super::budget_tests::scheduler_with_slow_generation(24495, 600);
        let scheduler = StdArc::new(scheduler);
        scheduler.preload_always_loaded().await.unwrap();

        // Резидент загружен, 'Fits' — нет.
        let long = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.run("Resident", false, empty_request()).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let needs_load = scheduler.run("Heavy", false, empty_request()).await;
        let message = needs_load
            .err()
            .expect("задача с загрузкой не должна идти параллельно")
            .to_string();
        assert!(
            message.contains("не загружена"),
            "отказ обязан объяснять, что дело в загрузке: {message}"
        );

        long.await.unwrap().expect("первая задача обязана пройти");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Вытеснение не выбирает модель, на которой идёт генерация, — и, если
    /// свободных кандидатов нет, отказывает вместо выгрузки занятой.
    #[tokio::test]
    async fn a_model_with_a_running_generation_is_never_evicted() {
        let (scheduler, dir, _) = super::budget_tests::scheduler_with_slow_generation(24495, 800);
        let scheduler = StdArc::new(scheduler);
        scheduler.preload_always_loaded().await.unwrap();
        assert!(scheduler.select_model("Heavy", false).await.is_ready());

        // 'Fits' (18000) занята генерацией; 'TooBig' (26000) попросит место,
        // и единственный кандидат на вытеснение — как раз занятая 'Fits'.
        let long = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.run("Heavy", false, empty_request()).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Пока идёт генерация, занятая модель обязана быть вне выбора жертв.
        {
            let loaded = scheduler.loaded.lock().await;
            assert_eq!(
                scheduler.pick_eviction_victim(&loaded),
                None,
                "единственный не-резидент занят генерацией — вытеснять некого"
            );
        }
        assert_eq!(scheduler.busy_models(), vec![("Fits".to_string(), 1)]);

        long.await.unwrap().expect("генерация обязана дойти до конца");

        // Освободилась — снова кандидат.
        let loaded = scheduler.loaded.lock().await;
        assert_eq!(scheduler.pick_eviction_victim(&loaded), Some("Fits".to_string()));
        drop(loaded);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Все кандидаты на вытеснение заняты — задача получает отказ, а не
    /// выгрузку занятой модели.
    #[tokio::test]
    async fn placement_is_refused_when_every_victim_is_busy() {
        let (scheduler, dir, _) = super::budget_tests::scheduler_with_slow_generation(24495, 800);
        let scheduler = StdArc::new(scheduler);
        scheduler.preload_always_loaded().await.unwrap();
        assert!(scheduler.select_model("Heavy", false).await.is_ready());

        let long = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.run("Heavy", false, empty_request()).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // 'Heaviest' требует вытеснить 'Fits', но та занята.
        let refused = scheduler.run("Heaviest", false, empty_request()).await;
        assert!(
            refused.is_err(),
            "размещение обязано быть отвергнуто, а не выполнено за счёт занятой модели"
        );
        // Занятая модель на месте — её не выгрузили.
        assert!(scheduler
            .snapshot()
            .await
            .iter()
            .any(|(name, _)| name == "Fits"));

        long.await.unwrap().expect("генерация не должна была пострадать");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Счётчик занятости снимается и при ОШИБКЕ генерации. Иначе модель
    /// навсегда осталась бы «занятой» и её больше никто не вытеснил бы —
    /// тот же класс дефекта, что не снятый маркер операции.
    #[tokio::test]
    async fn the_busy_counter_is_released_when_generation_fails() {
        let (scheduler, dir) = super::budget_tests::scheduler_with_failing_generation(24495);
        scheduler.preload_always_loaded().await.unwrap();

        assert_eq!(scheduler.running_generations(), 0, "в покое занятых нет");

        let outcome = scheduler.run("Resident", false, empty_request()).await;
        assert!(outcome.is_err(), "backend обязан был отказать");

        assert_eq!(
            scheduler.running_generations(),
            0,
            "после неудачной генерации счётчик обязан быть снят"
        );
        assert!(scheduler.busy_models().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Счётчик одновременных загрузок для проверки инварианта.
    pub struct ConcurrencyWatch {
        pub now: AtomicUsize,
        pub peak: StdArc<AtomicUsize>,
    }

    impl ConcurrencyWatch {
        pub fn enter(&self) {
            let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
        }
        pub fn leave(&self) {
            self.now.fetch_sub(1, Ordering::SeqCst);
        }
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
            Ok(GenerateResponse::Text(String::new()))
        }
    }

    /// Backend, чья загрузка занимает заданное время и считает, сколько
    /// загрузок идёт одновременно. На реальных моделях проверить и то и
    /// другое невозможно: загрузка занимает минуты и зависит от машины.
    pub struct SleepyBackend {
        pub load_ms: u64,
        pub watch: super::concurrency_tests::ConcurrencyWatch,
    }

    #[async_trait]
    impl ModelBackend for SleepyBackend {
        async fn is_loaded(&self, _model: &str) -> Result<bool> {
            Ok(false)
        }
        async fn load(&self, _model: &str) -> Result<()> {
            self.watch.enter();
            tokio::time::sleep(std::time::Duration::from_millis(self.load_ms)).await;
            self.watch.leave();
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
            Ok(GenerateResponse::Text(String::new()))
        }
    }

    /// Тот же реестр, что и у `scheduler_with_budget`, но с медленным
    /// backend'ом. Третьим элементом — пик одновременных загрузок.
    pub fn scheduler_with_sleepy_backend(
        budget_mb: u32,
        load_ms: u64,
    ) -> (Scheduler, PathBuf, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let peak = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let backend = std::sync::Arc::new(SleepyBackend {
            load_ms,
            watch: super::concurrency_tests::ConcurrencyWatch {
                now: std::sync::atomic::AtomicUsize::new(0),
                peak: peak.clone(),
            },
        });
        let (scheduler, dir) = scheduler_with_backend(budget_mb, backend);
        (scheduler, dir, peak)
    }

    /// Backend, у которого загрузка всегда падает.
    struct FailingBackend;

    #[async_trait]
    impl ModelBackend for FailingBackend {
        async fn is_loaded(&self, _model: &str) -> Result<bool> {
            Ok(false)
        }
        async fn load(&self, _model: &str) -> Result<()> {
            Err(anyhow!("подставной отказ загрузки"))
        }
        async fn unload(&self, _model: &str) -> Result<()> {
            Ok(())
        }
        async fn generate(
            &self,
            _model: &str,
            _request: GenerateRequest,
        ) -> Result<GenerateResponse> {
            Err(anyhow!("подставной отказ генерации"))
        }
    }

    pub fn scheduler_with_failing_backend(budget_mb: u32) -> (Scheduler, PathBuf) {
        scheduler_with_backend(budget_mb, Arc::new(FailingBackend))
    }

    /// Backend, который грузит мгновенно, а генерирует медленно, считая пик
    /// одновременных генераций. Именно этот пик и есть предмет проверки:
    /// на реальных моделях его не измерить.
    struct SlowGenerationBackend {
        generate_ms: u64,
        watch: super::concurrency_tests::ConcurrencyWatch,
    }

    #[async_trait]
    impl ModelBackend for SlowGenerationBackend {
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
            self.watch.enter();
            tokio::time::sleep(std::time::Duration::from_millis(self.generate_ms)).await;
            self.watch.leave();
            Ok(GenerateResponse::Text(String::new()))
        }
    }

    pub fn scheduler_with_slow_generation(
        budget_mb: u32,
        generate_ms: u64,
    ) -> (Scheduler, PathBuf, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let peak = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let backend = Arc::new(SlowGenerationBackend {
            generate_ms,
            watch: super::concurrency_tests::ConcurrencyWatch {
                now: std::sync::atomic::AtomicUsize::new(0),
                peak: peak.clone(),
            },
        });
        let (scheduler, dir) = scheduler_with_backend(budget_mb, backend);
        (scheduler, dir, peak)
    }

    /// Загружает нормально, а генерацию всегда проваливает — для проверки,
    /// что счётчик занятости снимается и на пути ошибки.
    struct FailingGenerationBackend;

    #[async_trait]
    impl ModelBackend for FailingGenerationBackend {
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
            Err(anyhow!("подставной отказ генерации"))
        }
    }

    pub fn scheduler_with_failing_generation(budget_mb: u32) -> (Scheduler, PathBuf) {
        scheduler_with_backend(budget_mb, Arc::new(FailingGenerationBackend))
    }

    /// Резидент на 6000 и два кандидата: один влезает поверх него, другой нет.
    fn scheduler_with_budget(budget_mb: u32) -> (Scheduler, PathBuf) {
        scheduler_with_backend(budget_mb, Arc::new(AlwaysOkBackend))
    }

    fn scheduler_with_backend(
        budget_mb: u32,
        backend: Arc<dyn ModelBackend>,
    ) -> (Scheduler, PathBuf) {
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

Resident:
  primary: Resident
",
        )
        .unwrap();

        let scheduler = Scheduler::new(
            Arc::new(CapabilityRegistry::from_file(&capabilities).unwrap()),
            Arc::new(ResourceRegistry::from_file(&resources).unwrap()),
            backend,
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
