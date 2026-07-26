use std::num::NonZeroU32;

use anyhow::{Context, Result};
use llama_cpp_4::prelude::*;

/// Одно сообщение диалога. Простая структура данных, а не поведение —
/// используется и здесь, и в repl.rs (и позже будет использоваться любым
/// будущим оркестратором/агентом), поэтому вынесена как публичный тип, а
/// не спрятана внутри engine.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
        }
    }
}

/// Параметры одного вызова генерации. Тоже просто данные, не поведение.
#[derive(Debug, Clone)]
pub struct ChatParams {
    pub temperature: f32,
    pub max_tokens: usize,
    pub context: u32,
}

/// KaicEngine — минимальный публичный интерфейс: new() + chat().
///
/// Сознательно НЕ хранит историю диалога и НЕ хранит параметры генерации
/// между вызовами — они приходят в chat() как аргументы. Это отличается от
/// предыдущей версии, где KaicEngine сам владел историей/temperature, что
/// неизбежно тянуло за собой публичные методы set_temperature/clear_history/
/// history_text и т.д. Состояние сессии теперь целиком на стороне repl.rs.
///
/// Причина, почему это важно не только ради чистоты интерфейса: именно так
/// должен выглядеть слой, поверх которого позже будет строиться
/// Planner/Coder/Reviewer или несколько агентов — они будут собирать
/// историю сами и вызывать chat(), не трогая внутренности движка.
///
/// Сам inference-pipeline внутри chat() не изменён по сравнению с
/// проверенной версией (experiments/llama_cpp_smoke после фикса индекса
/// сэмплинга) ни на строку логики — единственное отличие: параметры
/// (temperature/max_tokens/context) читаются из аргумента, а не из
/// self.config, потому что self.config больше не существует.
pub struct KaicEngine {
    backend: LlamaBackend,
    model: LlamaModel,
}

impl KaicEngine {
    pub fn new(model_path: &str, n_gpu_layers: u32) -> Result<Self> {
        println!("[engine] backend init...");
        let backend = LlamaBackend::init().context("не удалось инициализировать backend")?;
        println!("[engine] backend готов");

        println!("[engine] model loading: {model_path}...");
        let model_params = LlamaModelParams::default().with_n_gpu_layers(n_gpu_layers);
        let model = LlamaModel::load_from_file(&backend, model_path, &model_params)
            .with_context(|| format!("не удалось загрузить модель: {model_path}"))?;
        println!("[engine] model loaded");

        Ok(Self { backend, model })
    }

    pub fn chat(&self, history: &[ChatMessage], params: &ChatParams) -> Result<String> {
        // --- контекст ---
        println!("[engine] context creating...");
        let ctx_params = LlamaContextParams::default().with_n_ctx(NonZeroU32::new(params.context));
        let mut ctx = self
            .model
            .new_context(&self.backend, ctx_params)
            .context("не удалось создать контекст генерации")?;
        println!("[engine] context created");

        // --- chat template ---
        println!("[engine] applying chat template...");
        let messages: Vec<LlamaChatMessage> = history
            .iter()
            .map(|m| LlamaChatMessage::new(m.role.clone(), m.content.clone()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("не удалось построить сообщения чата")?;
        let prompt = self
            .model
            .apply_chat_template(None, &messages, true)
            .context("не удалось применить chat template")?;
        println!("[engine] prompt готов ({} символов)", prompt.len());

        // --- токенизация ---
        println!("[engine] tokenize...");
        let tokens = self
            .model
            .str_to_token(&prompt, AddBos::Always)
            .context("не удалось токенизировать промпт")?;
        let n_prompt = tokens.len();
        println!("[engine] got {n_prompt} tokens");

        // --- батч промпта ---
        println!("[engine] creating batch (capacity={})...", params.context);
        let mut batch = LlamaBatch::new(params.context as usize, 1);
        println!("[engine] batch создан, заполняю токенами промпта...");
        let mut logits_true_count = 0usize;
        for (i, &tok) in tokens.iter().enumerate() {
            let is_last = i == n_prompt - 1;
            println!("[BEFORE] batch.add(token={tok:?}, pos={i}, logits={is_last})");
            batch
                .add(tok, i as i32, &[0], is_last)
                .context("batch.add() промпта упал")?;
            println!("[AFTER] batch.add() успешно");
            if is_last {
                logits_true_count += 1;
            }
        }
        println!(
            "[engine] batch ready: size={n_prompt} | количество токенов с logits=true: {logits_true_count}"
        );
        if logits_true_count != 1 {
            anyhow::bail!(
                "ожидался ровно 1 токен с logits=true в batch промпта, получено {logits_true_count}"
            );
        }

        // --- decode промпта ---
        println!("[BEFORE] ctx.decode(prompt batch)");
        ctx.decode(&mut batch).context("ошибка decode() промпта")?;
        println!("[AFTER] ctx.decode(prompt batch) успешно");
        println!("[engine] prompt decoded");

        // --- sampler ---
        // Seed фиксирован (0), как в проверенной версии smoke-теста — никаких
        // "улучшений" вроде динамического seed на этом этапе не вносится.
        println!("[engine] создаю sampler (temp={})...", params.temperature);
        let sampler = LlamaSampler::chain_simple([
            LlamaSampler::temp(params.temperature),
            LlamaSampler::dist(0),
        ]);
        println!("[engine] sampler готов");

        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let mut pos = n_prompt as i32;
        let mut output = String::new();

        // --- цикл генерации ---
        println!(
            "[engine] старт цикла генерации, max_tokens={}",
            params.max_tokens
        );
        for step in 0..params.max_tokens {
            // Индекс подтверждён эмпирически в experiments/llama_cpp_smoke:
            // именно batch.n_tokens() - 1, а не голый 0.
            let sample_idx = (batch.n_tokens() - 1) as i32;
            println!("[{step}] sampling logits index = {sample_idx}");
            println!("[BEFORE] sampler.sample(idx={sample_idx})");
            let token = sampler.sample(&ctx, sample_idx);
            println!("[AFTER] sampler.sample() успешно, token={token:?}");

            if self.model.is_eog_token(token) {
                println!("[{step}] eog-токен — генерация завершена");
                break;
            }

            println!("[BEFORE] model.token_to_bytes(token={token:?})");
            let bytes = self
                .model
                .token_to_bytes(token, Special::Plaintext)
                .context("не удалось получить байты токена")?;
            println!("[AFTER] token_to_bytes() успешно, {} байт", bytes.len());

            let mut piece = String::new();
            decoder.decode_to_string(&bytes, &mut piece, false);
            println!("[{step}] token text = {piece:?}");
            output.push_str(&piece);

            batch.clear();
            println!("[BEFORE] batch.add(token={token:?}, pos={pos}, logits=true)");
            batch
                .add(token, pos, &[0], true)
                .context("batch.add() при генерации упал")?;
            println!("[AFTER] batch.add() успешно");

            println!("[BEFORE] ctx.decode(next)");
            ctx.decode(&mut batch)
                .context("ошибка decode() при генерации")?;
            println!("[AFTER] ctx.decode(next) успешно");

            pos += 1;
        }

        println!("[engine] генерация завершена");
        Ok(output)
    }
}
