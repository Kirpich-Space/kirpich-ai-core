//! Снимок отдельного тестового окна. Запускать только на выделенном Xvfb.
use eframe::{App, egui};
use kaic_studio::{storage, ui::Studio};
use std::{path::PathBuf, time::Instant};

struct Capture {
    studio: Studio,
    output: PathBuf,
    frames: usize,
    started: Instant,
}

impl App for Capture {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.studio.ui(ui, frame);
        self.frames += 1;
        let ctx = ui.ctx();
        if self.frames == 4 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        for event in ctx.input(|i| i.events.clone()) {
            if let egui::Event::Screenshot { image, .. } = event {
                let pixels: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
                image::save_buffer(
                    &self.output,
                    &pixels,
                    image.width() as u32,
                    image.height() as u32,
                    image::ColorType::Rgba8,
                )
                .expect("Не удалось сохранить тестовый снимок");
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        assert!(self.started.elapsed().as_secs() < 20, "Снимок не получен");
        ctx.request_repaint();
    }
    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        self.studio.on_exit(gl);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let scenario = std::env::args().nth(1).unwrap_or_else(|| "empty".into());
    let (width, height, conversation) = match scenario.as_str() {
        "empty" => (1200.0, 850.0, false),
        "narrow" => (540.0, 620.0, false),
        "conversation" | "sidebar" => (1200.0, 850.0, true),
        "model-picker" | "settings" => (1200.0, 850.0, false),
        _ => panic!("Неизвестный тестовый сценарий"),
    };
    let root = std::env::var_os("KAIC_VERIFICATION_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("verification/ollama-full")
        });
    let data = root.join(format!("capture-data-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&data)?;
    if conversation {
        let mut chat = storage::Chat::new();
        chat.title = "Синтетический разговор · только снимок".into();
        chat.messages.push(storage::Message::user(
            "Как устроена локальная студия?".into(),
        ));
        let mut reply = storage::Message::assistant();
        reply.content = "**Синтетическая фикстура: инференс не выполнялся.**\n\nМодель работает на вашем компьютере. Прикрепите текстовый файл или задайте следующий вопрос.\n\n## Возможности\n\n- **Текст** и *выделение*\n- [Документация](https://example.com)\n\n| Режим | Данные |\n| --- | --- |\n| Локальный | На компьютере |\n\n```rust\n// Синтетический пример для проверки COPY\nfn main() {}\n```".into();
        chat.messages.push(reply);
        let state = storage::SavedState {
            chats: vec![chat],
            ..Default::default()
        };
        storage::save_json(&data.join("state.json"), &state)?;
    }
    let output = root.join(format!("{scenario}.png"));
    eframe::run_native(
        "KAIC Studio · проверка интерфейса",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([width, height]),
            renderer: eframe::Renderer::Glow,
            ..Default::default()
        },
        Box::new(move |cc| {
            Ok(Box::new(Capture {
                studio: {
                    let mut studio = Studio::new(cc, data);
                    studio.show_capture_view(&scenario);
                    studio
                },
                output,
                frames: 0,
                started: Instant::now(),
            }))
        }),
    )
    .map_err(|e| e.to_string().into())
}
