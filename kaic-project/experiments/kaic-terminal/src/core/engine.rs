use std::num::NonZeroU32;

use anyhow::{Context, Result};
use llama_cpp_4::prelude::*;

/// Имя стартового правила GBNF. Соглашение llama.cpp — "root"; вынесено в
/// константу, чтобы вызывающий не гадал, как назвать корневое правило.
pub const GRAMMAR_ROOT_RULE: &str = "root";

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
    /// Необязательная GBNF-грамматика, ограничивающая сэмплинг.
    ///
    /// Движок её не интерпретирует и не знает, кто и зачем её прислал: строка
    /// как есть уходит в грамматический сэмплер llama.cpp. Смысл конкретной
    /// грамматики — целиком забота вызывающего.
    ///
    /// `None` — путь генерации ровно тот же, что был до появления поля.
    pub grammar: Option<String>,
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
        // Грамматика ставится первой в цепочке: она обнуляет вероятность
        // недопустимых токенов до того, как temp/dist выберут из оставшихся.
        // Обратный порядок ничего бы не гарантировал.
        //
        // Ветка None собирает ровно ту же цепочку, что и до появления поля, —
        // без грамматики поведение не меняется ни на шаг.
        let sampler = match params.grammar.as_deref() {
            Some(grammar) => {
                println!("[engine] sampler с грамматикой ({} символов)", grammar.len());
                LlamaSampler::chain_simple([
                    LlamaSampler::grammar(&self.model, grammar, GRAMMAR_ROOT_RULE),
                    LlamaSampler::temp(params.temperature),
                    LlamaSampler::dist(0),
                ])
            }
            None => LlamaSampler::chain_simple([
                LlamaSampler::temp(params.temperature),
                LlamaSampler::dist(0),
            ]),
        };
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

            let piece = decode_utf8_piece(&mut decoder, &bytes, false);
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

        // Flush any incomplete UTF-8 sequence still held by the stateful decoder.
        output.push_str(&decode_utf8_piece(&mut decoder, &[], true));

        println!("[engine] генерация завершена");
        Ok(output)
    }
}

/// Incremental UTF-8 decode of one token chunk into a new `String`.
///
/// `encoding_rs::Decoder::decode_to_string` does **not** grow `dst`: it only
/// writes into the capacity already reserved beyond `dst.len()`. Callers must
/// `reserve` first (см. документацию encoding_rs). Без этого пустой
/// `String::new()` (capacity 0) молча возвращает `CoderResult::OutputFull`,
/// прочитав 0 байт, — то есть каждый токен превращается в пустую строку.
///
/// Состояние декодера намеренно живёт снаружи, между вызовами: byte-level BPE
/// режет многобайтовый UTF-8 (кириллица — 2 байта на символ) по границе
/// токена, и хвост незавершённой последовательности должен переноситься в
/// следующий вызов. Последний вызов с `last=true` отдаёт то, что осталось в
/// буфере декодера.
///
/// Результат `decode_to_string` не игнорируется: именно молча выброшенный
/// `CoderResult` и был первопричиной бага, поэтому цикл дочитывает вход до
/// `InputEmpty`, доращивая буфер, вместо того чтобы предполагать, что одного
/// вызова всегда достаточно.
fn decode_utf8_piece(decoder: &mut encoding_rs::Decoder, bytes: &[u8], last: bool) -> String {
    let mut piece = String::new();
    let mut consumed = 0usize;

    loop {
        let remaining = &bytes[consumed..];
        let needed = decoder
            .max_utf8_buffer_length(remaining.len())
            .unwrap_or_else(|| remaining.len().saturating_mul(3).max(4));
        piece.reserve(needed.max(4));

        let (result, read, _had_errors) = decoder.decode_to_string(remaining, &mut piece, last);
        consumed += read;

        match result {
            encoding_rs::CoderResult::InputEmpty => return piece,
            // Буфера не хватило: следующая итерация зарезервирует ещё под
            // непрочитанный остаток. read>0 гарантирует продвижение, а при
            // read==0 растёт зарезервированная ёмкость — цикл конечен.
            encoding_rs::CoderResult::OutputFull => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::decode_utf8_piece;

    /// Прогоняет чанки ровно так, как это делает цикл генерации: один
    /// stateful-декодер на весь проход, по вызову на токен, финальный flush.
    fn decode_stream(chunks: &[&[u8]]) -> String {
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let mut output = String::new();
        for chunk in chunks {
            output.push_str(&decode_utf8_piece(&mut decoder, chunk, false));
        }
        output.push_str(&decode_utf8_piece(&mut decoder, &[], true));
        output
    }

    #[test]
    fn decode_utf8_piece_reassembles_split_cyrillic() {
        // "Я" в UTF-8 — D0 AF; byte-level BPE регулярно режет такой символ
        // ровно по этой границе, отдавая по одному байту на токен.
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let first = decode_utf8_piece(&mut decoder, &[0xD0], false);
        let second = decode_utf8_piece(&mut decoder, &[0xAF], false);
        let flush = decode_utf8_piece(&mut decoder, &[], true);

        assert_eq!(first, "", "неполный ведущий байт обязан остаться в буфере");
        assert_eq!(second, "Я");
        assert_eq!(flush, "");
        assert_eq!(format!("{first}{second}{flush}"), "Я");
    }

    #[test]
    fn decode_stream_recovers_phrase_split_at_every_byte() {
        // Худший случай: каждый байт приходит отдельным токеном.
        let phrase = "STATUS: READY — план готов ✅";
        let chunks: Vec<&[u8]> = phrase.as_bytes().chunks(1).collect();

        assert_eq!(decode_stream(&chunks), phrase);
    }

    #[test]
    fn decode_stream_handles_realistic_multibyte_chunking() {
        // Границы токенов, попадающие внутрь 2- и 3-байтовых символов.
        let phrase = "Прочитать src/repl.rs";
        let bytes = phrase.as_bytes();
        let chunks: Vec<&[u8]> = vec![&bytes[..1], &bytes[1..5], &bytes[5..14], &bytes[14..]];

        assert_eq!(decode_stream(&chunks), phrase);
    }

    #[test]
    fn final_flush_emits_dangling_partial_sequence() {
        // Генерация оборвалась на max_tokens посреди символа. Без вызова с
        // last=true этот байт остался бы в декодере и потерялся молча.
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let piece = decode_utf8_piece(&mut decoder, &[0xD0], false);
        let flush = decode_utf8_piece(&mut decoder, &[], true);

        assert_eq!(piece, "");
        assert_eq!(flush, "\u{FFFD}", "flush обязан отдать хвост буфера");
    }

    #[test]
    fn decode_utf8_piece_empty_capacity_would_drop_bytes_without_reserve() {
        // Первопричина бага, зафиксированная как исполняемый факт:
        // decode_to_string в String::new() (capacity 0) не пишет ничего и не
        // читает ни байта даже для чистого ASCII — просто OutputFull.
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let mut piece = String::new();
        let (result, read, _) = decoder.decode_to_string(b"ok", &mut piece, false);

        assert!(piece.is_empty());
        assert_eq!(read, 0);
        assert!(matches!(result, encoding_rs::CoderResult::OutputFull));
    }
}
