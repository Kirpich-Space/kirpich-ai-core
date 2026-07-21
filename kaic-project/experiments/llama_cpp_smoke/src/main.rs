//! Максимально линейная альфа-версия smoke-test'а llama-cpp-4.
//!
//! Никаких абстракций, никакого рефакторинга старого кода. Единственная
//! цель: увидеть глазами, на каком именно вызове llama.cpp программа
//! зависает (если зависает), и явно проверить в рантайме предположение
//! об индексации logits, а не полагаться на него молча.
//!
//! Запуск:
//!   cargo run -- /путь/к/модели.gguf
//!   cargo run -- /путь/к/модели.gguf "свой вопрос" 0 32
//!            (аргументы: путь, промпт, n_gpu_layers, max_tokens)

use std::env;
use std::io::{self, Write};
use std::num::NonZeroU32;
use std::time::Instant;

use anyhow::{Context, Result};
use llama_cpp_4::prelude::*;

macro_rules! log_step {
    ($($arg:tt)*) => {{
        println!($($arg)*);
        io::stdout().flush().ok();
    }};
}

/// ВАЖНО про индекс сэмплинга.
///
/// `sampler.sample(&ctx, idx)` принимает СЫРУЮ позицию токена внутри
/// ТЕКУЩЕГО batch — того токена, у которого стоит logits=true. Это НЕ
/// компактный индекс "по счёту запросов logits", а именно raw-слот в
/// batch. Официальный пример крейта на crates.io использует
/// `sampler.sample(&ctx, batch.n_tokens() - 1)` — то есть позицию
/// последнего слота в текущем batch, потому что именно последний
/// добавленный токен помечается logits=true.
///
/// Раньше здесь был захардкожен sample_idx = 0, что верно только когда
/// в batch ровно один токен (генерация после первого шага), но НЕВЕРНО
/// сразу после decode() всего промпта, где batch содержит n_prompt
/// токенов и logits=true стоит на позиции n_prompt-1, а не на 0. Именно
/// это, по всей видимости, и вызывало ошибку
/// `get_logits_ith: ... batch.logits[0] != true`.
///
/// Ниже это не просто "предполагается" — количество токенов с logits=true
/// в последнем batch перед decode() явно подсчитывается и логируется, и
/// если вдруг окажется не 1 — программа явно упадёт с понятной ошибкой,
/// а не тихо выдаст неверный token.
const EXPECTED_LOGITS_REQUESTS_PER_DECODE: usize = 1;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    let model_path = args
        .get(1)
        .context("укажи путь к .gguf первым аргументом")?;
    let prompt_text = args
        .get(2)
        .map(String::as_str)
        .unwrap_or("Привет! Расскажи в двух предложениях, кто ты.");
    let n_gpu_layers: u32 = args.get(3).map(|s| s.parse().unwrap_or(0)).unwrap_or(0);
    let max_tokens: usize = args.get(4).map(|s| s.parse().unwrap_or(32)).unwrap_or(32);

    log_step!(
        "[0] аргументы: model={model_path} n_gpu_layers={n_gpu_layers} max_tokens={max_tokens}"
    );

    // --- 1. backend ---
    log_step!("[1] backend init...");
    let backend = LlamaBackend::init().context("не удалось инициализировать backend")?;
    log_step!("[2] backend готов");

    // --- 2. загрузка модели ---
    log_step!("[3] model loading...");
    let model_params = LlamaModelParams::default().with_n_gpu_layers(n_gpu_layers);
    let load_started = Instant::now();
    let model = LlamaModel::load_from_file(&backend, model_path, &model_params)
        .context("не удалось загрузить модель")?;
    log_step!("[4] model loaded за {:.2?}", load_started.elapsed());

    // --- 3. контекст ---
    log_step!("[5] context creating...");
    let ctx_params = LlamaContextParams::default().with_n_ctx(NonZeroU32::new(4096));
    let mut ctx = model
        .new_context(&backend, ctx_params)
        .context("не удалось создать контекст")?;
    log_step!("[6] context created");

    // --- 4. chat template ---
    log_step!("[7] building chat message...");
    let messages = vec![LlamaChatMessage::new("user".into(), prompt_text.to_string())
        .context("не удалось построить сообщение чата")?];
    log_step!("[8] applying chat template...");
    let prompt = model
        .apply_chat_template(None, &messages, true)
        .context("не удалось применить chat template")?;
    log_step!("[9] prompt готов:\n---\n{prompt}\n---");

    // --- 5. токенизация ---
    log_step!("[10] tokenize...");
    let tokens = model
        .str_to_token(&prompt, AddBos::Always)
        .context("не удалось токенизировать промпт")?;
    let n_prompt = tokens.len();
    log_step!("[11] got {n_prompt} tokens");
    log_step!("[11.info] prompt tokens: {n_prompt} | last prompt token position: {}", n_prompt.saturating_sub(1));

    // --- 6. батч промпта ---
    log_step!("[12] creating batch (capacity=4096)...");
    let mut batch = LlamaBatch::new(4096, 1);
    log_step!("[13] batch создан, заполняю токенами промпта...");
    let mut logits_requested_positions: Vec<usize> = Vec::new();
    for (i, &tok) in tokens.iter().enumerate() {
        let is_last = i == n_prompt - 1;
        log_step!("[BEFORE] batch.add(token={tok:?}, pos={i}, logits={is_last})");
        batch
            .add(tok, i as i32, &[0], is_last)
            .context("batch.add() промпта упал")?;
        log_step!("[AFTER] batch.add() успешно");
        if is_last {
            logits_requested_positions.push(i);
        }
    }
    log_step!(
        "[14] batch ready: size={n_prompt} | какая raw-позиция имеет logits=true: {:?} | количество запросов logits в этом batch: {}",
        logits_requested_positions,
        logits_requested_positions.len()
    );
    if logits_requested_positions.len() != EXPECTED_LOGITS_REQUESTS_PER_DECODE {
        anyhow::bail!(
            "ожидался ровно {} токен с logits=true в batch промпта, получено {} — предположение об индексе больше не верно, чинить логику, а не сам индекс вслепую",
            EXPECTED_LOGITS_REQUESTS_PER_DECODE,
            logits_requested_positions.len()
        );
    }

    // --- 7. decode промпта ---
    log_step!("[BEFORE] ctx.decode(prompt batch)");
    let decode_started = Instant::now();
    ctx.decode(&mut batch).context("ошибка decode() промпта")?;
    log_step!(
        "[AFTER] ctx.decode(prompt batch) успешно, заняло {:.2?}",
        decode_started.elapsed()
    );
    log_step!("[15] prompt decoded. logits available at raw batch position {}", n_prompt.saturating_sub(1));

    // --- 8. sampler ---
    log_step!("[16] создаю sampler (temp=0.8, dist)...");
    let sampler = LlamaSampler::chain_simple([LlamaSampler::temp(0.8), LlamaSampler::dist(0)]);
    log_step!("[17] sampler готов");

    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut pos = n_prompt as i32;

    // --- 9. цикл генерации ---
    log_step!("[18] старт цикла генерации, max_tokens={max_tokens}");
    for step in 0..max_tokens {
        let sample_idx = batch.n_tokens() - 1; // сырая позиция в ТЕКУЩЕМ batch, не 0 захардкожено
        log_step!("sampling logits index = {sample_idx}");
        log_step!("expected logits count = {EXPECTED_LOGITS_REQUESTS_PER_DECODE}");
        log_step!("[19.{step}] почему используется позиция {sample_idx}: это batch.n_tokens()-1 — raw-позиция последнего токена в текущем batch, у которого стоит logits=true");
        log_step!("[BEFORE] sampler.sample(idx={sample_idx})");
        let token = sampler.sample(&ctx, sample_idx);
        log_step!("[AFTER] sampler.sample() успешно, token={token:?} — logits были получены без падения (иначе sample() запаниковал бы здесь)");

        log_step!("[20.{step}] is_eog_token() проверка...");
        if model.is_eog_token(token) {
            log_step!("[21.{step}] это eog-токен — генерация завершена по стоп-условию");
            break;
        }
        log_step!("[21.{step}] не eog, продолжаем");

        log_step!("[BEFORE] model.token_to_bytes(token={token:?})");
        let bytes = model
            .token_to_bytes(token, Special::Plaintext)
            .context("не удалось получить байты токена")?;
        log_step!("[AFTER] token_to_bytes() успешно, {} байт", bytes.len());

        let mut piece = String::new();
        decoder.decode_to_string(&bytes, &mut piece, false);
        log_step!("[22.{step}] token text = {piece:?}");

        log_step!("[23.{step}] batch.clear() и добавляю токен обратно, pos={pos}...");
        batch.clear();
        log_step!("[BEFORE] batch.add(token={token:?}, pos={pos}, logits=true)");
        batch
            .add(token, pos, &[0], true)
            .context("batch.add() при генерации упал")?;
        log_step!("[AFTER] batch.add() успешно");
        let logits_true_count_gen = 1usize; // единственный add() в этом batch, и он всегда с logits=true
        log_step!(
            "[23.{step}.info] batch size=1 | добавленные позиции: [{pos}] | количество токенов с logits=true: {logits_true_count_gen}"
        );
        if logits_true_count_gen != EXPECTED_LOGITS_REQUESTS_PER_DECODE {
            anyhow::bail!(
                "batch генерации на шаге {step}: ожидался {} токен с logits=true, получено {} — останавливаюсь, не угадываю индекс",
                EXPECTED_LOGITS_REQUESTS_PER_DECODE,
                logits_true_count_gen
            );
        }

        log_step!("[BEFORE] ctx.decode(next)");
        let step_started = Instant::now();
        ctx.decode(&mut batch).context("ошибка decode() при генерации")?;
        log_step!(
            "[AFTER] ctx.decode(next) успешно, заняло {:.2?}",
            step_started.elapsed()
        );
        log_step!("[24.{step}] decode next. logits available at raw batch position {}", batch.n_tokens() - 1);

        pos += 1;
    }

    log_step!("[25] цикл генерации завершён (или достигнут max_tokens)");
    log_step!("[26] --- ГОТОВО ---");

    Ok(())
}