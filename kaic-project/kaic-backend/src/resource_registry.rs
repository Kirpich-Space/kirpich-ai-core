//! Resource Registry — таблица ресурсных характеристик моделей.
//!
//! В отличие от Capability Registry (который знает, "что модель умеет"),
//! этот реестр знает, "сколько модель стоит": сколько VRAM занимает,
//! сколько RAM требуется при CPU-offload оставшихся слоёв, сколько
//! секунд занимает холодная загрузка, и должна ли модель быть
//! всегда резидентной в памяти.
//!
//! Использует его исключительно Scheduler — для решения, можно ли
//! сейчас загрузить модель, что выгрузить при нехватке VRAM (LRU),
//! и какие модели выгружать нельзя никогда (`always_loaded`).
//!
//! Router и Capability Registry об этой таблице не знают.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Где физически размещается модель при загрузке.
///
/// Это диагностическое поле на будущее: сейчас все модели грузятся на GPU
/// (с частичным CPU-offload, см. `ram_offload_mb`), но если появится модель,
/// которую выгоднее держать целиком в RAM — Scheduler'у не придётся меняться,
/// он просто прочитает другое значение этого поля.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreferredDevice {
    Gpu,
    Cpu,
    Hybrid,
}

impl Default for PreferredDevice {
    fn default() -> Self {
        PreferredDevice::Gpu
    }
}

/// Ресурсные характеристики одной модели.
#[derive(Debug, Clone, Deserialize)]
pub struct ResourceEntry {
    /// Реальное потребление VRAM (в мегабайтах) при выбранной конфигурации
    /// загрузки — то есть именно то число, на которое Scheduler ориентируется
    /// при решении "поместится ли модель в свободную память сейчас".
    /// Не имеет отношения к тому, сколько слоёв ушло на CPU — это учтено
    /// заранее в самом числе.
    pub vram_mb: u32,

    /// Сколько системной RAM (в мегабайтах) дополнительно занято за счёт
    /// CPU-offload вынесенных слоёв. Scheduler в принятии решений это число
    /// не использует — это диагностическая информация для GUI, логов
    /// и мониторинга памяти.
    #[serde(default)]
    pub ram_offload_mb: u32,

    /// Ожидаемое время холодной загрузки модели в секундах.
    /// Используется Scheduler'ом, чтобы понимать цену переключения моделей.
    pub load_seconds: u32,

    /// Если true — модель не подлежит автоматической выгрузке (LRU).
    /// Держится в памяти постоянно (например Nemotron и Fable9B).
    #[serde(default)]
    pub always_loaded: bool,

    /// Где физически размещается модель. По умолчанию — GPU.
    #[serde(default)]
    pub preferred_device: PreferredDevice,

    /// Ключ модели у провайдера (поле `key` в LM Studio `GET /api/v1/models`).
    ///
    /// Ярлыки реестра (`Nemotron`, `Fable9B`, ...) — внутренние имена KAIC и
    /// с ключами провайдера не совпадают: попытка загрузить модель по ярлыку
    /// даёт `Model not found: Nemotron`. Трансляция живёт здесь, потому что
    /// это единственный слой, который вообще знает о существовании
    /// провайдера: Router оперирует категорией, Capability Registry —
    /// ярлыком, Scheduler — ресурсами.
    ///
    /// `None` — ключ совпадает с ярлыком (или запись обслуживается
    /// backend'ом, который ходит по `path`, а не по ключу).
    #[serde(default)]
    pub lm_studio_key: Option<String>,

    /// Эффективная длина контекста загруженной модели (в токенах).
    ///
    /// Параметр ЗАГРУЗКИ, а не вызова: на `/v1/chat/completions` он молча
    /// игнорируется, но распознаётся схемой `/api/v1/models/load` и виден в
    /// конфиге загруженного инстанса. Значит смена значения потребует
    /// перезагрузки модели.
    ///
    /// `None` — не измерено (модель не загружается на этой машине).
    #[serde(default)]
    pub context_length: Option<u32>,

    /// Потолок контекста, заявленный самой моделью (`max_context_length`
    /// из `GET /api/v1/models`). В отличие от `context_length`, известен для
    /// всех моделей без загрузки.
    #[serde(default)]
    pub max_context_length: Option<u32>,

    /// Температура сэмплинга.
    ///
    /// Параметр ВЫЗОВА: `/v1/chat/completions` принимает его на каждый
    /// запрос, перезагрузка не нужна. `None` означает «не задана» — тогда
    /// применяется умолчание провайдера, которое LM Studio через API не
    /// сообщает.
    #[serde(default)]
    pub temperature: Option<f32>,

    /// Путь к файлу модели (например GGUF), относительно рабочей директории.
    /// Нужен только для backend'ов, которые сами загружают файл модели
    /// (см. будущий EmbeddedBackend) — LMStudioBackend обращается к модели
    /// по имени через LM Studio и это поле не использует, поэтому оно
    /// опционально и для существующих записей реестра не требуется.
    #[serde(default)]
    pub path: Option<String>,
}

/// Реестр ресурсных характеристик всех известных моделей.
///
/// Загружается один раз из YAML-файла при старте системы.
#[derive(Debug)]
pub struct ResourceRegistry {
    entries: HashMap<String, ResourceEntry>,

    /// Откуда реестр прочитан. Нужен, чтобы правка temperature попадала
    /// в тот же файл, а не в предполагаемый.
    source_path: PathBuf,

    /// Изменённые в рантайме значения temperature.
    ///
    /// Отдельная карта, а не мутация `entries`, по одной причине: `get()`
    /// возвращает `&ResourceEntry`, и перевод всего реестра под `RwLock`
    /// сломал бы эту сигнатуру и всех её вызывающих. Temperature —
    /// единственное изменяемое поле, и отдельная карта для него
    /// соразмернее, чем переделка работающего компонента.
    ///
    /// Источник правды на диске — YAML: сюда пишется то же, что туда.
    temperature_overrides: RwLock<HashMap<String, Option<f32>>>,
}

/// Нижняя граница temperature. Не наше решение: провайдер отвергает
/// отрицательные значения (`-1` → 400, `too_small, minimum: 0`).
pub const TEMPERATURE_MIN: f32 = 0.0;

/// Верхняя граница temperature. ЭТО НАШЕ РЕШЕНИЕ, а не ограничение
/// провайдера: LM Studio принимает и 9.0. Берём 2.0 — потолок
/// OpenAI-совместимого API, схему которого повторяет
/// `/v1/chat/completions`. Выше 2.0 распределение вырождается в почти
/// равномерное, то есть в шум, а не в «больше творчества».
pub const TEMPERATURE_MAX: f32 = 2.0;

/// Проверяет значение temperature перед записью.
///
/// `None` («снять настройку») допустимо всегда: это не число, а отказ
/// от параметра, после которого поле не отправляется провайдеру вообще.
///
/// Возвращает текст ошибки для человека — он же уходит в тело 4xx-ответа,
/// поэтому называет и поле, и допустимый диапазон.
pub fn validate_temperature(value: Option<f32>) -> Result<(), String> {
    let Some(v) = value else {
        return Ok(());
    };
    if v.is_nan() || !(TEMPERATURE_MIN..=TEMPERATURE_MAX).contains(&v) {
        return Err(format!(
            "temperature должна быть числом в диапазоне {TEMPERATURE_MIN}–{TEMPERATURE_MAX} \
             (или null, чтобы снять настройку); получено {v}"
        ));
    }
    Ok(())
}

impl ResourceRegistry {
    /// Загружает реестр из YAML-файла.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("не удалось прочитать {}", path.display()))?;
        let entries: HashMap<String, ResourceEntry> = serde_yaml::from_str(&raw)
            .with_context(|| format!("не удалось разобрать YAML в {}", path.display()))?;
        Ok(Self {
            entries,
            source_path: path.to_path_buf(),
            temperature_overrides: RwLock::new(HashMap::new()),
        })
    }

    /// Перечисляет все известные модели вместе с их характеристиками.
    /// Используется Control Center API для отображения списка моделей.
    pub fn all(&self) -> impl Iterator<Item = (&String, &ResourceEntry)> {
        self.entries.iter()
    }

    /// Возвращает ресурсные характеристики модели по имени, если она известна реестру.
    pub fn get(&self, model_name: &str) -> Option<&ResourceEntry> {
        self.entries.get(model_name)
    }

    /// Актуальная temperature модели с учётом правок, сделанных в рантайме.
    ///
    /// `None` означает «не задана»: вызывающий обязан НЕ отправлять поле
    /// провайдеру вообще, сохраняя его умолчание.
    pub fn temperature_for(&self, model: &str) -> Option<f32> {
        if let Ok(overrides) = self.temperature_overrides.read() {
            if let Some(value) = overrides.get(model) {
                return *value;
            }
        }
        self.entries.get(model).and_then(|e| e.temperature)
    }

    /// Задаёт temperature модели и сохраняет её в YAML.
    ///
    /// Файл правится ПОСТРОЧНО, а не перезаписывается через сериализацию:
    /// в `resource_registry.yaml` десятки строк комментариев с
    /// обоснованиями измерений, и `serde_yaml::to_string` стёр бы их все.
    pub fn set_temperature(&self, model: &str, value: Option<f32>) -> Result<()> {
        if !self.entries.contains_key(model) {
            anyhow::bail!("модель '{model}' отсутствует в Resource Registry");
        }
        // Дублирует проверку маршрута API намеренно: реестр — последний
        // рубеж, и он не обязан доверять вызывающему.
        validate_temperature(value).map_err(|e| anyhow::anyhow!(e))?;

        let raw = std::fs::read_to_string(&self.source_path)
            .with_context(|| format!("не удалось прочитать {}", self.source_path.display()))?;
        let updated = replace_temperature_line(&raw, model, value)?;
        write_atomically(&self.source_path, &updated)?;

        self.temperature_overrides
            .write()
            .map_err(|e| anyhow::anyhow!("не удалось обновить кэш temperature: {e}"))?
            .insert(model.to_string(), value);
        Ok(())
    }

    /// Разворачивает внутренний ярлык KAIC в ключ провайдера.
    ///
    /// Вызывается только на границе с `ModelBackend`. Внутреннее состояние
    /// Scheduler'а (LRU, счётчики) продолжает жить на ярлыках — наружу уходит
    /// ключ. Если ключ не задан, возвращается сам ярлык: это сохраняет
    /// прежнее поведение для записей, у которых имя и ключ совпадают.
    pub fn provider_key<'a>(&'a self, label: &'a str) -> &'a str {
        self.entries
            .get(label)
            .and_then(|entry| entry.lm_studio_key.as_deref())
            .unwrap_or(label)
    }

    /// Суммарный объём VRAM (в мегабайтах), который обязаны занимать
    /// always_loaded-модели одновременно. Scheduler использует это как
    /// нижнюю границу постоянно занятой памяти при расчёте свободного места.
    pub fn always_loaded_vram_mb(&self) -> u32 {
        self.entries
            .values()
            .filter(|e| e.always_loaded)
            .map(|e| e.vram_mb)
            .sum()
    }

    /// Имена моделей, помеченных как always_loaded —
    /// Scheduler не имеет права их выгружать по LRU.
    pub fn always_loaded_models(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(_, e)| e.always_loaded)
            .map(|(name, _)| name.as_str())
            .collect()
    }
}

/// Записывает файл целиком, не подставляя его под риск обрыва.
///
/// Прямая запись поверх файла усекает его в самом начале: любой сбой между
/// усечением и концом записи оставляет обрубок. Для `resource_registry.yaml` это
/// потеря 119 строк обоснований измерений, восстановить которые неоткуда.
///
/// Поэтому пишем во временный файл В ТОМ ЖЕ каталоге (rename атомарен
/// только внутри одной файловой системы) и переименовываем поверх цели.
/// `rename` заменяет существующий файл одной операцией: читатель видит
/// либо целиком старое содержимое, либо целиком новое, и никогда — обрубок.
fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "registry.yaml".to_string());
    let tmp = dir.join(format!(".{file_name}.tmp"));

    std::fs::write(&tmp, contents)
        .with_context(|| format!("не удалось записать временный файл {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| {
        // Временный файл оставляем на месте: он содержит валидное новое
        // состояние, и молча удалить его — потерять единственную копию.
        format!(
            "не удалось переименовать {} в {}",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// Заменяет (или добавляет) строку `temperature:` в блоке модели.
///
/// Работает с текстом, а не с деревом YAML, чтобы сохранить комментарии.
/// Блок модели — строки от `Model:` до следующей строки, начинающейся не
/// с пробела.
fn replace_temperature_line(raw: &str, model: &str, value: Option<f32>) -> Result<String> {
    let rendered = match value {
        Some(v) => format!("  temperature: {v}"),
        None => "  temperature: null".to_string(),
    };

    let mut out: Vec<String> = raw.lines().map(str::to_string).collect();
    let header = format!("{model}:");

    let Some(start) = out.iter().position(|l| l.starts_with(&header)) else {
        anyhow::bail!("блок модели '{model}' не найден в YAML");
    };

    // Границы блока: от заголовка до первой строки, начинающейся не с
    // пробела. Пустые строки внутри блок не обрывают — они встречаются
    // между записями и в комментариях.
    let end = out[start + 1..]
        .iter()
        .position(|l| !l.starts_with(' ') && !l.trim().is_empty())
        .map(|offset| start + 1 + offset)
        .unwrap_or(out.len());

    if let Some(idx) = out[start..end]
        .iter()
        .position(|l| l.trim_start().starts_with("temperature:"))
    {
        out[start + idx] = rendered;
    } else {
        // Поля не было — дописываем сразу после последней СОДЕРЖАТЕЛЬНОЙ
        // строки блока, а не в конец диапазона: иначе строка оторвалась бы
        // от блока пустой строкой-разделителем.
        let insert_at = out[start..end]
            .iter()
            .rposition(|l| !l.trim().is_empty())
            .map(|offset| start + offset + 1)
            .unwrap_or(end);
        out.insert(insert_at, rendered);
    }

    let mut text = out.join("\n");
    text.push('\n');
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# шапка с обоснованием
Fable9B:
  vram_mb: 5734
  # комментарий про измерение
  temperature: null
  load_seconds: 9

Nemotron:
  vram_mb: 25723
  temperature: null
";

    #[test]
    fn set_temperature_preserves_comments() {
        // Главное свойство правки: serde_yaml::to_string стёр бы все
        // комментарии с обоснованиями измерений, поэтому правим построчно.
        let out = replace_temperature_line(SAMPLE, "Fable9B", Some(0.2)).expect("правка удалась");

        assert!(out.contains("# шапка с обоснованием"));
        assert!(out.contains("# комментарий про измерение"));
        assert!(out.contains("  temperature: 0.2"));
        // Соседняя модель не задета.
        assert!(out.contains("Nemotron:
  vram_mb: 25723
  temperature: null"));
    }

    #[test]
    fn set_temperature_touches_only_the_named_model() {
        let out = replace_temperature_line(SAMPLE, "Nemotron", Some(1.5)).expect("правка удалась");

        assert!(out.contains("Nemotron:
  vram_mb: 25723
  temperature: 1.5"));
        // У Fable9B осталось прежнее значение.
        assert!(out.contains("  temperature: null
  load_seconds: 9"));
    }

    #[test]
    fn null_is_written_back_as_null_not_dropped() {
        // "Убрать настройку" — осмысленная операция: поле должно остаться
        // в файле со значением null, а не исчезнуть.
        let with_value = replace_temperature_line(SAMPLE, "Fable9B", Some(0.7)).unwrap();
        let cleared = replace_temperature_line(&with_value, "Fable9B", None).unwrap();

        assert!(cleared.contains("  temperature: null"));
        assert!(!cleared.contains("0.7"));
    }

    #[test]
    fn missing_temperature_line_is_appended_to_the_block() {
        let without = "Fable9B:
  vram_mb: 5734

Nemotron:
  vram_mb: 1
";
        let out = replace_temperature_line(without, "Fable9B", Some(0.3)).unwrap();

        assert!(out.contains("Fable9B:
  vram_mb: 5734
  temperature: 0.3"));
    }

    #[test]
    fn unknown_model_is_an_error_not_a_silent_noop() {
        let err = replace_temperature_line(SAMPLE, "НетТакой", Some(0.5))
            .expect_err("несуществующая модель — ошибка");
        assert!(err.to_string().contains("не найден"));
    }


    #[test]
    fn temperature_outside_our_range_is_rejected() {
        // Нижнюю границу диктует провайдер (-1 → 400 too_small),
        // верхнюю выбрали мы: LM Studio принимает и 9.0.
        for bad in [-1.0_f32, -0.1, 2.01, 9.0, f32::NAN] {
            let err = validate_temperature(Some(bad)).expect_err("значение вне диапазона");
            assert!(err.contains("temperature"), "ошибка называет поле: {err}");
            assert!(err.contains("0–2"), "ошибка называет диапазон: {err}");
        }
        // Границы включительно, и null («снять настройку») — не ошибка.
        assert!(validate_temperature(Some(0.0)).is_ok());
        assert!(validate_temperature(Some(2.0)).is_ok());
        assert!(validate_temperature(None).is_ok());
    }

    #[test]
    fn set_temperature_writes_through_a_temp_file_and_leaves_none_behind() {
        // Цена прямой записи поверх файла — обрубок вместо реестра при
        // обрыве. Проверяем, что запись идёт через временный файл в том же
        // каталоге и что после успеха он не остаётся мусором.
        let dir = std::env::temp_dir().join(format!("kaic-registry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("resource_registry.yaml");
        std::fs::write(
            &path,
            "# шапка с обоснованием
Fable9B:
  vram_mb: 5734
  load_seconds: 9
  temperature: null
",
        )
        .unwrap();

        let registry = ResourceRegistry::from_file(&path).unwrap();
        registry.set_temperature("Fable9B", Some(1.0)).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("  temperature: 1"));
        assert!(written.contains("# шапка с обоснованием"));

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "остался временный файл: {leftovers:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    fn sample_registry() -> ResourceRegistry {
        let yaml = r#"
Nemotron:
  vram_mb: 3000
  load_seconds: 2
  always_loaded: true

Fable9B:
  vram_mb: 6000
  load_seconds: 5
  always_loaded: true

Qwen40B:
  vram_mb: 6000
  ram_offload_mb: 18000
  load_seconds: 90
"#;
        let entries: HashMap<String, ResourceEntry> = serde_yaml::from_str(yaml).unwrap();
        ResourceRegistry {
            entries,
            // Тестовый реестр не привязан к файлу: set_temperature здесь
            // не вызывается, а чтение работает и без источника на диске.
            source_path: PathBuf::from("<test>"),
            temperature_overrides: RwLock::new(HashMap::new()),
        }
    }

    #[test]
    fn get_returns_known_model() {
        let registry = sample_registry();
        let entry = registry.get("Fable9B").unwrap();
        assert_eq!(entry.vram_mb, 6000);
        assert!(entry.always_loaded);
    }

    #[test]
    fn get_returns_none_for_unknown_model() {
        let registry = sample_registry();
        assert!(registry.get("Unknown").is_none());
    }

    #[test]
    fn always_loaded_vram_sums_only_flagged_models() {
        let registry = sample_registry();
        // Nemotron (3000) + Fable9B (6000), Qwen40B не always_loaded
        assert_eq!(registry.always_loaded_vram_mb(), 9000);
    }

    #[test]
    fn ram_offload_defaults_to_zero() {
        let registry = sample_registry();
        assert_eq!(registry.get("Nemotron").unwrap().ram_offload_mb, 0);
    }

    #[test]
    fn preferred_device_defaults_to_gpu() {
        let registry = sample_registry();
        assert_eq!(
            registry.get("Nemotron").unwrap().preferred_device,
            PreferredDevice::Gpu
        );
    }

    #[test]
    fn path_defaults_to_none() {
        let registry = sample_registry();
        assert_eq!(registry.get("Nemotron").unwrap().path, None);
    }
}
