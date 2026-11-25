//! Размещение не должно выгружать модели ради заведомо недостижимого кандидата.
use super::*;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;

#[derive(Default)]
struct RecordingBackend {
    operations: StdMutex<Vec<String>>,
}

#[async_trait]
impl ModelBackend for RecordingBackend {
    async fn is_loaded(&self, _: &str) -> Result<bool> {
        Ok(false)
    }

    async fn load(&self, model: &str) -> Result<()> {
        self.operations
            .lock()
            .unwrap()
            .push(format!("load:{model}"));
        Ok(())
    }

    async fn unload(&self, model: &str) -> Result<()> {
        self.operations
            .lock()
            .unwrap()
            .push(format!("unload:{model}"));
        Ok(())
    }

    async fn generate(&self, _: &str, _: GenerateRequest) -> Result<GenerateResponse> {
        Ok(GenerateResponse::Text("ok".into()))
    }
}

struct Fixture {
    scheduler: Scheduler,
    backend: Arc<RecordingBackend>,
    dir: PathBuf,
}

impl Fixture {
    fn with_memory(budget_mb: u32, entries: &[(&str, u32, bool)]) -> Self {
        let mut fixture = Self::new();
        let resources = fixture.dir.join("arithmetic-resources.yaml");
        let yaml: String = entries
            .iter()
            .map(|(name, mb, resident)| {
                format!("{name}: {{vram_mb: {mb}, load_seconds: 0, always_loaded: {resident}}}\n")
            })
            .collect();
        std::fs::write(&resources, yaml).unwrap();
        fixture.scheduler.resource_registry =
            Arc::new(ResourceRegistry::from_file(resources).unwrap());
        fixture.scheduler.total_model_memory_mb = budget_mb;
        fixture
    }

    fn new() -> Self {
        let backend = Arc::new(RecordingBackend::default());
        let (mut scheduler, dir) =
            super::budget_tests::scheduler_with_backend(100, backend.clone());
        let resources = dir.join("placement-resources.yaml");
        std::fs::write(
            &resources,
            "Resident: {vram_mb: 30, load_seconds: 1, always_loaded: true}\n\
             Cached: {vram_mb: 20, load_seconds: 1}\n\
             Busy: {vram_mb: 40, load_seconds: 1}\n\
             Huge: {vram_mb: 110, load_seconds: 1}\n\
             Blocked: {vram_mb: 80, load_seconds: 1}\n\
             Fits: {vram_mb: 70, load_seconds: 1}\n",
        )
        .unwrap();
        let capabilities = dir.join("placement-capabilities.yaml");
        std::fs::write(
            &capabilities,
            "Warm: {primary: Cached}\n\
             Huge: {primary: Huge}\n\
             Blocked: {primary: Blocked}\n\
             Fallback: {primary: Blocked, fallback: [Cached]}\n\
             Fits: {primary: Fits}\n",
        )
        .unwrap();
        scheduler.resource_registry = Arc::new(ResourceRegistry::from_file(resources).unwrap());
        scheduler.capability_registry =
            Arc::new(CapabilityRegistry::from_file(capabilities).unwrap());
        Self {
            scheduler,
            backend,
            dir,
        }
    }

    async fn warm(&self, resident: bool) {
        if resident {
            self.scheduler.preload_always_loaded().await.unwrap();
        }
        let (_, guard) = self.scheduler.acquire("Warm", false).await.unwrap();
        drop(guard);
        self.backend.operations.lock().unwrap().clear();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).unwrap();
    }
}

#[tokio::test]
async fn impossible_candidates_preserve_cached_models_without_backend_calls() {
    // Первый кандидат превышает весь бюджет; второй не помещается с резидентом.
    for (category, resident) in [("Huge", false), ("Blocked", true)] {
        let fixture = Fixture::new();
        fixture.warm(resident).await;
        let before = fixture.scheduler.snapshot().await;

        assert!(fixture.scheduler.acquire(category, false).await.is_err());

        let after = fixture.scheduler.snapshot().await;
        assert_eq!(after.len(), before.len(), "кандидат {category}");
        for entry in before {
            assert!(after.contains(&entry), "сохранён учёт и LRU: {entry:?}");
        }
        assert!(fixture.backend.operations.lock().unwrap().is_empty());
        assert_eq!(fixture.scheduler.running_generations(), 0);
        assert!(fixture.scheduler.active_operation().is_none());
        assert!(fixture.scheduler.recovery_warnings().is_empty());
    }
}

#[tokio::test]
async fn impossible_primary_reuses_loaded_fallback_without_reload() {
    let fixture = Fixture::new();
    fixture.warm(true).await;

    let (model, guard) = fixture.scheduler.acquire("Fallback", false).await.unwrap();

    assert_eq!(model, "Cached");
    assert!(fixture.backend.operations.lock().unwrap().is_empty());
    drop(guard);
}

#[tokio::test]
async fn busy_models_count_toward_memory_that_cannot_be_reclaimed() {
    let fixture = Fixture::new();
    fixture.warm(true).await;
    // Прямой вызов размещения под тем же локом, что держат production callers.
    let _operation = fixture.scheduler.gpu_lock.lock().await;
    assert!(fixture.scheduler.fit_and_load("Busy", 40, "test").await);
    let guard = fixture.scheduler.try_begin_generation("Busy").unwrap();
    fixture.backend.operations.lock().unwrap().clear();

    assert!(!fixture.scheduler.fit_and_load("Fits", 70, "test").await);

    assert!(fixture.backend.operations.lock().unwrap().is_empty());
    assert_eq!(fixture.scheduler.snapshot().await.len(), 3);
    assert_eq!(fixture.scheduler.running_generations(), 1);
    drop(guard);
}

#[test]
fn memory_budget_does_not_wrap_at_u32_limit() {
    assert!(!fits_in_budget(30, u32::MAX, 100));
    assert!(!fits_in_budget(30, u32::MAX, u32::MAX));
    assert!(fits_in_budget(30, u32::MAX - 30, u32::MAX));
    assert!(!fits_in_budget(u64::from(u32::MAX) + 1, 0, u32::MAX));
    assert!(!fits_in_budget(u64::MAX, 1, u32::MAX));
}

#[tokio::test]
async fn reachable_candidate_still_evicts_lru_and_fits_exact_budget() {
    let fixture = Fixture::new();
    fixture.warm(true).await;

    let (model, guard) = fixture.scheduler.acquire("Fits", false).await.unwrap();

    assert_eq!(model, "Fits");
    assert_eq!(
        *fixture.backend.operations.lock().unwrap(),
        ["unload:Cached", "load:Fits"]
    );
    let loaded = fixture.scheduler.snapshot().await;
    assert_eq!(loaded.len(), 2);
    assert!(loaded.iter().any(|(name, _)| name == "Resident"));
    assert!(loaded.iter().any(|(name, _)| name == "Fits"));
    drop(guard);
}

#[test]
fn resident_accounting_handles_multiple_and_zero_memory_entries() {
    for (entries, expected) in [
        (
            vec![
                ("Resident", 30, true),
                ("Other", 20, true),
                ("Cached", 90, false),
            ],
            50,
        ),
        (
            vec![
                ("Resident", 0, true),
                ("Other", 0, true),
                ("Cached", 90, false),
            ],
            0,
        ),
        (vec![("Cached", u32::MAX, false)], 0),
    ] {
        let fixture = Fixture::with_memory(100, &entries);
        assert_eq!(
            fixture.scheduler.resource_registry.always_loaded_vram_mb(),
            expected
        );
    }
}

#[test]
fn resident_accounting_is_exact_at_and_above_u32_max() {
    for (first, second) in [
        (u32::MAX - 20, 19),
        (u32::MAX - 20, 20),
        (u32::MAX, 1),
        (u32::MAX, u32::MAX),
    ] {
        let fixture = Fixture::with_memory(
            u32::MAX,
            &[("Resident", first, true), ("Other", second, true)],
        );
        let actual = fixture.scheduler.resource_registry.always_loaded_vram_mb();
        assert_eq!(actual, u64::from(first) + u64::from(second));
    }
}

#[tokio::test]
async fn oversized_resident_total_is_rejected_before_any_backend_call() {
    let fixture = Fixture::with_memory(
        u32::MAX,
        &[("Resident", u32::MAX, true), ("Other", 1, true)],
    );
    assert!(fixture.scheduler.preload_always_loaded().await.is_err());
    assert!(fixture.backend.operations.lock().unwrap().is_empty());
    assert!(fixture.scheduler.snapshot().await.is_empty());
    assert!(fixture.scheduler.active_operation().is_none());
    assert!(fixture.scheduler.recovery_warnings().is_empty());
}

#[test]
fn oversized_resident_total_cannot_make_any_model_appear_reachable() {
    let fixture = Fixture::with_memory(
        u32::MAX,
        &[
            ("Resident", u32::MAX, true),
            ("Other", 1, true),
            ("Cached", 0, false),
        ],
    );
    let report = fixture.scheduler.reachability();
    assert_eq!(report.resident_mb, u64::from(u32::MAX) + 1);
    assert_eq!(report.unreachable.len(), 3);
    assert!(report.empty_categories.contains(&"Warm".to_string()));
    fixture.scheduler.log_reachability();
}

#[tokio::test]
async fn exact_u32_budget_preserves_resident_fallback_without_eviction() {
    let fixture = Fixture::with_memory(
        u32::MAX,
        &[
            ("Resident", u32::MAX - 20, true),
            ("Cached", 20, true),
            ("Blocked", 1, false),
        ],
    );
    fixture.scheduler.preload_always_loaded().await.unwrap();
    fixture.backend.operations.lock().unwrap().clear();
    let (model, guard) = fixture.scheduler.acquire("Fallback", false).await.unwrap();
    assert_eq!(model, "Cached");
    assert!(fixture.backend.operations.lock().unwrap().is_empty());
    assert_eq!(fixture.scheduler.snapshot().await.len(), 2);
    drop(guard);
}

#[tokio::test]
async fn overflowing_protected_total_rejects_placement_without_evicting_cache() {
    let fixture = Fixture::with_memory(
        u32::MAX,
        &[
            ("Resident", u32::MAX, true),
            ("Other", 1, true),
            ("Cached", 0, false),
            ("Fits", 1, false),
        ],
    );
    // Defensive test for a pre-existing inconsistent residency snapshot.
    // Loading must fail safely even if the accounted state exceeds capacity.
    for name in ["Resident", "Other", "Cached"] {
        fixture.scheduler.loaded.lock().await.insert(
            name.into(),
            ModelUsage {
                last_used: Utc::now(),
            },
        );
    }
    let before = fixture.scheduler.snapshot().await;
    let _operation = fixture.scheduler.gpu_lock.lock().await;
    assert!(!fixture.scheduler.fit_and_load("Fits", 1, "test").await);
    let after = fixture.scheduler.snapshot().await;
    assert_eq!(after.len(), before.len());
    assert!(before.iter().all(|entry| after.contains(entry)));
    assert!(fixture.backend.operations.lock().unwrap().is_empty());
}

#[tokio::test]
async fn overflowing_loaded_total_must_be_reduced_before_loading_a_zero_memory_model() {
    let fixture = Fixture::with_memory(
        u32::MAX,
        &[
            ("Resident", u32::MAX, true),
            ("Cached", 1, false),
            ("Fits", 0, false),
        ],
    );
    for name in ["Resident", "Cached"] {
        fixture.scheduler.loaded.lock().await.insert(
            name.into(),
            ModelUsage {
                last_used: Utc::now(),
            },
        );
    }
    let _operation = fixture.scheduler.gpu_lock.lock().await;
    assert!(fixture.scheduler.fit_and_load("Fits", 0, "test").await);
    assert_eq!(
        *fixture.backend.operations.lock().unwrap(),
        ["unload:Cached", "load:Fits"]
    );
    let loaded = fixture.scheduler.snapshot().await;
    assert_eq!(loaded.len(), 2);
    assert!(loaded.iter().any(|(name, _)| name == "Resident"));
    assert!(loaded.iter().any(|(name, _)| name == "Fits"));
}

#[tokio::test]
async fn managed_zero_memory_model_can_use_an_exactly_full_budget() {
    let fixture = Fixture::with_memory(
        u32::MAX,
        &[("Resident", u32::MAX, true), ("Cached", 0, false)],
    );
    fixture.warm(true).await;
    assert_eq!(fixture.scheduler.snapshot().await.len(), 2);
    assert!(fixture.backend.operations.lock().unwrap().is_empty());
}

#[tokio::test]
async fn near_u32_limit_placement_evicts_only_the_oldest_eligible_model() {
    let fixture = Fixture::with_memory(
        u32::MAX,
        &[
            ("Resident", u32::MAX - 40, true),
            ("Cached", 20, false),
            ("Busy", 10, false),
            ("Fits", 20, false),
        ],
    );
    fixture.warm(true).await;
    {
        let _operation = fixture.scheduler.gpu_lock.lock().await;
        assert!(fixture.scheduler.fit_and_load("Busy", 10, "test").await);
    }
    fixture
        .scheduler
        .loaded
        .lock()
        .await
        .get_mut("Cached")
        .unwrap()
        .last_used = Utc::now() - chrono::Duration::seconds(60);
    fixture.backend.operations.lock().unwrap().clear();
    let (model, guard) = fixture.scheduler.acquire("Fits", false).await.unwrap();
    assert_eq!(model, "Fits");
    assert_eq!(
        *fixture.backend.operations.lock().unwrap(),
        ["unload:Cached", "load:Fits"]
    );
    let loaded = fixture.scheduler.snapshot().await;
    assert_eq!(loaded.len(), 3);
    assert!(loaded.iter().any(|(name, _)| name == "Resident"));
    assert!(loaded.iter().any(|(name, _)| name == "Busy"));
    drop(guard);
}

#[tokio::test]
async fn preloading_residents_cannot_overcommit_memory_already_used_by_cache() {
    let fixture = Fixture::with_memory(
        u32::MAX,
        &[("Resident", u32::MAX, true), ("Cached", 20, false)],
    );
    fixture.warm(false).await;
    let before = fixture.scheduler.snapshot().await;
    assert!(fixture.scheduler.preload_always_loaded().await.is_err());
    assert!(fixture.backend.operations.lock().unwrap().is_empty());
    assert_eq!(fixture.scheduler.snapshot().await, before);
}
