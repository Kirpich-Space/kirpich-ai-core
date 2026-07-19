//! Изолированный smoke-test `llama-cpp-4`.
//!
//! Не часть KAIC. Не использует Scheduler, Router, TaskStore, trait
//! ModelBackend — ничего из основного проекта. Единственная цель:
//! проверить своими руками, а не по документации, что:
//!
//! 1. библиотека реально собирается;
//! 2. CUDA реально работает (при сборке с --features cuda);
//! 3. GGUF реально открывается;
//! 4. chat template применяется автоматически;
//! 5. генерация проходит без сюрпризов.
//!
//! Запуск:
//!   cargo run -- /путь/к/модели.gguf
//!   cargo run -- /путь/к/модели.gguf "свой вопрос модели"
//!   cargo run --features cuda -- /путь/к/модели.gguf
//!
//! Если результат этого теста — успешная, разумная по скорости
//! генерация с правильно отформатированным промптом — тогда имеет смысл
//! начинать `EmbeddedBackend`. Если что-то здесь не заработает — дешевле
//! узнать об этом сейчас, на 150 строках, чем после того, как это будет
//! обёрнуто в trait и интегрировано в Scheduler.

use std::env;
use std::num::NonZeroU32;
use std::time::Instant;

use anyhow::{Context, Result};
use llama_cpp_4::prelude::*;

const CTX_TOKENS: u32 = 4096;
const MAX_RESPONSE_TOKENS: usize = 512;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let model_path = args.get(1).context(
        "укажи путь к .gguf первым аргументом: cargo run -- /path/to/model.gguf [\"вопрос\"]",
    )?;
    let prompt_text = args
        .get(2)
        .map(String::as_str)
        .unwrap_or("Привет! Расскажи в двух предложениях, кто ты.");

    // --- Шаг 1: библиотека реально собирается и инициализируется ---
    println!("Инициализация backend...");
    let backend =
        LlamaBackend::init().context("не удалось инициализировать llama.cpp backend")?;

    // --- Шаг 2: GGUF реально открывается ---
    println!("Загрузка модели из {model_path}...");
    let load_started = Instant::now();
    let model = LlamaModel::load_from_file(&backend, model_path, &LlamaModelParams::default())
        .context("не удалось загрузить модель")?;
    println!("Модель загружена за {:.2?}", load_started.elapsed());

    // --- Шаг 3: chat template применяется автоматически ---
    let messages = vec![LlamaChatMessage::new("user".into(), prompt_text.to_string())
        .context("не удалось построить сообщение чата")?];

    let prompt = model
        .apply_chat_template(None, messages, true)
        .context("не удалось применить chat template модели")?;

    println!("--- Промпт после apply_chat_template (проверь глазами формат под свою модель) ---");
    println!("{prompt}");
    println!("--- Конец промпта ---\n");

    // --- Шаг 4: генерация проходит без сюрпризов ---
    let ctx_params = LlamaContextParams::default().with_n_ctx(NonZeroU32::new(CTX_TOKENS));
    let mut ctx = model
        .new_context(&backend, ctx_params)
        .context("не удалось создать контекст генерации")?;

    let tokens = model
        .str_to_token(&prompt, AddBos::Always)
        .context("не удалось токенизировать промпт")?;
    let n_prompt = tokens.len();

    let mut batch = LlamaBatch::new(CTX_TOKENS as usize, 1);
    for (i, &tok) in tokens.iter().enumerate() {
        batch.add(tok, i as i32, &[0], i == n_prompt - 1)?;
    }

    println!("Генерация...\n");
    let generation_started = Instant::now();
    ctx.decode(&mut batch).context("ошибка decode() промпта")?;

    let sampler = LlamaSampler::chain_simple([LlamaSampler::temp(0.8), LlamaSampler::dist(0)]);

    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut pos = n_prompt as i32;
    let mut generated_tokens = 0usize;

    for _ in 0..MAX_RESPONSE_TOKENS {
        let token = sampler.sample(&ctx, 0);
        if model.is_eog_token(token) {
            break;
        }

        let bytes = model
            .token_to_bytes(token, Special::Plaintext)
            .context("не удалось получить байты токена")?;
        let mut piece = String::new();
        decoder.decode_to_string(&bytes, &mut piece, false);
        print!("{piece}");
        generated_tokens += 1;

        batch.clear();
        batch.add(token, pos, &[0], true)?;
        ctx.decode(&mut batch).context("ошибка decode() при генерации")?;
        pos += 1;
    }
    println!("\n");

    // Скорость — не строгое доказательство, что CUDA реально задействован
    // (медленная генерация может быть и по другим причинам), но грубый
    // диагностический сигнал: единицы ток/сек на GPU-сборке — повод
    // проверить, действительно ли офлоад на видеокарту включился.
    let elapsed = generation_started.elapsed();
    println!("--- Готово ---");
    println!(
        "Токенов промпта: {n_prompt} | сгенерировано: {generated_tokens} за {:.2?} ({:.1} ток/сек)",
        elapsed,
        generated_tokens as f64 / elapsed.as_secs_f64()
    );

    Ok(())
}
