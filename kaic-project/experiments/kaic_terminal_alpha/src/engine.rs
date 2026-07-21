//! Загрузка модели и генерация текста.
//!
//! Вся низкоуровневая логика decode/sample/batch — прямое продолжение
//! того, что уже проверено и работает в experiments/llama_cpp_smoke,
//! включая исправленный индекс сэмплинга:
//!     sampler.sample(&ctx, batch.n_tokens() - 1)
//! Здесь она НЕ меняется, только оборачивается в функции с логами через
//! logger::log(), чтобы её можно было переиспользовать в чат-цикле.
//!
//! Сознательное упрощение для альфы: KV-cache НЕ переиспользуется между
//! репликами диалога. На каждую реплику — ctx.clear_kv_cache() и полный
//! передекод всей истории с нуля. Это проще и предсказуемее инкрементального
//! кэширования позиций между ходами; если станет реальной проблемой
//! скорости на длинных диалогах — это отдельная, осознанная оптимизация
//! на будущее, не то, что нужно решать в первой альфе.

use std::num::NonZeroU32;
use std::time::Instant;

use anyhow::{Context, Result};
use llama_cpp_4::prelude::*;

use crate::chat::ChatHistory;
use crate::config::Config;
use crate::logger;

pub fn init_backend() -> Result<LlamaBackend> {
    LlamaBackend::init().context("не удалось инициализировать backend llama.cpp")
}

pub fn load_model(backend: &LlamaBackend, cfg: &Config) -> Result<LlamaModel> {
    logger::log(&cfg.log_path, &format!("model loading... path={}", cfg.model_path));
    let started = Instant::now();
    let params = LlamaModelParams::default().with_n_gpu_layers(cfg.n_gpu_layers);
    let model = LlamaModel::load_from_file(backend, &cfg.model_path, &params)
        .context("не удалось загрузить модель")?;
    logger::log(
        &cfg.log_path,
        &format!("model loaded за {:.2?}", started.elapsed()),
    );
    Ok(model)
}

pub struct Engine<'a> {
    model: &'a LlamaModel,
    ctx: LlamaContext<'a>,
    cfg: Config,
}

impl<'a> Engine<'a> {
    pub fn new(backend: &'a LlamaBackend, model: &'a LlamaModel, cfg: Config) -> Result<Self> {
        logger::log(&cfg.log_path, "context creating...");
        let ctx_params =
            LlamaContextParams::default().with_n_ctx(NonZeroU32::new(cfg.n_ctx));
        let ctx = model
            .new_context(backend, ctx_params)
            .context("не удалось создать контекст")?;
        logger::log(&cfg.log_path, "context created");
        Ok(Engine { model, ctx, cfg })
    }

    pub fn model_path(&self) -> &str {
        &self.cfg.model_path
    }

    pub fn temperature(&self) -> f32 {
        self.cfg.temperature
    }

    pub fn set_temperature(&mut self, t: f32) {
        self.cfg.temperature = t;
    }

    /// Полный цикл: история -> prompt -> decode -> генерация до eog/max_tokens.
    /// Возвращает сгенерированный текст ответа ассистента.
    pub fn generate(&mut self, history: &ChatHistory) -> Result<String> {
        let log_path = self.cfg.log_path.clone();

        // Каждую реплику начинаем с чистого KV-cache — см. пояснение в шапке файла.
        self.ctx.clear_kv_cache();

        let messages = history.to_llama_messages()?;
        let prompt = self
            .model
            .apply_chat_template(None, &messages, true)
            .context("не удалось применить chat template")?;

        let tokens = self
            .model
            .str_to_token(&prompt, AddBos::Always)
            .context("не удалось токенизировать промпт")?;
        let n_prompt = tokens.len();
        logger::log(&log_path, &format!("prompt tokens: {n_prompt}"));

        let mut batch = LlamaBatch::new(self.cfg.n_ctx as usize, 1);
        for (i, &tok) in tokens.iter().enumerate() {
            let is_last = i == n_prompt - 1;
            batch
                .add(tok, i as i32, &[0], is_last)
                .context("batch.add() промпта упал")?;
        }

        let decode_started = Instant::now();
        self.ctx
            .decode(&mut batch)
            .context("ошибка decode() промпта")?;
        logger::log(
            &log_path,
            &format!("decode(prompt) завершён за {:.2?}", decode_started.elapsed()),
        );

        let sampler = LlamaSampler::chain_simple([
            LlamaSampler::temp(self.cfg.temperature),
            LlamaSampler::dist(0),
        ]);

        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let mut pos = n_prompt as i32;
        let mut reply = String::new();
        let mut generated_count: usize = 0;

        logger::log(&log_path, "генерация начата...");
        let gen_started = Instant::now();

        for _step in 0..self.cfg.max_tokens {
            let sample_idx = batch.n_tokens() - 1; // raw-позиция последнего токена с logits=true
            let token = sampler.sample(&self.ctx, sample_idx);

            if self.model.is_eog_token(token) {
                break;
            }

            let bytes = self
                .model
                .token_to_bytes(token, Special::Plaintext)
                .context("не удалось получить байты токена")?;
            let mut piece = String::new();
            decoder.decode_to_string(&bytes, &mut piece, false);
            reply.push_str(&piece);
            print!("{piece}");
            use std::io::Write;
            std::io::stdout().flush().ok();

            generated_count += 1;

            batch.clear();
            batch
                .add(token, pos, &[0], true)
                .context("batch.add() при генерации упал")?;
            self.ctx
                .decode(&mut batch)
                .context("ошибка decode() при генерации")?;

            pos += 1;
        }

        println!();

        let elapsed = gen_started.elapsed();
        let tok_per_sec = if elapsed.as_secs_f64() > 0.0 {
            generated_count as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };
        logger::log(
            &log_path,
            &format!(
                "генерация завершена: создано токенов={generated_count}, время={:.2?}, скорость={tok_per_sec:.2} tok/s",
                elapsed
            ),
        );

        Ok(reply)
    }
}
