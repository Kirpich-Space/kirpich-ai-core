use anyhow::{Context, Result, bail};
use serde_json::Value;
#[derive(Debug, PartialEq)]
pub enum StreamEvent {
    Text(String),
    Reasoning(String),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments_delta: Option<String>,
    },
    ToolCallsDone,
    Timings {
        tokens: u32,
        tps: f32,
        seconds: f32,
    },
    Limit,
    Done,
}
#[derive(Default)]
pub struct SseDecoder {
    pending: Vec<u8>,
    data: Vec<String>,
    done: bool,
    event_error: bool,
}
impl SseDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<StreamEvent>> {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() > 2 * 1024 * 1024 {
            bail!("Слишком большое событие SSE");
        }
        let mut out = vec![];
        while let Some(end) = self.pending.iter().position(|&c| c == b'\n') {
            let mut line = self.pending.drain(..=end).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = std::str::from_utf8(&line).context("Повреждённый UTF-8 в SSE")?;
            if line.is_empty() {
                if !self.data.is_empty() && !self.done {
                    let payload = self.data.join("\n");
                    self.data.clear();
                    if self.event_error {
                        bail!("Локальный сервер прислал событие ошибки SSE");
                    }
                    if payload == "[DONE]" {
                        self.done = true;
                        out.push(StreamEvent::Done);
                    } else {
                        let v: Value =
                            serde_json::from_str(&payload).context("Некорректное событие SSE")?;
                        if self.event_error || v.get("error").is_some() {
                            bail!("Локальный сервер вернул ошибку потока");
                        }
                        if let Some(choices) = v["choices"].as_array() {
                            for c in choices {
                                if c["finish_reason"].as_str() == Some("length") {
                                    out.push(StreamEvent::Limit);
                                }
                                if matches!(
                                    c["finish_reason"].as_str(),
                                    Some("error" | "content_filter")
                                ) {
                                    bail!(
                                        "Модель завершила ответ с ошибкой или ограничением содержимого"
                                    );
                                }
                                if let Some(s) = c["delta"]["content"].as_str() {
                                    out.push(StreamEvent::Text(s.into()));
                                }
                                if let Some(s) = c["delta"]["reasoning_content"].as_str() {
                                    out.push(StreamEvent::Reasoning(s.into()));
                                }
                                if let Some(calls) = c["delta"]["tool_calls"].as_array() {
                                    for tc in calls {
                                        let index = tc["index"].as_u64().unwrap_or(0) as usize;
                                        let id = tc["id"].as_str().map(str::to_owned);
                                        let name =
                                            tc["function"]["name"].as_str().map(str::to_owned);
                                        let arguments_delta =
                                            tc["function"]["arguments"].as_str().map(str::to_owned);
                                        if id.is_some()
                                            || name.is_some()
                                            || arguments_delta.is_some()
                                        {
                                            out.push(StreamEvent::ToolCallDelta {
                                                index,
                                                id,
                                                name,
                                                arguments_delta,
                                            });
                                        }
                                    }
                                }
                                if c["finish_reason"].as_str() == Some("tool_calls") {
                                    out.push(StreamEvent::ToolCallsDone);
                                }
                            }
                        }
                        if let Some(t) = v.get("timings") {
                            let tps = t["predicted_per_second"].as_f64().unwrap_or(0.0) as f32;
                            let tokens = t["predicted_n"].as_u64().unwrap_or(0) as u32;
                            let ms = t["predicted_ms"].as_f64().unwrap_or(0.0) as f32;
                            if tps > 0.0 && tokens > 0 {
                                out.push(StreamEvent::Timings {
                                    tokens,
                                    tps,
                                    seconds: (ms / 1000.0).max(0.001),
                                });
                            }
                        }
                    }
                }
                self.event_error = false;
            } else if let Some(x) = line.strip_prefix("data:") {
                self.data.push(x.strip_prefix(' ').unwrap_or(x).to_string());
            } else if let Some(x) = line.strip_prefix("event:") {
                self.event_error = x.trim() == "error";
            }
            if self.data.iter().map(|x| x.len()).sum::<usize>() > 2 * 1024 * 1024 {
                bail!("Слишком большое событие SSE");
            }
        }
        Ok(out)
    }
    pub fn finish(&self) -> Result<()> {
        if !self.done {
            bail!("Поток оборван до [DONE]; ответ неполный");
        }
        Ok(())
    }
}
