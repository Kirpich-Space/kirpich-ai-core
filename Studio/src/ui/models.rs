//! Менеджер моделей, экран загрузок и поиск Hugging Face в фирменном стиле KAIC Studio (100% нативный Rust / egui).
use super::{Studio, Tab, anim, i18n::Lang, palette};
use crate::{catalog, desktop::hf, service::Job};
use eframe::egui::{self, Color32, RichText, Stroke};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::mpsc, time::Duration};

#[derive(PartialEq, Clone, Copy)]
pub(super) enum ModelsTab {
    Discover,
    Local,
    Downloads,
}

#[derive(PartialEq, Clone, Copy)]
enum QuantScope {
    All,
    Recommended,
    FitsGpu,
}

#[derive(PartialEq, Clone, Copy)]
pub(super) enum LocalKindFilter {
    All,
    Text,
    Image,
    Video,
}

#[derive(PartialEq, Clone, Copy)]
enum LocalSort {
    Recommended,
    Speed,
    SizeAsc,
    SizeDesc,
    Name,
}

#[derive(Clone, Debug)]
struct HfRepo {
    id: String,
    downloads: u64,
    likes: u64,
    last_modified: String,
    tags: Vec<String>,
}

#[derive(Clone, Debug)]
struct HfFile {
    filename: String,
    size: u64,
}

enum HfReply {
    Search(Result<Vec<HfRepo>, String>),
    Files(String, Result<Vec<HfFile>, String>),
    Action(Result<(), String>),
}

const TRUSTED_AUTHORS: &[&str] = &[
    "lmstudio-community",
    "unsloth",
    "bartowski",
    "ggml-org",
    "city96",
    "Qwen",
    "ibm-granite",
    "mistralai",
    "MaziyarPanahi",
    "QuantFactory",
    "mradermacher",
];

fn is_trusted_author(repo_id: &str) -> bool {
    let owner = repo_id.split('/').next().unwrap_or("");
    TRUSTED_AUTHORS
        .iter()
        .any(|a| a.eq_ignore_ascii_case(owner))
}

const FEATURED_GGUF: &[(&str, &[&str])] = &[
    (
        "unsloth/Qwen3-8B-GGUF",
        &["qwen3", "text-generation", "gguf"],
    ),
    (
        "unsloth/Qwen2.5-Coder-7B-Instruct-GGUF",
        &["qwen2", "code", "gguf"],
    ),
    (
        "bartowski/gemma-3-4b-it-GGUF",
        &["gemma3", "text-generation", "gguf"],
    ),
    (
        "unsloth/gemma-3-12b-it-GGUF",
        &["gemma3", "text-generation", "gguf"],
    ),
    (
        "bartowski/Meta-Llama-3.1-8B-Instruct-GGUF",
        &["llama", "text-generation", "gguf"],
    ),
    (
        "unsloth/DeepSeek-R1-Distill-Qwen-7B-GGUF",
        &["deepseek", "reasoning", "gguf"],
    ),
    (
        "mistralai/Ministral-3-8B-Instruct-2512-GGUF",
        &["mistral", "text-generation", "gguf"],
    ),
    (
        "ibm-granite/granite-4.0-h-tiny-GGUF",
        &["granite", "text-generation", "gguf"],
    ),
    (
        "unsloth/Phi-4-mini-instruct-GGUF",
        &["phi4", "text-generation", "gguf"],
    ),
    (
        "ggml-org/SmolLM3-3B-GGUF",
        &["smollm3", "text-generation", "gguf"],
    ),
    (
        "city96/FLUX.1-schnell-gguf",
        &["flux", "text-to-image", "gguf"],
    ),
    (
        "unsloth/Qwen-Image-2.1-Turbo-GGUF",
        &["qwen-image", "text-to-image", "gguf"],
    ),
    (
        "city96/Wan2.1-T2V-1.3B-gguf",
        &["wan2.1", "text-to-video", "t2v", "gguf"],
    ),
];

fn featured_fallback(query: &str) -> Vec<HfRepo> {
    let q = query.trim().to_lowercase();
    FEATURED_GGUF
        .iter()
        .filter(|(id, tags)| {
            q.is_empty() || id.to_lowercase().contains(&q) || tags.iter().any(|t| t.contains(&q))
        })
        .map(|(id, tags)| HfRepo {
            id: (*id).to_string(),
            downloads: 0,
            likes: 0,
            last_modified: String::new(),
            tags: tags.iter().map(|t| (*t).to_string()).collect(),
        })
        .collect()
}

pub(super) struct HfPanel {
    manager: hf::Manager,
    pub(super) tab: ModelsTab,
    query: String,
    sort: &'static str,
    only_llm: bool,
    repos: Vec<HfRepo>,
    selected: Option<HfRepo>,
    files: Vec<HfFile>,
    quant_filter: String,
    quant_scope: QuantScope,
    local_filter: String,
    pub(super) local_kind: LocalKindFilter,
    local_sort: LocalSort,
    pub(super) directory: String,
    pub(super) token_draft: String,
    searching: bool,
    loading_files: bool,
    initialized: bool,
    was_hf_active: bool,
    confirm_delete_model: Option<catalog::Model>,
    error: Option<String>,
    tx: mpsc::Sender<HfReply>,
    rx: mpsc::Receiver<HfReply>,
}

impl HfPanel {
    pub(super) fn new(data: PathBuf) -> Self {
        let manager = hf::Manager::open(data);
        let default_dir = manager.snapshot()["default_directory"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let (tx, rx) = mpsc::channel();
        Self {
            manager,
            tab: ModelsTab::Discover,
            query: String::new(),
            sort: "best",
            only_llm: false,
            repos: Vec::new(),
            selected: None,
            files: Vec::new(),
            quant_filter: String::new(),
            quant_scope: QuantScope::All,
            local_filter: String::new(),
            local_kind: LocalKindFilter::All,
            local_sort: LocalSort::Recommended,
            directory: default_dir,
            token_draft: String::new(),
            searching: false,
            loading_files: false,
            initialized: false,
            was_hf_active: false,
            confirm_delete_model: None,
            error: None,
            tx,
            rx,
        }
    }

    pub(super) fn token_present(&self) -> bool {
        self.manager.snapshot()["token_present"]
            .as_bool()
            .unwrap_or(false)
    }

    pub(super) fn save_token(&mut self, ctx: &egui::Context, token: String) {
        self.spawn_api(
            ctx.clone(),
            "/api/hf/token",
            json!({ "token": token }),
            |r| HfReply::Action(r.map(|_| ())),
        );
    }

    pub(super) fn default_directory(&self) -> String {
        self.manager.snapshot()["default_directory"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    fn spawn_api(
        &self,
        ctx: egui::Context,
        path: &'static str,
        body: Value,
        map: impl FnOnce(Result<Value, String>) -> HfReply + Send + 'static,
    ) {
        let mgr = self.manager.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("{e:#}"))
                .and_then(|rt| {
                    let bytes = serde_json::to_vec(&body).unwrap_or_default();
                    rt.block_on(mgr.api(path, &bytes))
                        .map_err(|e| format!("{e:#}"))
                });
            let _ = tx.send(map(res));
            ctx.request_repaint();
        });
    }

    fn run_search(&mut self, ctx: &egui::Context) {
        self.error = None;
        let q = self.query.trim().to_string();
        if q.contains('/')
            && q.split('/').count() == 2
            && q.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
        {
            let direct = HfRepo {
                id: q.clone(),
                downloads: 0,
                likes: 0,
                last_modified: String::new(),
                tags: vec!["gguf".into()],
            };
            self.repos = vec![direct.clone()];
            self.load_files(ctx, direct);
            return;
        }
        self.searching = true;
        let api_sort = if self.sort == "best" {
            "downloads"
        } else {
            self.sort
        };
        let rank_best = self.sort == "best";
        let body = json!({
            "query": q,
            "sort": api_sort,
            "all_types": !self.only_llm,
        });
        self.spawn_api(ctx.clone(), "/api/hf/search", body, move |res| {
            HfReply::Search(res.map(|v| {
                let mut list: Vec<HfRepo> = if let Some(arr) = v["items"].as_array() {
                    arr.iter()
                        .filter_map(|e| {
                            Some(HfRepo {
                                id: e["id"].as_str()?.to_string(),
                                downloads: e["downloads"].as_u64().unwrap_or(0),
                                likes: e["likes"].as_u64().unwrap_or(0),
                                last_modified: e["last_modified"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string(),
                                tags: e["tags"]
                                    .as_array()
                                    .map(|t| {
                                        t.iter()
                                            .filter_map(|x| x.as_str().map(str::to_string))
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                            })
                        })
                        .collect()
                } else {
                    v["repos"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|x| x.as_str())
                        .map(|id| HfRepo {
                            id: id.to_string(),
                            downloads: 0,
                            likes: 0,
                            last_modified: String::new(),
                            tags: vec!["gguf".into()],
                        })
                        .collect()
                };
                if rank_best {
                    list.sort_by(|a, b| {
                        let ta = is_trusted_author(&a.id);
                        let tb = is_trusted_author(&b.id);
                        tb.cmp(&ta)
                            .then_with(|| b.downloads.cmp(&a.downloads))
                            .then_with(|| b.likes.cmp(&a.likes))
                    });
                }
                list
            }))
        });
    }

    fn load_files(&mut self, ctx: &egui::Context, repo: HfRepo) {
        let id = repo.id.clone();
        self.selected = Some(repo);
        self.files.clear();
        self.quant_filter.clear();
        self.loading_files = true;
        self.error = None;
        let id_reply = id.clone();
        self.spawn_api(ctx.clone(), "/api/hf/files", json!({ "repo": id }), |res| {
            HfReply::Files(
                id_reply,
                res.map(|v| {
                    let mut list: Vec<HfFile> = v["files"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|f| {
                            Some(HfFile {
                                filename: f["filename"].as_str()?.to_string(),
                                size: f["size"].as_u64().unwrap_or(0),
                            })
                        })
                        .collect();
                    list.sort_by_key(|f| f.size);
                    list
                }),
            )
        });
    }

    pub(super) fn poll(&mut self, ctx: &egui::Context) -> bool {
        let mut need_refresh_models = false;
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                HfReply::Search(Ok(list)) => {
                    self.searching = false;
                    self.repos = list;
                    if self.repos.is_empty() {
                        self.selected = None;
                        self.files.clear();
                    } else {
                        let still_in_list = self
                            .selected
                            .as_ref()
                            .is_some_and(|s| self.repos.iter().any(|r| r.id == s.id));
                        if !still_in_list {
                            let first = self.repos[0].clone();
                            #[cfg(not(test))]
                            self.load_files(ctx, first);
                            #[cfg(test)]
                            {
                                let _ = ctx;
                                self.selected = Some(first);
                            }
                        }
                    }
                }
                HfReply::Search(Err(e)) => {
                    self.searching = false;
                    self.error = Some(e);
                    if self.repos.is_empty() {
                        self.repos = featured_fallback(&self.query);
                    }
                }
                HfReply::Files(id, Ok(files)) => {
                    if self.selected.as_ref().is_some_and(|s| s.id == id) {
                        self.loading_files = false;
                        self.files = files;
                    }
                }
                HfReply::Files(id, Err(e)) => {
                    if self.selected.as_ref().is_some_and(|s| s.id == id) {
                        self.loading_files = false;
                        self.error = Some(e);
                    }
                }
                HfReply::Action(Ok(())) => {
                    need_refresh_models = true;
                }
                HfReply::Action(Err(e)) => {
                    self.error = Some(e);
                }
            }
        }
        need_refresh_models
    }
}

fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

pub(super) fn parse_quant(filename: &str) -> String {
    let stem = filename
        .strip_suffix(".gguf")
        .or_else(|| filename.strip_suffix(".GGUF"))
        .or_else(|| filename.strip_suffix(".safetensors"))
        .unwrap_or(filename);
    for part in stem.rsplit(['-', '.', '_']) {
        let up = part.to_ascii_uppercase();
        if matches!(
            up.as_str(),
            "BF16" | "F16" | "F32" | "FP16" | "FP8" | "MXFP4"
        ) {
            return up;
        }
    }
    for token in stem.rsplit(['-', '.']) {
        let up = token.to_ascii_uppercase();
        if (up.starts_with("IQ") || up.starts_with('Q'))
            && up.len() >= 3
            && up
                .as_bytes()
                .get(1)
                .is_some_and(|b| b.is_ascii_digit() || *b == b'Q')
        {
            return up;
        }
    }
    if filename.ends_with(".safetensors") {
        "FP16".into()
    } else {
        "GGUF".into()
    }
}

fn is_recommended(quant: &str) -> bool {
    matches!(quant, "Q4_K_M" | "Q5_K_M")
}

/// Человекопонятное описание уровня квантизации GGUF.
pub(super) fn quant_quality_hint(quant: &str, lang: Lang) -> &'static str {
    match quant {
        "Q4_K_M" => lang.t(
            "Balanced · best speed & quality (4-bit)",
            "Оптимальный баланс скорости и качества (4-бит)",
        ),
        "Q5_K_M" => lang.t(
            "High quality · low perplexity (5-bit)",
            "Высокая точность · минимум потерь (5-бит)",
        ),
        "Q5_K_S" | "Q5_K_L" | "Q5_K_XL" | "Q5_0" => lang.t(
            "High quality · 5-bit quantization",
            "Высокое качество · 5-битная квантизация",
        ),
        "Q4_K_S" | "Q4_K_L" | "Q4_K_XL" | "Q4_0" | "IQ4_NL" | "IQ4_XS" => lang.t(
            "Fast & compact · 4-bit quantization",
            "Быстрая и компактная · 4-битная квантизация",
        ),
        "Q6_K" | "Q6_K_L" | "Q6_K_XL" => lang.t(
            "Very high quality · near-FP16 (6-bit)",
            "Очень высокое качество · близко к FP16 (6-бит)",
        ),
        "Q8_0" => lang.t(
            "Maximum quality · near-lossless (8-bit)",
            "Максимальное качество · без потерь (8-бит)",
        ),
        "Q3_K_S" | "Q3_K_M" | "Q3_K_L" | "Q3_K_XL" | "IQ3_M" | "IQ3_S" | "IQ3_XS" | "IQ3_XXS" => {
            lang.t(
                "Compact · saves VRAM with moderate quality loss (3-bit)",
                "Компактная · экономия VRAM (3-бит)",
            )
        }
        "Q2_K" | "Q2_K_L" | "Q2_K_XL" | "IQ2_M" | "IQ2_S" | "IQ2_XS" | "IQ2_XXS" | "IQ1_M"
        | "IQ1_S" => lang.t(
            "Extreme compression · high quality loss (1–2 bit)",
            "Экстремальное сжатие · заметная потеря точности",
        ),
        "F16" | "BF16" | "FP16" => lang.t(
            "Full 16-bit precision weights",
            "Полная 16-битная точность весов",
        ),
        "MXFP4" => lang.t("Microscaling 4-bit float", "Микроскейлинг 4-бит FP4"),
        _ => lang.t("Standard GGUF weights", "Стандартный файл весов GGUF"),
    }
}

/// Извлекает бейдж числа параметров из имени модели (например `26B`, `8B`, `1.3B`).
pub(super) fn extract_param_badge(text: &str) -> Option<String> {
    for raw in text.split(['/', '-', '_', ' ']) {
        let up = raw.to_ascii_uppercase();
        if let Some(num) = up.strip_suffix('B')
            && !num.is_empty()
            && num.len() <= 5
            && num.chars().next().is_some_and(|c| c.is_ascii_digit())
            && num.chars().all(|c| c.is_ascii_digit() || c == '.')
        {
            return Some(format!("{num}B"));
        }
    }
    None
}

/// Выбирает лучший рекомендуемый GGUF-файл из списка файлов репозитория под 8 ГБ VRAM.
fn pick_recommended_file(files: &[HfFile]) -> Option<&HfFile> {
    let non_aux = |f: &&HfFile| !catalog::is_auxiliary_encoder(&f.filename);
    // 1. Сначала ищем Q4_K_M или Q5_K_M, который помещается в GPU (<= 6.5 ГБ)
    if let Some(f) = files.iter().filter(non_aux).find(|f| {
        let q = parse_quant(&f.filename);
        (q == "Q4_K_M" || q == "Q4_K") && f.size <= 6_979_321_856
    }) {
        return Some(f);
    }
    if let Some(f) = files.iter().filter(non_aux).find(|f| {
        let q = parse_quant(&f.filename);
        (q == "Q5_K_M" || q == "Q5_0") && f.size <= 6_979_321_856
    }) {
        return Some(f);
    }
    // 2. Иначе ищем любой Q4_K_M / Q4_K или Q5_K_M / Q5_0 среди основных весов
    if let Some(f) = files.iter().filter(non_aux).find(|f| {
        let q = parse_quant(&f.filename);
        q == "Q4_K_M" || q == "Q4_K"
    }) {
        return Some(f);
    }
    if let Some(f) = files.iter().filter(non_aux).find(|f| {
        let q = parse_quant(&f.filename);
        q == "Q5_K_M" || q == "Q5_0"
    }) {
        return Some(f);
    }
    // 3. Иначе ищем самый крупный основной файл, который влезает в 8 ГБ VRAM, либо первый подходящий
    files
        .iter()
        .filter(non_aux)
        .rev()
        .find(|f| f.size <= 6_979_321_856 && f.size >= 100 * 1024 * 1024)
        .or_else(|| files.iter().find(non_aux))
        .or_else(|| files.first())
}

/// Оценка размещения GGUF в 8 ГБ VRAM строго в палитре KAIC (Carbon / Bone / Kiln — без синего и жёлтого).
pub(super) fn fit_hint(size: u64, lang: Lang) -> (&'static str, Color32, Color32, bool, f32) {
    let pal = palette::p();
    let gb = size as f64 / 1_073_741_824.0;
    let fill = (gb as f32 / 8.0).clamp(0.08, 1.0);
    if gb <= 6.2 {
        (
            lang.t("Full GPU", "Полностью в GPU"),
            pal.ceramic_200,
            pal.brand,
            true,
            fill,
        )
    } else if gb <= 10.5 {
        (
            lang.t("Partial GPU", "Частично GPU"),
            pal.ink_muted,
            pal.line_strong,
            true,
            fill,
        )
    } else {
        (
            lang.t("CPU offload", "Выгрузка в ОЗУ"),
            pal.ink_muted,
            pal.line,
            false,
            1.0,
        )
    }
}

/// Тонкая 4px шкала заполнения 8 ГБ VRAM для файла GGUF.
pub(super) fn vram_bar(ui: &mut egui::Ui, frac: f32, color: Color32) {
    let pal = palette::p();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(52.0, 12.0), egui::Sense::hover());
    let track = egui::Rect::from_center_size(rect.center(), egui::vec2(52.0, 4.0));
    let p = ui.painter();
    p.rect_filled(track, 2, pal.surface_2);
    let filled = egui::Rect::from_min_size(
        track.min,
        egui::vec2(
            (track.width() * frac.clamp(0.05, 1.0)).max(3.0),
            track.height(),
        ),
    );
    p.rect_filled(filled, 2, color);
}

fn dl_label(state: &str, lang: Lang) -> &str {
    match state {
        "connecting" => lang.t("Connecting to Hugging Face…", "Подключение к Hugging Face…"),
        "downloading" => lang.t("Downloading", "Скачивание"),
        "validating" => lang.t("Verifying SHA256", "Проверка SHA256"),
        "paused" => lang.t("Paused", "На паузе"),
        "error" => lang.t("Error", "Ошибка"),
        "ready" => lang.t("Ready · in My Models", "Готова · в Моих моделях"),
        other => other,
    }
}

pub(super) fn format_speed(bps: u64, lang: Lang) -> String {
    if bps == 0 {
        return String::new();
    }
    let mb = bps as f64 / (1024.0 * 1024.0);
    if mb >= 1.0 {
        format!("{mb:.1} {}", lang.t("MB/s", "МБ/с"))
    } else {
        let kb = bps as f64 / 1024.0;
        format!("{kb:.0} {}", lang.t("KB/s", "КБ/с"))
    }
}

pub(super) fn format_eta(secs: u64, lang: Lang) -> String {
    if secs >= 3600 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}{} {m:02}{}", lang.t("h", "ч"), lang.t("m", "м"))
    } else if secs >= 60 {
        let m = secs / 60;
        let s = secs % 60;
        format!("{m}{} {s:02}{}", lang.t("m", "м"), lang.t("s", "с"))
    } else {
        format!("{secs}{}", lang.t("s", "с"))
    }
}

/// Фирменный прогресс-бар KAIC Studio без наезда текста внутри полосы.
pub(super) fn draw_kaic_progress_bar(ui: &mut egui::Ui, frac: f32, state: &str, height: f32) {
    let pal = palette::p();
    let w = ui.available_width().max(80.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, height), egui::Sense::hover());
    let p = ui.painter();
    let radius = (height * 0.5).round() as u8;
    p.rect_filled(rect, radius, pal.surface_0);
    p.rect_stroke(
        rect,
        radius,
        Stroke::new(1.0, pal.line),
        egui::StrokeKind::Inside,
    );
    let clamped = if state == "validating" {
        1.0
    } else {
        frac.clamp(0.0, 1.0)
    };
    if clamped > 0.002 || state == "connecting" {
        let fill_w = if state == "connecting" && clamped < 0.03 {
            (rect.width() * 0.06).max(12.0)
        } else {
            (rect.width() * clamped).max(height)
        };
        let fill_rect = egui::Rect::from_min_size(
            rect.min,
            egui::vec2(fill_w.min(rect.width()), rect.height()),
        );
        let fill_col = match state {
            "ready" => pal.ceramic_200,
            "paused" => pal.line_strong,
            "error" => pal.err,
            _ => pal.brand,
        };
        p.rect_filled(fill_rect, radius, fill_col);
        if matches!(state, "downloading" | "connecting" | "validating") && fill_rect.width() > 14.0
        {
            let tip = egui::Rect::from_min_max(
                egui::pos2(fill_rect.right() - 10.0, fill_rect.top()),
                fill_rect.max,
            );
            p.rect_filled(tip, radius, pal.brand_text);
        }
    }
}

/// Рисует компактный бейдж (пилюлю) в палитре KAIC.
pub(super) fn draw_badge(
    ui: &mut egui::Ui,
    text: &str,
    text_col: Color32,
    fill_col: Color32,
    border_col: Color32,
) {
    egui::Frame::new()
        .fill(fill_col)
        .stroke(Stroke::new(1.0, border_col))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text)
                    .monospace()
                    .strong()
                    .size(10.5)
                    .color(text_col),
            );
        });
}

impl Studio {
    pub(super) fn open_downloads_tab(&mut self) {
        self.tab = Tab::Models;
        self.hf.tab = ModelsTab::Downloads;
        self.settings_open = false;
    }

    pub(super) fn open_discover_tab(&mut self) {
        self.tab = Tab::Models;
        self.hf.tab = ModelsTab::Discover;
        self.settings_open = false;
    }

    /// Возвращает краткую сводку активной загрузки HF для верхней панели (`(имя_квантизации, прогресс_0_1, скорость_байт_с)`).
    pub(super) fn active_download_badge(&self) -> Option<(String, f32, u64)> {
        let snap = self.hf.manager.snapshot();
        if !snap["active"].as_bool().unwrap_or(false) {
            return None;
        }
        let downloads = snap["downloads"].as_array()?;
        let job = downloads.iter().find(|j| {
            matches!(
                j["state"].as_str().unwrap_or(""),
                "connecting" | "downloading" | "validating"
            )
        })?;
        let fname = job["file"]["filename"].as_str().unwrap_or("GGUF");
        let repo_name = job["repo"]
            .as_str()
            .unwrap_or("")
            .split('/')
            .next_back()
            .unwrap_or(fname);
        let dl = job["downloaded"].as_u64().unwrap_or(0);
        let sz = job["file"]["size"].as_u64().unwrap_or(1).max(1);
        let frac = (dl as f32 / sz as f32).clamp(0.0, 1.0);
        let speed = job["speed_bps"].as_u64().unwrap_or(0);
        Some((repo_name.to_string(), frac, speed))
    }

    pub(super) fn refresh_models_catalog(&mut self) {
        let (mut models, warnings) = catalog::discover(std::path::Path::new(catalog::MODEL_ROOT));
        models.extend(hf::registered(&self.data));
        super::merge_media_models(&mut models, std::path::Path::new(catalog::MODEL_ROOT));
        if !self.hf.directory.is_empty() {
            super::merge_media_models(&mut models, std::path::Path::new(&self.hf.directory));
        }
        self.models = models;
        self.catalog_warnings = warnings;
    }

    /// Проверяет, установлена ли уже модель из репозитория `repo_id` (или конкретный `filename`).
    fn find_installed_model(
        &self,
        repo_id: &str,
        filename: Option<&str>,
    ) -> Option<&catalog::Model> {
        let base = filename.map(|f| f.rsplit('/').next().unwrap_or(f));
        self.models.iter().find(|m| {
            if let Some(b) = base {
                m.filename.eq_ignore_ascii_case(b)
            } else {
                m.repo.eq_ignore_ascii_case(repo_id)
            }
        })
    }

    pub(super) fn models_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let pal = palette::p();
        if !self.hf.initialized {
            self.hf.initialized = true;
            #[cfg(not(test))]
            self.hf.run_search(&ctx);
        }
        if self.hf.poll(&ctx) {
            self.refresh_models_catalog();
        }
        let snap = self.hf.manager.snapshot();
        let downloads = snap["downloads"].as_array().cloned().unwrap_or_default();
        let free_bytes = snap["free_bytes"].as_u64();
        let hf_active = snap["active"].as_bool().unwrap_or(false);
        if self.hf.was_hf_active && !hf_active {
            self.refresh_models_catalog();
        }
        self.hf.was_hf_active = hf_active;
        if hf_active {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        self.delete_model_dialog(&ctx);

        let lang = self.lang();
        let active_or_paused_count = downloads
            .iter()
            .filter(|j| j["state"].as_str() != Some("ready"))
            .count();

        // ── Верхняя панель вкладок (Поиск · Мои модели · Загрузки) ─────────
        ui.horizontal(|ui| {
            for (tab, label) in [
                (
                    ModelsTab::Discover,
                    lang.t("Discover (HF)", "Каталог Hugging Face").to_string(),
                ),
                (
                    ModelsTab::Local,
                    format!(
                        "{} ({})",
                        lang.t("My Models", "Мои модели"),
                        self.models.len()
                    ),
                ),
                (
                    ModelsTab::Downloads,
                    if active_or_paused_count > 0 {
                        format!(
                            "{} ({} · {})",
                            lang.t("Downloads", "Загрузки"),
                            active_or_paused_count,
                            if hf_active {
                                lang.t("active", "активна")
                            } else {
                                lang.t("paused", "пауза")
                            }
                        )
                    } else {
                        format!("{} ({})", lang.t("Downloads", "Загрузки"), downloads.len())
                    },
                ),
            ] {
                let sel = self.hf.tab == tab;
                let has_live = tab == ModelsTab::Downloads && hf_active;
                let btn =
                    egui::Button::new(RichText::new(label).size(13.0).strong().color(if sel {
                        pal.ink
                    } else if has_live {
                        pal.brand_text
                    } else {
                        pal.ink_muted
                    }))
                    .fill(if sel {
                        pal.surface_2
                    } else {
                        Color32::TRANSPARENT
                    })
                    .stroke(if sel {
                        Stroke::new(1.0, pal.brand)
                    } else if has_live {
                        Stroke::new(1.0, pal.line_strong)
                    } else {
                        Stroke::NONE
                    })
                    .corner_radius(7);
                if ui.add(btn).clicked() {
                    self.hf.tab = tab;
                    if tab == ModelsTab::Local {
                        self.refresh_models_catalog();
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(loaded) = self.loaded.clone() {
                    if ui
                        .add_enabled(!self.busy, egui::Button::new(lang.t("Unload", "Выгрузить")))
                        .clicked()
                    {
                        self.start(Job::Unload);
                    }
                    ui.label(RichText::new(loaded).size(12.0).color(pal.brand_text));
                }
                if let Some(free) = free_bytes {
                    ui.label(
                        RichText::new(format!(
                            "{}: {}",
                            lang.t("Free disk", "Свободно"),
                            lang.format_bytes(free)
                        ))
                        .monospace()
                        .size(11.5)
                        .color(pal.ink_muted),
                    );
                }
            });
        });
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(6.0);

        // Если идёт активная загрузка, а пользователь находится во вкладке Discover или My Models —
        // показываем информативный живой док-баннер загрузки сверху!
        if self.hf.tab != ModelsTab::Downloads {
            self.active_download_dock(ui, &ctx, &downloads);
        }

        match self.hf.tab {
            ModelsTab::Discover => self.discover_ui(ui, &ctx, &downloads, hf_active),
            ModelsTab::Local => self.local_models_ui(ui),
            ModelsTab::Downloads => self.downloads_ui(ui, &ctx, &downloads, hf_active, free_bytes),
        }
    }

    /// Живой баннер активной загрузки сверху экрана моделей.
    fn active_download_dock(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        downloads: &[Value],
    ) {
        let Some(job) = downloads.iter().find(|j| {
            matches!(
                j["state"].as_str().unwrap_or(""),
                "connecting" | "downloading" | "validating"
            )
        }) else {
            return;
        };
        let pal = palette::p();
        let lang = self.lang();
        let repo = job["repo"].as_str().unwrap_or("");
        let fname = job["file"]["filename"].as_str().unwrap_or("");
        let quant = parse_quant(fname);
        let st = job["state"].as_str().unwrap_or("downloading");
        let dl = job["downloaded"].as_u64().unwrap_or(0);
        let sz = job["file"]["size"].as_u64().unwrap_or(1).max(1);
        let speed = job["speed_bps"].as_u64().unwrap_or(0);
        let eta = job["eta_secs"].as_u64();
        let jid = job["id"].as_str().unwrap_or("").to_string();
        let frac = (dl as f32 / sz as f32).clamp(0.0, 1.0);
        let pct = (frac * 100.0).clamp(0.0, 100.0);

        let mut pause_id: Option<String> = None;
        let mut open_dl_tab = false;

        egui::Frame::new()
            .fill(pal.surface_1)
            .stroke(Stroke::new(1.0, pal.brand))
            .corner_radius(9)
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    anim::pulse_dot(ui, pal.brand, true, true);
                    draw_badge(
                        ui,
                        lang.t("ACTIVE DOWNLOAD", "АКТИВНАЯ ЗАГРУЗКА"),
                        pal.on_brand,
                        pal.brand,
                        pal.brand,
                    );
                    draw_badge(ui, &quant, pal.ink, pal.surface_2, pal.line_strong);
                    ui.label(RichText::new(repo).strong().size(13.5).color(pal.ink));
                    ui.add(
                        egui::Label::new(
                            RichText::new(fname)
                                .monospace()
                                .size(11.5)
                                .color(pal.ink_muted),
                        )
                        .truncate(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(lang.t("Open Downloads", "Экран загрузок"))
                            .clicked()
                        {
                            open_dl_tab = true;
                        }
                        if ui.button(lang.t("Pause", "Пауза")).clicked() {
                            pause_id = Some(jid);
                        }
                    });
                });
                ui.add_space(6.0);
                draw_kaic_progress_bar(ui, frac, st, 8.0);
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("{pct:.1}%"))
                            .monospace()
                            .strong()
                            .size(12.5)
                            .color(pal.brand_text),
                    );
                    ui.label(
                        RichText::new(format!(
                            "{} / {}",
                            lang.format_bytes(dl),
                            lang.format_bytes(sz)
                        ))
                        .monospace()
                        .size(12.0)
                        .color(pal.ink),
                    );
                    ui.label(
                        RichText::new(format!("· {}", dl_label(st, lang)))
                            .size(12.0)
                            .color(pal.ink_muted),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(rem) = eta {
                            ui.label(
                                RichText::new(format!(
                                    "{}: ~{}",
                                    lang.t("ETA", "Осталось"),
                                    format_eta(rem, lang)
                                ))
                                .monospace()
                                .size(12.0)
                                .color(pal.ink_muted),
                            );
                        }
                        let spd = format_speed(speed, lang);
                        if !spd.is_empty() {
                            ui.label(
                                RichText::new(spd)
                                    .monospace()
                                    .strong()
                                    .size(12.0)
                                    .color(pal.ceramic_200),
                            );
                        }
                    });
                });
            });
        ui.add_space(8.0);

        if let Some(id) = pause_id {
            self.hf
                .spawn_api(ctx.clone(), "/api/hf/cancel", json!({ "id": id }), |r| {
                    HfReply::Action(r.map(|_| ()))
                });
        }
        if open_dl_tab {
            self.hf.tab = ModelsTab::Downloads;
        }
    }

    fn discover_ui(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        downloads: &[Value],
        hf_active: bool,
    ) {
        let pal = palette::p();
        let lang = self.lang();
        let mut trigger_search = false;

        // Строка 1: широкий поиск + Найти
        ui.horizontal(|ui| {
            let input_w = (ui.available_width() - 102.0).max(180.0);
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.hf.query)
                    .hint_text(lang.t(
                        "Search Hugging Face GGUF or paste owner/repo…",
                        "Поиск GGUF на Hugging Face или вставьте owner/repo…",
                    ))
                    .margin(egui::Margin::symmetric(10, 6))
                    .desired_width(input_w),
            );
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                trigger_search = true;
            }
            let search_btn = egui::Button::new(
                RichText::new(if self.hf.searching {
                    lang.t("Searching…", "Поиск…")
                } else {
                    lang.t("Search", "Найти")
                })
                .color(pal.on_brand)
                .strong(),
            )
            .fill(pal.brand)
            .stroke(Stroke::NONE)
            .min_size(egui::vec2(86.0, 30.0));
            if ui.add_enabled(!self.hf.searching, search_btn).clicked() {
                trigger_search = true;
            }
        });

        // Строка 2: пресеты семейств + сортировка + опциональный фильтр «Только чат-LLM»
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);
            for (label, q) in [
                (lang.t("Popular", "Популярные"), ""),
                ("Qwen", "Qwen"),
                ("Gemma", "Gemma"),
                ("Llama", "Llama"),
                ("Mistral", "Mistral"),
                ("DeepSeek", "DeepSeek"),
                ("Granite", "Granite"),
                ("Phi", "Phi"),
                ("Coder", "Coder"),
                ("Vision", "VL"),
                (lang.t("Image", "Фото"), "FLUX"),
                (lang.t("Video", "Видео"), "T2V"),
            ] {
                let active = self.hf.query.eq_ignore_ascii_case(q);
                let btn = egui::Button::new(RichText::new(label).size(12.0).color(if active {
                    pal.ink
                } else {
                    pal.ink_muted
                }))
                .fill(if active { pal.surface_2 } else { pal.surface_1 })
                .stroke(Stroke::new(1.0, if active { pal.brand } else { pal.line }))
                .corner_radius(12);
                if ui.add_enabled(!self.hf.searching, btn).clicked() {
                    self.hf.query = q.to_string();
                    trigger_search = true;
                }
            }
            ui.add_space(6.0);
            egui::ComboBox::from_id_salt("hf-sort")
                .selected_text(match self.hf.sort {
                    "best" => lang.t("Recommended", "Рекомендуемые"),
                    "likes" => lang.t("Most likes", "По лайкам"),
                    "trendingScore" => lang.t("Trending", "В тренде"),
                    "lastModified" => lang.t("Recent", "Недавние"),
                    _ => lang.t("Most downloads", "По скачиваниям"),
                })
                .width(156.0)
                .show_ui(ui, |ui| {
                    for (k, lbl) in [
                        ("best", lang.t("Recommended", "Рекомендуемые")),
                        ("downloads", lang.t("Most downloads", "По скачиваниям")),
                        ("likes", lang.t("Most likes", "По лайкам")),
                        ("trendingScore", lang.t("Trending", "В тренде")),
                        ("lastModified", lang.t("Recent", "Недавние")),
                    ] {
                        if ui.selectable_value(&mut self.hf.sort, k, lbl).changed() {
                            trigger_search = true;
                        }
                    }
                });
            if ui
                .checkbox(
                    &mut self.hf.only_llm,
                    RichText::new(lang.t("Chat LLMs only", "Только чат-LLM")).size(12.0),
                )
                .changed()
            {
                trigger_search = true;
            }
        });

        if let Some(err) = self.hf.error.clone() {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&err).color(pal.err).size(12.5));
                if ui.small_button(lang.t("Retry", "Повторить")).clicked() {
                    trigger_search = true;
                }
                if err.contains("429")
                    && ui
                        .small_button(
                            lang.t("Set HF_TOKEN in Settings", "Указать HF_TOKEN в настройках"),
                        )
                        .clicked()
                {
                    self.settings_open = true;
                }
            });
        }

        if trigger_search {
            self.hf.run_search(ctx);
        }

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(6.0);

        // ── Двухпанельный сплит ───────────────────────────────────────────
        let avail = ui.available_size();
        let left_w = (avail.x * 0.34).clamp(250.0, 360.0);
        let right_w = (avail.x - left_w - 14.0).max(260.0);
        let col_h = (avail.y - 8.0).max(220.0);
        let mut pick_repo: Option<HfRepo> = None;
        let mut use_installed_in_chat: Option<String> = None;

        ui.horizontal_top(|ui| {
            // ── ЛЕВАЯ КОЛОНКА: список карточек репозиториев ──
            ui.vertical(|ui| {
                ui.set_width(left_w);
                ui.set_max_width(left_w);
                egui::Frame::new()
                    .fill(pal.surface_1)
                    .stroke(Stroke::new(1.0, pal.line))
                    .corner_radius(8)
                    .inner_margin(6)
                    .show(ui, |ui| {
                        ui.set_width(left_w - 14.0);
                        ui.set_min_height(col_h);
                        egui::ScrollArea::vertical()
                            .id_salt("hf-repos-list")
                            .max_height(col_h)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.vertical(|ui| {
                                    ui.set_width(ui.available_width());
                                    if self.hf.searching && self.hf.repos.is_empty() {
                                        ui.horizontal(|ui| {
                                            ui.spinner();
                                            ui.label(lang.t(
                                                "Loading Hugging Face catalog…",
                                                "Загрузка каталога Hugging Face…",
                                            ));
                                        });
                                        return;
                                    }
                                    if self.hf.repos.is_empty() {
                                        ui.label(
                                            RichText::new(lang.t(
                                                "No models found",
                                                "Модели не найдены",
                                            ))
                                            .color(pal.ink_muted)
                                            .size(13.0),
                                        );
                                        return;
                                    }
                                    let selected_id =
                                        self.hf.selected.as_ref().map(|s| s.id.as_str());
                                    for repo in &self.hf.repos {
                                        let is_sel = selected_id == Some(repo.id.as_str());
                                        let trusted = is_trusted_author(&repo.id);
                                        let (owner, name) = repo
                                            .id
                                            .split_once('/')
                                            .unwrap_or(("huggingface", repo.id.as_str()));
                                        let repo_dl = downloads.iter().find(|j| {
                                            j["repo"].as_str() == Some(repo.id.as_str())
                                                && j["state"].as_str() != Some("ready")
                                        });
                                        let repo_installed =
                                            self.find_installed_model(&repo.id, None).is_some()
                                                || downloads.iter().any(|j| {
                                                    j["repo"].as_str() == Some(repo.id.as_str())
                                                        && j["state"].as_str() == Some("ready")
                                                });
                                        let card_w = ui.available_width();
                                        let card_id = ui.id().with(("hf-card", &repo.id));
                                        let was_hovered = ui
                                            .ctx()
                                            .data_mut(|d| d.get_temp::<bool>(card_id))
                                            .unwrap_or(false);
                                        let hover_t = if ui.input(|i| i.time) > 0.0 {
                                            ui.ctx().animate_bool_with_time(
                                                card_id.with("anim"),
                                                was_hovered && !is_sel,
                                                0.12,
                                            )
                                        } else {
                                            0.0
                                        };
                                        let card_fill = if is_sel {
                                            pal.surface_2
                                        } else {
                                            anim::lerp_color(
                                                Color32::TRANSPARENT,
                                                anim::with_alpha(pal.surface_2, 160),
                                                hover_t,
                                            )
                                        };
                                        let card_stroke = if is_sel {
                                            pal.brand
                                        } else {
                                            anim::lerp_color(
                                                Color32::TRANSPARENT,
                                                pal.line,
                                                hover_t,
                                            )
                                        };
                                        let resp = egui::Frame::new()
                                            .fill(card_fill)
                                            .stroke(Stroke::new(1.0, card_stroke))
                                            .corner_radius(7)
                                            .inner_margin(egui::Margin {
                                                left: 10,
                                                right: 8,
                                                top: 8,
                                                bottom: 8,
                                            })
                                            .show(ui, |ui| {
                                                ui.vertical(|ui| {
                                                    ui.set_width((card_w - 20.0).max(120.0));
                                                    ui.horizontal(|ui| {
                                                        ui.label(
                                                            RichText::new(owner)
                                                            .size(11.0)
                                                            .color(if trusted {
                                                                pal.brand_text
                                                            } else {
                                                                pal.ink_muted
                                                            }),
                                                        );
                                                        ui.with_layout(
                                                            egui::Layout::right_to_left(
                                                                egui::Align::Center,
                                                            ),
                                                            |ui| {
                                                                let kind = catalog::classify_model(
                                                                    &repo.id,
                                                                    &repo.tags.join(" "),
                                                                );
                                                                let (badge, col) = match kind {
                                                                    catalog::ModelKind::Image => {
                                                                        ("IMAGE", pal.brand_text)
                                                                    }
                                                                    catalog::ModelKind::Video => {
                                                                        ("VIDEO", pal.brand_text)
                                                                    }
                                                                    catalog::ModelKind::Text => {
                                                                        ("LLM", pal.ink_muted)
                                                                    }
                                                                };
                                                                ui.label(
                                                                    RichText::new(badge)
                                                                        .monospace()
                                                                        .size(10.0)
                                                                        .color(col),
                                                                );
                                                                if let Some(p) =
                                                                    extract_param_badge(name)
                                                                {
                                                                    ui.label(
                                                                        RichText::new(p)
                                                                            .monospace()
                                                                            .strong()
                                                                            .size(10.5)
                                                                            .color(pal.ceramic_200),
                                                                    );
                                                                }
                                                            },
                                                        );
                                                    });
                                                    ui.add(
                                                        egui::Label::new(
                                                            RichText::new(name)
                                                                .strong()
                                                                .size(13.0)
                                                                .color(pal.ink),
                                                        )
                                                        .truncate(),
                                                    );
                                                    ui.horizontal(|ui| {
                                                        if repo.downloads > 0 {
                                                            ui.label(
                                                                RichText::new(format!(
                                                                    "↓ {}",
                                                                    format_count(repo.downloads)
                                                                ))
                                                                .monospace()
                                                                .size(11.0)
                                                                .color(pal.ink_muted),
                                                            );
                                                        }
                                                        if repo.likes > 0 {
                                                            ui.label(
                                                                RichText::new(format!(
                                                                    "♥ {}",
                                                                    format_count(repo.likes)
                                                                ))
                                                                .monospace()
                                                                .size(11.0)
                                                                .color(pal.ink_muted),
                                                            );
                                                        }
                                                        ui.with_layout(
                                                            egui::Layout::right_to_left(
                                                                egui::Align::Center,
                                                            ),
                                                            |ui| {
                                                                if let Some(j) = repo_dl {
                                                                    let dl = j["downloaded"]
                                                                        .as_u64()
                                                                        .unwrap_or(0);
                                                                    let sz = j["file"]["size"]
                                                                        .as_u64()
                                                                        .unwrap_or(1)
                                                                        .max(1);
                                                                    let pct = ((dl as f32
                                                                        / sz as f32)
                                                                        * 100.0)
                                                                        .round()
                                                                        as u32;
                                                                    let st = j["state"]
                                                                        .as_str()
                                                                        .unwrap_or("");
                                                                    ui.label(
                                                                        RichText::new(if matches!(
                                                                            st,
                                                                            "downloading"
                                                                                | "connecting"
                                                                                | "validating"
                                                                        ) {
                                                                            format!("↓ {pct}%")
                                                                        } else {
                                                                            format!("II {pct}%")
                                                                        })
                                                                        .monospace()
                                                                        .strong()
                                                                        .size(10.5)
                                                                        .color(pal.brand_text),
                                                                    );
                                                                } else if repo_installed {
                                                                    ui.label(
                                                                        RichText::new(lang.t(
                                                                            "Installed",
                                                                            "Установлена",
                                                                        ))
                                                                        .monospace()
                                                                        .size(10.5)
                                                                        .color(pal.ceramic_200),
                                                                    );
                                                                } else if repo.last_modified.len()
                                                                    >= 10
                                                                {
                                                                    ui.label(
                                                                        RichText::new(
                                                                            &repo.last_modified
                                                                                [..10],
                                                                        )
                                                                        .monospace()
                                                                        .size(10.5)
                                                                        .color(pal.ink_muted),
                                                                    );
                                                                }
                                                            },
                                                        );
                                                    });
                                                });
                                            })
                                            .response
                                            .interact(egui::Sense::click());
                                        ui.ctx()
                                            .data_mut(|d| d.insert_temp(card_id, resp.hovered()));
                                        if is_sel {
                                            let bar = egui::Rect::from_min_max(
                                                egui::pos2(
                                                    resp.rect.left() + 1.0,
                                                    resp.rect.top() + 8.0,
                                                ),
                                                egui::pos2(
                                                    resp.rect.left() + 3.5,
                                                    resp.rect.bottom() - 8.0,
                                                ),
                                            );
                                            ui.painter().rect_filled(bar, 2, pal.brand);
                                        }
                                        if resp.clicked() {
                                            pick_repo = Some(repo.clone());
                                        }
                                        ui.add_space(4.0);
                                    }
                                });
                            });
                    });
            });

            ui.add_space(8.0);

            // ── ПРАВАЯ КОЛОНКА: инспектор модели и карточки квантизаций GGUF ──
            ui.vertical(|ui| {
                ui.set_width(right_w);
                ui.set_max_width(right_w);
                egui::Frame::new()
                    .fill(pal.surface_0)
                    .stroke(Stroke::new(1.0, pal.line))
                    .corner_radius(8)
                    .inner_margin(12)
                    .show(ui, |ui| {
                        ui.set_width((right_w - 26.0).max(200.0));
                        ui.set_min_height(col_h);
                        let Some(sel) = self.hf.selected.clone() else {
                            ui.centered_and_justified(|ui| {
                                ui.label(
                                    RichText::new(lang.t(
                                        "Select a model on the left to inspect GGUF files and quantizations",
                                        "Выберите модель слева, чтобы посмотреть GGUF-файлы и квантизации",
                                    ))
                                    .color(pal.ink_muted),
                                );
                            });
                            return;
                        };
                        let (owner, name) =
                            sel.id.split_once('/').unwrap_or(("", sel.id.as_str()));
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(owner).size(11.5).color(
                                            if is_trusted_author(&sel.id) {
                                                pal.brand_text
                                            } else {
                                                pal.ink_muted
                                            },
                                        ),
                                    );
                                    if is_trusted_author(&sel.id) {
                                        ui.label(
                                            RichText::new(lang.t(
                                                "· verified quantizer",
                                                "· проверенный квантизатор",
                                            ))
                                            .size(11.0)
                                            .color(pal.ink_muted),
                                        );
                                    }
                                    if let Some(p) = extract_param_badge(name) {
                                        draw_badge(
                                            ui,
                                            &p,
                                            pal.ceramic_200,
                                            pal.surface_2,
                                            pal.line_strong,
                                        );
                                    }
                                });
                                ui.label(
                                    RichText::new(name).strong().size(18.0).color(pal.ink),
                                );
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui
                                    .small_button(lang.t("Copy URL", "Копировать URL"))
                                    .on_hover_text(lang.t(
                                        "Copy Hugging Face link",
                                        "Скопировать ссылку на Hugging Face",
                                    ))
                                    .clicked()
                                {
                                    ctx.copy_text(format!("https://huggingface.co/{}", sel.id));
                                }
                                if sel.likes > 0 {
                                    ui.label(
                                        RichText::new(format!("♥ {}", format_count(sel.likes)))
                                            .size(12.0)
                                            .color(pal.ink_muted),
                                    );
                                }
                                if sel.downloads > 0 {
                                    ui.label(
                                        RichText::new(format!(
                                            "↓ {}",
                                            format_count(sel.downloads)
                                        ))
                                        .size(12.0)
                                        .color(pal.ink_muted),
                                    );
                                }
                            });
                        });

                        if !sel.tags.is_empty() {
                            ui.add_space(4.0);
                            ui.horizontal_wrapped(|ui| {
                                for tag in sel.tags.iter().take(6) {
                                    egui::Frame::new()
                                        .fill(pal.surface_1)
                                        .stroke(Stroke::new(1.0, pal.line))
                                        .corner_radius(4)
                                        .inner_margin(egui::Margin::symmetric(6, 1))
                                        .show(ui, |ui| {
                                            ui.label(
                                                RichText::new(tag)
                                                    .monospace()
                                                    .size(10.5)
                                                    .color(pal.ink_muted),
                                            );
                                        });
                                }
                            });
                        }

                        let mut start_file: Option<String> = None;
                        let mut cancel_id: Option<String> = None;
                        let mut resume_id: Option<String> = None;
                        let mut remove_id: Option<String> = None;

                        // ── Карточка рекомендуемой квантизации (Быстрый выбор в 1 клик) ──
                        if !self.hf.loading_files
                            && let Some(rec_file) = pick_recommended_file(&self.hf.files)
                        {
                            let rec_quant = parse_quant(&rec_file.filename);
                            let (fit_text, fit_col, fit_bar, _, fit_frac) =
                                fit_hint(rec_file.size, lang);
                            let rec_installed = self
                                .find_installed_model(&sel.id, Some(&rec_file.filename))
                                .map(|m| m.selection_id());
                            let rec_job = downloads.iter().find(|j| {
                                j["repo"].as_str() == Some(sel.id.as_str())
                                    && j["file"]["filename"].as_str()
                                        == Some(rec_file.filename.as_str())
                            });

                            ui.add_space(8.0);
                            egui::Frame::new()
                                .fill(pal.surface_1)
                                .stroke(Stroke::new(1.0, pal.brand))
                                .corner_radius(8)
                                .inner_margin(egui::Margin::symmetric(12, 9))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.vertical(|ui| {
                                            ui.horizontal_wrapped(|ui| {
                                                draw_badge(
                                                    ui,
                                                    lang.t("RECOMMENDED BUILD", "РЕКОМЕНДУЕМАЯ СБОРКА"),
                                                    pal.on_brand,
                                                    pal.brand,
                                                    pal.brand,
                                                );
                                                draw_badge(
                                                    ui,
                                                    &rec_quant,
                                                    pal.ink,
                                                    pal.surface_2,
                                                    pal.line_strong,
                                                );
                                                ui.label(
                                                    RichText::new(lang.format_bytes(rec_file.size))
                                                        .monospace()
                                                        .strong()
                                                        .size(12.5)
                                                        .color(pal.ink),
                                                );
                                                vram_bar(ui, fit_frac, fit_bar);
                                                ui.label(
                                                    RichText::new(fit_text)
                                                        .size(11.5)
                                                        .color(fit_col),
                                                );
                                            });
                                            ui.label(
                                                RichText::new(format!(
                                                    "{} · {}",
                                                    quant_quality_hint(&rec_quant, lang),
                                                    rec_file.filename
                                                ))
                                                .size(11.5)
                                                .color(pal.ink_muted),
                                            );
                                        });
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if let Some(sel_id) = rec_installed {
                                                    let btn = egui::Button::new(
                                                        RichText::new(lang.t(
                                                            "Open in Chat",
                                                            "Открыть в диалоге",
                                                        ))
                                                        .color(pal.on_brand)
                                                        .strong(),
                                                    )
                                                    .fill(pal.brand)
                                                    .stroke(Stroke::NONE);
                                                    if ui.add(btn).clicked() {
                                                        use_installed_in_chat = Some(sel_id);
                                                    }
                                                } else if let Some(j) = rec_job {
                                                    let st = j["state"].as_str().unwrap_or("");
                                                    let jid =
                                                        j["id"].as_str().unwrap_or("").to_string();
                                                    if matches!(
                                                        st,
                                                        "connecting" | "downloading" | "validating"
                                                    ) {
                                                        if ui
                                                            .button(lang.t("Pause", "Пауза"))
                                                            .clicked()
                                                        {
                                                            cancel_id = Some(jid);
                                                        }
                                                    } else if st != "ready"
                                                        && ui
                                                            .add_enabled(
                                                                !hf_active,
                                                                egui::Button::new(
                                                                    RichText::new(lang.t(
                                                                        "Resume",
                                                                        "Продолжить",
                                                                    ))
                                                                    .color(pal.on_brand)
                                                                    .strong(),
                                                                )
                                                                .fill(pal.brand)
                                                                .stroke(Stroke::NONE),
                                                            )
                                                            .clicked()
                                                    {
                                                        resume_id = Some(jid);
                                                    }
                                                } else {
                                                    let btn = egui::Button::new(
                                                        RichText::new(format!(
                                                            "{} {} ({})",
                                                            lang.t("Download", "Скачать"),
                                                            rec_quant,
                                                            lang.format_bytes(rec_file.size)
                                                        ))
                                                        .color(pal.on_brand)
                                                        .strong(),
                                                    )
                                                    .fill(pal.brand)
                                                    .stroke(Stroke::NONE);
                                                    if ui.add_enabled(!hf_active, btn).clicked() {
                                                        start_file =
                                                            Some(rec_file.filename.clone());
                                                    }
                                                }
                                            },
                                        );
                                    });
                                });
                        }

                        ui.add_space(8.0);
                        // Подпанель фильтров квантизаций
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "{} ({})",
                                    lang.t("All quantizations", "Все варианты GGUF"),
                                    self.hf.files.len()
                                ))
                                .size(12.5)
                                .strong()
                                .color(pal.ink),
                            );
                            for (scope, lbl) in [
                                (QuantScope::All, lang.t("All", "Все")),
                                (
                                    QuantScope::Recommended,
                                    lang.t("Recommended", "Рекомендуемые"),
                                ),
                                (
                                    QuantScope::FitsGpu,
                                    lang.t("Fits 8 GB GPU", "Влезет в 8 ГБ GPU"),
                                ),
                            ] {
                                let act = self.hf.quant_scope == scope;
                                let btn = egui::Button::new(
                                    RichText::new(lbl)
                                        .size(11.5)
                                        .color(if act { pal.ink } else { pal.ink_muted }),
                                )
                                .fill(if act {
                                    pal.surface_2
                                } else {
                                    Color32::TRANSPARENT
                                })
                                .stroke(Stroke::new(
                                    1.0,
                                    if act { pal.brand } else { pal.line },
                                ))
                                .corner_radius(10);
                                if ui.add(btn).clicked() {
                                    self.hf.quant_scope = scope;
                                }
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.hf.quant_filter)
                                        .hint_text(lang.t("Filter (Q4_K_M…)", "Фильтр (Q4_K_M…)"))
                                        .margin(egui::Margin::symmetric(8, 5))
                                        .desired_width(156.0),
                                );
                            });
                        });
                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(4.0);

                        let qf = self.hf.quant_filter.trim().to_lowercase();

                        egui::ScrollArea::vertical()
                            .id_salt("hf-files-list")
                            .max_height((col_h - 175.0).max(140.0))
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.vertical(|ui| {
                                    ui.set_width(ui.available_width());
                                    if self.hf.loading_files {
                                        ui.horizontal(|ui| {
                                            ui.spinner();
                                            ui.label(lang.t(
                                                "Fetching GGUF files…",
                                                "Получение списка GGUF…",
                                            ));
                                        });
                                        return;
                                    }
                                    let mut any = false;
                                    for f in &self.hf.files {
                                        if !qf.is_empty()
                                            && !f.filename.to_lowercase().contains(&qf)
                                        {
                                            continue;
                                        }
                                        let quant = parse_quant(&f.filename);
                                        let rec = is_recommended(&quant);
                                        let (
                                            fit_text,
                                            fit_text_color,
                                            fit_bar_color,
                                            fits_gpu,
                                            fit_frac,
                                        ) = fit_hint(f.size, lang);
                                        if self.hf.quant_scope == QuantScope::Recommended && !rec {
                                            continue;
                                        }
                                        if self.hf.quant_scope == QuantScope::FitsGpu && !fits_gpu {
                                            continue;
                                        }
                                        any = true;
                                        let installed_model = self
                                            .find_installed_model(&sel.id, Some(&f.filename))
                                            .cloned();
                                        let job = downloads.iter().find(|j| {
                                            j["repo"].as_str() == Some(sel.id.as_str())
                                                && j["file"]["filename"].as_str()
                                                    == Some(f.filename.as_str())
                                        });
                                        let is_ready_job = job
                                            .is_some_and(|j| j["state"].as_str() == Some("ready"));
                                        let is_active_job = job.is_some_and(|j| {
                                            matches!(
                                                j["state"].as_str().unwrap_or(""),
                                                "connecting" | "downloading" | "validating"
                                            )
                                        });
                                        egui::Frame::new()
                                            .fill(pal.surface_1)
                                            .stroke(Stroke::new(
                                                1.0,
                                                if is_active_job {
                                                    pal.brand
                                                } else if installed_model.is_some() || is_ready_job {
                                                    pal.line_strong
                                                } else {
                                                    pal.line
                                                },
                                            ))
                                            .corner_radius(7)
                                            .inner_margin(10)
                                            .show(ui, |ui| {
                                                ui.horizontal(|ui| {
                                                    ui.vertical(|ui| {
                                                        ui.set_max_width(
                                                            (ui.available_width() - 210.0)
                                                                .max(140.0),
                                                        );
                                                        ui.horizontal_wrapped(|ui| {
                                                            draw_badge(
                                                                ui,
                                                                &quant,
                                                                pal.ink,
                                                                pal.surface_2,
                                                                if rec {
                                                                    pal.brand
                                                                } else {
                                                                    pal.line
                                                                },
                                                            );
                                                            ui.label(
                                                                RichText::new(
                                                                    lang.format_bytes(f.size),
                                                                )
                                                                .monospace()
                                                                .strong()
                                                                .size(12.5)
                                                                .color(pal.ink),
                                                            );
                                                            vram_bar(ui, fit_frac, fit_bar_color);
                                                            ui.label(
                                                                RichText::new(fit_text)
                                                                    .size(11.5)
                                                                    .color(fit_text_color),
                                                            );
                                                            ui.label(
                                                                RichText::new(format!(
                                                                    "· {}",
                                                                    quant_quality_hint(&quant, lang)
                                                                ))
                                                                .size(11.5)
                                                                .color(pal.ink_muted),
                                                            );
                                                        });
                                                        ui.add(
                                                            egui::Label::new(
                                                                RichText::new(&f.filename)
                                                                    .monospace()
                                                                    .size(11.0)
                                                                    .color(pal.ink_muted),
                                                            )
                                                            .truncate(),
                                                        );
                                                    });
                                                    ui.with_layout(
                                                        egui::Layout::right_to_left(
                                                            egui::Align::Center,
                                                        ),
                                                        |ui| {
                                                            if installed_model.is_some()
                                                                || is_ready_job
                                                            {
                                                                if let Some(m) =
                                                                    installed_model.clone()
                                                                {
                                                                    let del_btn = egui::Button::new(
                                                                        RichText::new(lang.t(
                                                                            "Delete",
                                                                            "Удалить",
                                                                        ))
                                                                        .color(pal.err),
                                                                    );
                                                                    if ui
                                                                        .add_enabled(
                                                                            !self.busy, del_btn,
                                                                        )
                                                                        .clicked()
                                                                    {
                                                                        self.hf
                                                                            .confirm_delete_model =
                                                                            Some(m.clone());
                                                                    }
                                                                } else if let Some(j) = job {
                                                                    let jid = j["id"]
                                                                        .as_str()
                                                                        .unwrap_or("")
                                                                        .to_string();
                                                                    let del_btn = egui::Button::new(
                                                                        RichText::new(lang.t(
                                                                            "Delete",
                                                                            "Удалить",
                                                                        ))
                                                                        .color(pal.err),
                                                                    );
                                                                    if ui.add(del_btn).clicked() {
                                                                        remove_id = Some(jid);
                                                                    }
                                                                }
                                                                let sel_id = installed_model
                                                                    .as_ref()
                                                                    .map(|m| m.selection_id())
                                                                    .unwrap_or_else(|| {
                                                                        let base = f
                                                                            .filename
                                                                            .rsplit('/')
                                                                            .next()
                                                                            .unwrap_or(&f.filename);
                                                                        format!(
                                                                            "{}::{base}",
                                                                            sel.id
                                                                        )
                                                                    });
                                                                if ui
                                                                    .button(lang.t(
                                                                        "Open in Chat",
                                                                        "В диалог",
                                                                    ))
                                                                    .clicked()
                                                                {
                                                                    use_installed_in_chat =
                                                                        Some(sel_id);
                                                                }
                                                                draw_badge(
                                                                    ui,
                                                                    lang.t(
                                                                        "INSTALLED",
                                                                        "УСТАНОВЛЕНА",
                                                                    ),
                                                                    pal.ceramic_200,
                                                                    pal.surface_2,
                                                                    pal.line_strong,
                                                                );
                                                            } else if let Some(j) = job {
                                                                let st = j["state"]
                                                                    .as_str()
                                                                    .unwrap_or("");
                                                                let jid = j["id"]
                                                                    .as_str()
                                                                    .unwrap_or("")
                                                                    .to_string();
                                                                if matches!(
                                                                    st,
                                                                    "connecting"
                                                                        | "downloading"
                                                                        | "validating"
                                                                ) {
                                                                    if ui
                                                                        .button(
                                                                            lang.t("Pause", "Пауза"),
                                                                        )
                                                                        .clicked()
                                                                    {
                                                                        cancel_id = Some(jid);
                                                                    }
                                                                } else {
                                                                    if ui
                                                                        .add(
                                                                            egui::Button::new(
                                                                                RichText::new("×")
                                                                                    .color(pal.err),
                                                                            ),
                                                                        )
                                                                        .on_hover_text(lang.t(
                                                                            "Remove download",
                                                                            "Удалить загрузку",
                                                                        ))
                                                                        .clicked()
                                                                    {
                                                                        remove_id =
                                                                            Some(jid.clone());
                                                                    }
                                                                    if ui
                                                                        .add_enabled(
                                                                            !hf_active,
                                                                            egui::Button::new(
                                                                                lang.t(
                                                                                    "Resume",
                                                                                    "Продолжить",
                                                                                ),
                                                                            ),
                                                                        )
                                                                        .clicked()
                                                                    {
                                                                        resume_id = Some(jid);
                                                                    }
                                                                }
                                                            } else {
                                                                let dl_lbl =
                                                                    lang.t("Download", "Скачать");
                                                                let btn = if rec {
                                                                    egui::Button::new(
                                                                        RichText::new(dl_lbl)
                                                                            .color(pal.on_brand)
                                                                            .strong(),
                                                                    )
                                                                    .fill(pal.brand)
                                                                    .stroke(Stroke::NONE)
                                                                } else {
                                                                    egui::Button::new(dl_lbl)
                                                                };
                                                                if ui
                                                                    .add_enabled(!hf_active, btn)
                                                                    .clicked()
                                                                {
                                                                    start_file =
                                                                        Some(f.filename.clone());
                                                                }
                                                            }
                                                        },
                                                    );
                                                });

                                                // Полноразмерный прогресс-бар и телеметрия скорости для активной/приостановленной загрузки
                                                if let Some(j) = job {
                                                    let st = j["state"].as_str().unwrap_or("");
                                                    if st != "ready" {
                                                        let dl = j["downloaded"]
                                                            .as_u64()
                                                            .unwrap_or(0);
                                                        let sz = j["file"]["size"]
                                                            .as_u64()
                                                            .unwrap_or(f.size)
                                                            .max(1);
                                                        let speed =
                                                            j["speed_bps"].as_u64().unwrap_or(0);
                                                        let eta = j["eta_secs"].as_u64();
                                                        let frac =
                                                            (dl as f32 / sz as f32).clamp(0.0, 1.0);
                                                        ui.add_space(6.0);
                                                        draw_kaic_progress_bar(ui, frac, st, 7.0);
                                                        ui.add_space(3.0);
                                                        ui.horizontal(|ui| {
                                                            ui.label(
                                                                RichText::new(format!(
                                                                    "{:.1}%",
                                                                    frac * 100.0
                                                                ))
                                                                .monospace()
                                                                .strong()
                                                                .size(11.5)
                                                                .color(pal.brand_text),
                                                            );
                                                            ui.label(
                                                                RichText::new(format!(
                                                                    "{} / {} · {}",
                                                                    lang.format_bytes(dl),
                                                                    lang.format_bytes(sz),
                                                                    dl_label(st, lang)
                                                                ))
                                                                .monospace()
                                                                .size(11.5)
                                                                .color(pal.ink_muted),
                                                            );
                                                            ui.with_layout(
                                                                egui::Layout::right_to_left(
                                                                    egui::Align::Center,
                                                                ),
                                                                |ui| {
                                                                    if let Some(rem) = eta {
                                                                        ui.label(
                                                                            RichText::new(format!(
                                                                                "ETA ~{}",
                                                                                format_eta(
                                                                                    rem, lang
                                                                                )
                                                                            ))
                                                                            .monospace()
                                                                            .size(11.5)
                                                                            .color(pal.ink_muted),
                                                                        );
                                                                    }
                                                                    let spd =
                                                                        format_speed(speed, lang);
                                                                    if !spd.is_empty() {
                                                                        ui.label(
                                                                            RichText::new(spd)
                                                                                .monospace()
                                                                                .strong()
                                                                                .size(11.5)
                                                                                .color(
                                                                                    pal.ceramic_200,
                                                                                ),
                                                                        );
                                                                    }
                                                                },
                                                            );
                                                        });
                                                        if let Some(err) = j["error"].as_str() {
                                                            ui.label(
                                                                RichText::new(err)
                                                                    .size(11.5)
                                                                    .color(pal.err),
                                                            );
                                                        }
                                                    }
                                                }
                                            });
                                        ui.add_space(4.0);
                                    }
                                    if !any {
                                        ui.label(
                                            RichText::new(lang.t(
                                                "No single-file GGUF models match the current filter.",
                                                "Подходящих цельных GGUF-файлов по фильтру не найдено.",
                                            ))
                                            .color(pal.ink_muted),
                                        );
                                    }
                                });
                            });

                        if let Some(filename) = start_file {
                            self.hf.spawn_api(
                                ctx.clone(),
                                "/api/hf/start",
                                json!({
                                    "repo": sel.id,
                                    "filename": filename,
                                    "directory": self.hf.directory,
                                }),
                                |r| HfReply::Action(r.map(|_| ())),
                            );
                        }
                        if let Some(id) = cancel_id {
                            self.hf.spawn_api(
                                ctx.clone(),
                                "/api/hf/cancel",
                                json!({ "id": id }),
                                |r| HfReply::Action(r.map(|_| ())),
                            );
                        }
                        if let Some(id) = resume_id {
                            self.hf.spawn_api(
                                ctx.clone(),
                                "/api/hf/resume",
                                json!({ "id": id }),
                                |r| HfReply::Action(r.map(|_| ())),
                            );
                        }
                        if let Some(id) = remove_id {
                            self.hf.spawn_api(
                                ctx.clone(),
                                "/api/hf/remove",
                                json!({ "id": id, "delete_file": true }),
                                |r| HfReply::Action(r.map(|_| ())),
                            );
                        }
                    });
            });
        });

        if let Some(repo) = pick_repo {
            self.hf.load_files(ctx, repo);
        }
        if let Some(sel_id) = use_installed_in_chat {
            self.refresh_models_catalog();
            self.select_model(&sel_id);
            self.tab = Tab::Chat;
        }
    }

    fn local_models_ui(&mut self, ui: &mut egui::Ui) {
        let pal = palette::p();
        let lang = self.lang();
        let total_bytes: u64 = self.models.iter().map(|m| m.size).sum();
        let text_count = self
            .models
            .iter()
            .filter(|m| m.kind() == catalog::ModelKind::Text)
            .count();
        let img_count = self
            .models
            .iter()
            .filter(|m| m.kind() == catalog::ModelKind::Image)
            .count();
        let vid_count = self
            .models
            .iter()
            .filter(|m| m.kind() == catalog::ModelKind::Video)
            .count();

        // ── Панель фильтрации, типов и сортировки локальных моделей ────────
        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.hf.local_filter)
                    .hint_text(lang.t("Filter local models…", "Фильтр локальных моделей…"))
                    .margin(egui::Margin::symmetric(10, 6))
                    .desired_width(240.0),
            );
            for (filter, label) in [
                (
                    LocalKindFilter::All,
                    format!("{} ({})", lang.t("All", "Все"), self.models.len()),
                ),
                (LocalKindFilter::Text, format!("LLM ({text_count})")),
                (
                    LocalKindFilter::Image,
                    format!("{} ({img_count})", lang.t("Image", "Фото")),
                ),
                (
                    LocalKindFilter::Video,
                    format!("{} ({vid_count})", lang.t("Video", "Видео")),
                ),
            ] {
                let act = self.hf.local_kind == filter;
                let btn = egui::Button::new(RichText::new(label).size(12.0).color(if act {
                    pal.ink
                } else {
                    pal.ink_muted
                }))
                .fill(if act { pal.surface_2 } else { pal.surface_1 })
                .stroke(Stroke::new(1.0, if act { pal.brand } else { pal.line }))
                .corner_radius(12);
                if ui.add(btn).clicked() {
                    self.hf.local_kind = filter;
                }
            }
            egui::ComboBox::from_id_salt("local-models-sort")
                .selected_text(match self.hf.local_sort {
                    LocalSort::Recommended => lang.t("Active & Fastest", "Активная и быстрые"),
                    LocalSort::Speed => lang.t("Speed (tok/s)", "По скорости (tok/s)"),
                    LocalSort::SizeAsc => lang.t("Size (smallest)", "Размер (по возрастанию)"),
                    LocalSort::SizeDesc => lang.t("Size (largest)", "Размер (по убыванию)"),
                    LocalSort::Name => lang.t("Name (A–Z)", "По имени (A–Z)"),
                })
                .width(172.0)
                .show_ui(ui, |ui| {
                    for (s, lbl) in [
                        (
                            LocalSort::Recommended,
                            lang.t("Active & Fastest", "Активная и быстрые"),
                        ),
                        (
                            LocalSort::Speed,
                            lang.t("Speed (tok/s)", "По скорости (tok/s)"),
                        ),
                        (
                            LocalSort::SizeAsc,
                            lang.t("Size (smallest)", "Размер (по возрастанию)"),
                        ),
                        (
                            LocalSort::SizeDesc,
                            lang.t("Size (largest)", "Размер (по убыванию)"),
                        ),
                        (LocalSort::Name, lang.t("Name (A–Z)", "По имени (A–Z)")),
                    ] {
                        ui.selectable_value(&mut self.hf.local_sort, s, lbl);
                    }
                });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(
                        !self.busy,
                        egui::Button::new(lang.t("Refresh catalog", "Обновить каталог")),
                    )
                    .clicked()
                {
                    self.refresh_models_catalog();
                }
                ui.label(
                    RichText::new(format!(
                        "{}: {}",
                        lang.t("Total", "Всего"),
                        lang.format_bytes(total_bytes)
                    ))
                    .monospace()
                    .size(12.0)
                    .color(pal.ink_muted),
                );
            });
        });

        for w in &self.catalog_warnings {
            ui.label(RichText::new(w).color(pal.brand_text).size(12.0));
        }
        ui.add_space(8.0);

        let q = self.hf.local_filter.trim().to_lowercase();
        let selected_repo = self.state.settings.selected_repo.clone();
        let mut indices: Vec<usize> = (0..self.models.len())
            .filter(|&i| {
                let m = &self.models[i];
                let kind_ok = match self.hf.local_kind {
                    LocalKindFilter::All => true,
                    LocalKindFilter::Text => m.kind() == catalog::ModelKind::Text,
                    LocalKindFilter::Image => m.kind() == catalog::ModelKind::Image,
                    LocalKindFilter::Video => m.kind() == catalog::ModelKind::Video,
                };
                let query_ok = q.is_empty()
                    || m.display_name().to_lowercase().contains(&q)
                    || m.filename.to_lowercase().contains(&q)
                    || m.repo.to_lowercase().contains(&q);
                kind_ok && query_ok
            })
            .collect();

        indices.sort_by(|&a, &b| {
            let ma = &self.models[a];
            let mb = &self.models[b];
            let sa = ma.selection_id() == selected_repo || ma.repo == selected_repo;
            let sb = mb.selection_id() == selected_repo || mb.repo == selected_repo;
            match self.hf.local_sort {
                LocalSort::Recommended => sb
                    .cmp(&sa)
                    .then_with(|| {
                        mb.generation_tps
                            .unwrap_or(0.0)
                            .total_cmp(&ma.generation_tps.unwrap_or(0.0))
                    })
                    .then_with(|| ma.display_name().cmp(&mb.display_name())),
                LocalSort::Speed => mb
                    .generation_tps
                    .unwrap_or(0.0)
                    .total_cmp(&ma.generation_tps.unwrap_or(0.0)),
                LocalSort::SizeAsc => ma.size.cmp(&mb.size),
                LocalSort::SizeDesc => mb.size.cmp(&ma.size),
                LocalSort::Name => ma
                    .display_name()
                    .to_lowercase()
                    .cmp(&mb.display_name().to_lowercase()),
            }
        });

        let mut open_in_chat: Option<String> = None;
        let mut ask_delete: Option<catalog::Model> = None;

        egui::ScrollArea::vertical()
            .id_salt("локальные-модели-список")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if indices.is_empty() {
                    ui.label(
                        RichText::new(lang.t(
                            "No local models match the current filter.",
                            "Нет локальных моделей, подходящих под фильтр.",
                        ))
                        .color(pal.ink_muted)
                        .size(13.0),
                    );
                    return;
                }
                for idx in indices {
                    let m = &self.models[idx];
                    ui.push_id(&m.path, |ui| {
                        let is_selected = self.state.settings.selected_repo == m.selection_id()
                            || self.state.settings.selected_repo == m.repo;
                        let is_loaded = self.loaded.as_deref().is_some_and(|l| {
                            l == m.alias() || l == m.repo || l == m.selection_id()
                        });
                        let quant = parse_quant(&m.filename);
                        let (fit_text, fit_col, fit_bar, _, fit_frac) = fit_hint(m.size, lang);

                        let frame_resp = egui::Frame::new()
                            .inner_margin(egui::Margin {
                                left: 14,
                                right: 12,
                                top: 10,
                                bottom: 10,
                            })
                            .fill(if is_selected {
                                pal.surface_2
                            } else {
                                pal.surface_1
                            })
                            .stroke(Stroke::new(
                                1.0,
                                if is_selected { pal.brand } else { pal.line },
                            ))
                            .corner_radius(8)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        ui.set_max_width((ui.available_width() - 185.0).max(180.0));
                                        ui.horizontal_wrapped(|ui| {
                                            ui.label(
                                                RichText::new(m.display_name())
                                                    .strong()
                                                    .size(14.5)
                                                    .color(pal.ink),
                                            );
                                            if is_selected {
                                                draw_badge(
                                                    ui,
                                                    lang.t("ACTIVE", "АКТИВНА"),
                                                    pal.on_brand,
                                                    pal.brand,
                                                    pal.brand,
                                                );
                                            }
                                            if is_loaded {
                                                draw_badge(
                                                    ui,
                                                    lang.t("IN VRAM", "В ПАМЯТИ"),
                                                    pal.ceramic_200,
                                                    pal.surface_1,
                                                    pal.brand,
                                                );
                                            }
                                            let (kind_tag, kind_col) = match m.kind() {
                                                catalog::ModelKind::Video => {
                                                    ("VIDEO", pal.brand_text)
                                                }
                                                catalog::ModelKind::Image => {
                                                    ("IMAGE", pal.brand_text)
                                                }
                                                catalog::ModelKind::Text => ("LLM", pal.ink_muted),
                                            };
                                            draw_badge(
                                                ui,
                                                kind_tag,
                                                kind_col,
                                                pal.surface_0,
                                                pal.line,
                                            );
                                            draw_badge(
                                                ui,
                                                &quant,
                                                pal.ink,
                                                pal.surface_0,
                                                pal.line_strong,
                                            );
                                        });
                                        ui.add_space(3.0);
                                        ui.horizontal_wrapped(|ui| {
                                            ui.label(
                                                RichText::new(lang.format_bytes(m.size))
                                                    .monospace()
                                                    .strong()
                                                    .size(12.0)
                                                    .color(pal.ink),
                                            );
                                            vram_bar(ui, fit_frac, fit_bar);
                                            ui.label(
                                                RichText::new(fit_text).size(11.5).color(fit_col),
                                            );
                                            if let Some(tps) = m.generation_tps {
                                                ui.label(
                                                    RichText::new(format!(
                                                        "· {tps:.1} {}",
                                                        lang.t("tok/s", "t/c")
                                                    ))
                                                    .monospace()
                                                    .size(11.5)
                                                    .color(pal.ceramic_200),
                                                );
                                            }
                                            ui.label(
                                                RichText::new(format!("· {}", m.filename))
                                                    .monospace()
                                                    .size(11.0)
                                                    .color(pal.ink_muted),
                                            );
                                        });
                                    });
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            let del_btn = egui::Button::new(
                                                RichText::new(lang.t("Delete", "Удалить"))
                                                    .color(pal.err),
                                            );
                                            if ui
                                                .add_enabled(!self.busy, del_btn)
                                                .on_hover_text(lang.t(
                                                    "Delete model file from disk",
                                                    "Удалить файл модели с диска",
                                                ))
                                                .clicked()
                                            {
                                                ask_delete = Some(m.clone());
                                            }
                                            let chat_btn = egui::Button::new(
                                                RichText::new(lang.t("Open in Chat", "В диалог"))
                                                    .color(if is_selected {
                                                        pal.on_brand
                                                    } else {
                                                        pal.ink
                                                    })
                                                    .strong(),
                                            )
                                            .fill(if is_selected {
                                                pal.brand
                                            } else {
                                                pal.surface_2
                                            })
                                            .stroke(Stroke::new(
                                                1.0,
                                                if is_selected {
                                                    pal.brand
                                                } else {
                                                    pal.line_strong
                                                },
                                            ));
                                            if ui
                                                .add_enabled(m.loadable() && !self.busy, chat_btn)
                                                .clicked()
                                            {
                                                open_in_chat = Some(m.selection_id());
                                            }
                                        },
                                    );
                                });
                            });
                        if is_selected {
                            let r = frame_resp.response.rect;
                            let bar = egui::Rect::from_min_max(
                                egui::pos2(r.left() + 1.0, r.top() + 8.0),
                                egui::pos2(r.left() + 4.0, r.bottom() - 8.0),
                            );
                            ui.painter().rect_filled(bar, 2, pal.brand);
                        }
                        ui.add_space(6.0);
                    });
                }
            });
        if let Some(m) = ask_delete {
            self.hf.confirm_delete_model = Some(m);
        }
        if let Some(repo) = open_in_chat {
            self.select_model(&repo);
            self.tab = Tab::Chat;
        }
    }

    fn delete_model_dialog(&mut self, ctx: &egui::Context) {
        let Some(model) = self.hf.confirm_delete_model.clone() else {
            return;
        };
        let pal = palette::p();
        let lang = self.lang();
        let mut confirm = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("удалить-локальную-модель")).show(ctx, |ui| {
            ui.set_max_width(400.0);
            ui.heading(
                RichText::new(lang.t("Delete model from disk?", "Удалить модель с диска?"))
                    .color(pal.ink)
                    .strong(),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!(
                    "{} ({})",
                    model.display_name(),
                    lang.format_bytes(model.size)
                ))
                .strong()
                .color(pal.ink),
            );
            ui.label(
                RichText::new(model.path.display().to_string())
                    .monospace()
                    .size(11.5)
                    .color(pal.ink_muted),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(lang.t(
                    "The model file will be permanently removed from disk.",
                    "Файл модели будет безвозвратно удалён с диска.",
                ))
                .size(12.5)
                .color(pal.ink_muted),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(lang.t("Cancel", "Отмена")).clicked() {
                    cancel = true;
                }
                let del_btn = egui::Button::new(
                    RichText::new(lang.t("Delete model", "Удалить модель"))
                        .color(pal.on_brand)
                        .strong(),
                )
                .fill(pal.err)
                .stroke(Stroke::NONE);
                if ui.add_enabled(!self.busy, del_btn).clicked() {
                    confirm = true;
                }
            });
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                cancel = true;
            }
        });
        if cancel {
            self.hf.confirm_delete_model = None;
        } else if confirm {
            self.hf.confirm_delete_model = None;
            let was_loaded = self.loaded.as_deref().is_some_and(|l| {
                l == model.alias() || l == model.repo || l == model.selection_id()
            });
            if was_loaded && !self.busy {
                self.start(Job::Unload);
            }
            let _ = self.hf.manager.remove_by_path(&model.path);
            match catalog::delete_model_from_disk(std::path::Path::new(catalog::MODEL_ROOT), &model)
            {
                Ok(()) => {
                    self.refresh_models_catalog();
                    if self.state.settings.selected_repo == model.selection_id()
                        || self.state.settings.selected_repo == model.repo
                    {
                        self.state.settings.selected_repo = catalog::default_model(&self.models)
                            .map(|m| m.selection_id())
                            .unwrap_or_default();
                        self.mark_dirty();
                    }
                    self.status = format!(
                        "{}: {}",
                        lang.t("Deleted model", "Модель удалена"),
                        model.filename
                    );
                }
                Err(e) => {
                    self.error = Some(format!("{e:#}"));
                }
            }
        }
    }

    fn downloads_ui(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        downloads: &[Value],
        hf_active: bool,
        free_bytes: Option<u64>,
    ) {
        let pal = palette::p();
        let lang = self.lang();
        let mut cancel_id = None;
        let mut resume_id = None;
        let mut remove_id: Option<(String, bool)> = None;
        let mut open_in_chat: Option<String> = None;

        let active_cnt = downloads
            .iter()
            .filter(|j| {
                matches!(
                    j["state"].as_str().unwrap_or(""),
                    "connecting" | "downloading" | "validating"
                )
            })
            .count();
        let paused_cnt = downloads
            .iter()
            .filter(|j| matches!(j["state"].as_str().unwrap_or(""), "paused" | "error"))
            .count();
        let ready_cnt = downloads
            .iter()
            .filter(|j| j["state"].as_str() == Some("ready"))
            .count();

        // ── Верхняя сводная карточка менеджера загрузок ──────────────────
        egui::Frame::new()
            .fill(pal.surface_1)
            .stroke(Stroke::new(
                1.0,
                if active_cnt > 0 { pal.brand } else { pal.line },
            ))
            .corner_radius(8)
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(lang.t("Download Manager", "Менеджер загрузок GGUF"))
                            .strong()
                            .size(14.5)
                            .color(pal.ink),
                    );
                    draw_badge(
                        ui,
                        &format!("{}: {active_cnt}", lang.t("ACTIVE", "АКТИВНО")),
                        if active_cnt > 0 {
                            pal.on_brand
                        } else {
                            pal.ink_muted
                        },
                        if active_cnt > 0 {
                            pal.brand
                        } else {
                            pal.surface_2
                        },
                        if active_cnt > 0 { pal.brand } else { pal.line },
                    );
                    draw_badge(
                        ui,
                        &format!("{}: {paused_cnt}", lang.t("PAUSED", "В ОЧЕРЕДИ / ПАУЗА")),
                        pal.ink,
                        pal.surface_2,
                        pal.line,
                    );
                    draw_badge(
                        ui,
                        &format!("{}: {ready_cnt}", lang.t("COMPLETED", "ЗАВЕРШЕНО")),
                        pal.ceramic_200,
                        pal.surface_2,
                        pal.line,
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(lang.t("+ Discover Models", "+ Найти модели"))
                            .clicked()
                        {
                            self.hf.tab = ModelsTab::Discover;
                        }
                        if let Some(free) = free_bytes {
                            ui.label(
                                RichText::new(format!(
                                    "{}: {}",
                                    lang.t("Free space", "Свободно на диске"),
                                    lang.format_bytes(free)
                                ))
                                .monospace()
                                .size(12.0)
                                .color(pal.ceramic_200),
                            );
                        }
                        ui.label(
                            RichText::new(format!(
                                "{}: {}",
                                lang.t("Folder", "Папка"),
                                self.hf.directory
                            ))
                            .monospace()
                            .size(11.5)
                            .color(pal.ink_muted),
                        );
                    });
                });
            });
        ui.add_space(10.0);

        egui::ScrollArea::vertical()
            .id_salt("история-загрузок-hf")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if downloads.is_empty() {
                    egui::Frame::new()
                        .fill(pal.surface_1)
                        .stroke(Stroke::new(1.0, pal.line))
                        .corner_radius(8)
                        .inner_margin(24)
                        .show(ui, |ui| {
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new(lang.t(
                                        "No active or past downloads",
                                        "Нет активных или завершённых загрузок",
                                    ))
                                    .strong()
                                    .size(14.5)
                                    .color(pal.ink),
                                );
                                ui.add_space(4.0);
                                ui.label(
                                    RichText::new(lang.t(
                                        "Open Discover (HF) to pick a GGUF model and download it with SHA256 verification and automatic resume.",
                                        "Откройте «Каталог Hugging Face», чтобы выбрать GGUF-модель и скачать её с проверкой SHA256 и докачкой.",
                                    ))
                                    .size(12.5)
                                    .color(pal.ink_muted),
                                );
                                ui.add_space(10.0);
                                let btn = egui::Button::new(
                                    RichText::new(lang.t("Browse Hugging Face", "Открыть каталог моделей"))
                                        .color(pal.on_brand)
                                        .strong(),
                                )
                                .fill(pal.brand)
                                .stroke(Stroke::NONE);
                                if ui.add(btn).clicked() {
                                    self.hf.tab = ModelsTab::Discover;
                                }
                            });
                        });
                    return;
                }

                // Сортируем: сначала активные, затем на паузе/ошибке, затем готовые
                let mut ordered: Vec<&Value> = downloads.iter().collect();
                ordered.sort_by_key(|j| match j["state"].as_str().unwrap_or("") {
                    "downloading" | "connecting" | "validating" => 0,
                    "paused" | "error" => 1,
                    _ => 2,
                });

                for j in ordered {
                    let repo = j["repo"].as_str().unwrap_or("");
                    let (owner, model_name) = repo.split_once('/').unwrap_or(("", repo));
                    let fname = j["file"]["filename"].as_str().unwrap_or("");
                    let quant = parse_quant(fname);
                    let st = j["state"].as_str().unwrap_or("");
                    let dl = j["downloaded"].as_u64().unwrap_or(0);
                    let sz = j["file"]["size"].as_u64().unwrap_or(1).max(1);
                    let speed = j["speed_bps"].as_u64().unwrap_or(0);
                    let eta = j["eta_secs"].as_u64();
                    let jid = j["id"].as_str().unwrap_or("").to_string();
                    let frac = (dl as f32 / sz as f32).clamp(0.0, 1.0);
                    let pct = (frac * 100.0).clamp(0.0, 100.0);
                    let is_active = matches!(st, "connecting" | "downloading" | "validating");
                    let is_ready = st == "ready";

                    egui::Frame::new()
                        .fill(pal.surface_1)
                        .stroke(Stroke::new(
                            1.0,
                            if is_active {
                                pal.brand
                            } else if is_ready {
                                pal.line_strong
                            } else {
                                pal.line
                            },
                        ))
                        .corner_radius(9)
                        .inner_margin(egui::Margin::symmetric(14, 12))
                        .show(ui, |ui| {
                            // Верхняя строка: бейдж статуса, автор, название модели, квантизация и кнопки действий
                            ui.horizontal(|ui| {
                                if is_active {
                                    anim::pulse_dot(ui, pal.brand, true, true);
                                }
                                let (st_badge, st_fg, st_bg, st_border) = match st {
                                    "downloading" | "connecting" => (
                                        lang.t("DOWNLOADING", "СКАЧИВАНИЕ"),
                                        pal.on_brand,
                                        pal.brand,
                                        pal.brand,
                                    ),
                                    "validating" => (
                                        lang.t("VERIFYING SHA256", "ПРОВЕРКА SHA256"),
                                        pal.on_brand,
                                        pal.brand,
                                        pal.brand,
                                    ),
                                    "ready" => (
                                        lang.t("READY", "ГОТОВА"),
                                        pal.ceramic_200,
                                        pal.surface_2,
                                        pal.line_strong,
                                    ),
                                    "error" => (
                                        lang.t("ERROR", "ОШИБКА"),
                                        pal.err,
                                        pal.surface_2,
                                        pal.err,
                                    ),
                                    _ => (
                                        lang.t("PAUSED", "ПАУЗА"),
                                        pal.ink_muted,
                                        pal.surface_2,
                                        pal.line,
                                    ),
                                };
                                draw_badge(ui, st_badge, st_fg, st_bg, st_border);
                                draw_badge(ui, &quant, pal.ink, pal.surface_2, pal.line_strong);
                                if !owner.is_empty() {
                                    ui.label(
                                        RichText::new(owner)
                                            .size(12.0)
                                            .color(pal.brand_text),
                                    );
                                    ui.label(RichText::new("/").color(pal.ink_muted));
                                }
                                ui.label(
                                    RichText::new(model_name)
                                        .strong()
                                        .size(14.5)
                                        .color(pal.ink),
                                );

                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if !is_active {
                                            let rem_lbl = if is_ready {
                                                lang.t("Delete file", "Удалить файл")
                                            } else {
                                                lang.t("Cancel & Delete", "Удалить")
                                            };
                                            let rem_btn = egui::Button::new(
                                                RichText::new(rem_lbl).color(pal.err),
                                            );
                                            if ui.add(rem_btn).clicked() {
                                                remove_id = Some((jid.clone(), true));
                                            }
                                        }
                                        if is_active {
                                            if ui.button(lang.t("Pause", "Пауза")).clicked() {
                                                cancel_id = Some(jid.clone());
                                            }
                                        } else if is_ready {
                                            let base_fname =
                                                fname.rsplit('/').next().unwrap_or(fname);
                                            let sel_id = format!("{repo}::{base_fname}");
                                            let chat_btn = egui::Button::new(
                                                RichText::new(lang.t(
                                                    "Open in Chat",
                                                    "Открыть в диалоге",
                                                ))
                                                .color(pal.on_brand)
                                                .strong(),
                                            )
                                            .fill(pal.brand)
                                            .stroke(Stroke::NONE);
                                            if ui.add(chat_btn).clicked() {
                                                open_in_chat = Some(sel_id);
                                            }
                                        } else {
                                            let res_btn = egui::Button::new(
                                                RichText::new(lang.t("Resume", "Продолжить"))
                                                    .color(pal.on_brand)
                                                    .strong(),
                                            )
                                            .fill(pal.brand)
                                            .stroke(Stroke::NONE);
                                            if ui.add_enabled(!hf_active, res_btn).clicked() {
                                                resume_id = Some(jid.clone());
                                            }
                                        }
                                    },
                                );
                            });

                            ui.add_space(4.0);
                            ui.label(
                                RichText::new(fname)
                                    .monospace()
                                    .size(11.5)
                                    .color(pal.ink_muted),
                            );

                            ui.add_space(6.0);
                            draw_kaic_progress_bar(
                                ui,
                                if is_ready { 1.0 } else { frac },
                                st,
                                9.0,
                            );
                            ui.add_space(5.0);

                            // Нижняя строка телеметрии
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "{:.1}%",
                                        if is_ready { 100.0 } else { pct }
                                    ))
                                    .monospace()
                                    .strong()
                                    .size(13.0)
                                    .color(if is_ready {
                                        pal.ceramic_200
                                    } else {
                                        pal.brand_text
                                    }),
                                );
                                ui.label(
                                    RichText::new(format!(
                                        "{} / {}",
                                        lang.format_bytes(if is_ready { sz } else { dl }),
                                        lang.format_bytes(sz)
                                    ))
                                    .monospace()
                                    .strong()
                                    .size(12.5)
                                    .color(pal.ink),
                                );
                                ui.label(
                                    RichText::new(format!("· {}", dl_label(st, lang)))
                                        .size(12.0)
                                        .color(pal.ink_muted),
                                );

                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if is_active {
                                            if let Some(rem) = eta {
                                                ui.label(
                                                    RichText::new(format!(
                                                        "{}: ~{}",
                                                        lang.t("Remaining", "Осталось"),
                                                        format_eta(rem, lang)
                                                    ))
                                                    .monospace()
                                                    .size(12.0)
                                                    .color(pal.ink_muted),
                                                );
                                            }
                                            let spd = format_speed(speed, lang);
                                            if !spd.is_empty() {
                                                ui.label(
                                                    RichText::new(spd)
                                                        .monospace()
                                                        .strong()
                                                        .size(12.5)
                                                        .color(pal.ceramic_200),
                                                );
                                            }
                                        } else if is_ready {
                                            if let Some(sha) = j["actual_sha256"].as_str() {
                                                ui.label(
                                                    RichText::new(format!(
                                                        "SHA256: {}…",
                                                        &sha[..sha.len().min(12)]
                                                    ))
                                                    .monospace()
                                                    .size(11.0)
                                                    .color(pal.ink_muted),
                                                );
                                            }
                                        } else if dl > 0 {
                                            ui.label(
                                                RichText::new(lang.t(
                                                    "Resumable .part saved on disk",
                                                    "Часть .part сохранена · готова к докачке",
                                                ))
                                                .size(11.5)
                                                .color(pal.ink_muted),
                                            );
                                        }
                                    },
                                );
                            });

                            if let Some(err) = j["error"].as_str() {
                                ui.add_space(4.0);
                                ui.label(RichText::new(err).color(pal.err).size(12.0));
                            }
                        });
                    ui.add_space(8.0);
                }
            });

        if let Some(id) = cancel_id {
            self.hf
                .spawn_api(ctx.clone(), "/api/hf/cancel", json!({ "id": id }), |r| {
                    HfReply::Action(r.map(|_| ()))
                });
        }
        if let Some(id) = resume_id {
            self.hf
                .spawn_api(ctx.clone(), "/api/hf/resume", json!({ "id": id }), |r| {
                    HfReply::Action(r.map(|_| ()))
                });
        }
        if let Some((id, delete_file)) = remove_id {
            self.hf.spawn_api(
                ctx.clone(),
                "/api/hf/remove",
                json!({ "id": id, "delete_file": delete_file }),
                |r| HfReply::Action(r.map(|_| ())),
            );
        }
        if let Some(sel_id) = open_in_chat {
            self.refresh_models_catalog();
            self.select_model(&sel_id);
            self.tab = Tab::Chat;
        }
    }
}
