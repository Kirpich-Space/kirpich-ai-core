//! Плавные 60-FPS GPU-анимации и инженерные детали (keycaps, телеметрия t/c) для KAIC Studio.
//! По правилам unslop: в состоянии покоя нет зацикленного качания и свечений;
//! анимация осмысленна и работает на действиях (выезд панелей, hover/focus, стриминг, t/c).
use super::palette;
use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
use std::f32::consts::TAU;

pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| -> u8 { (x as f32 + (y as f32 - x as f32) * t).round() as u8 };
    Color32::from_rgba_unmultiplied(
        mix(a.r(), b.r()),
        mix(a.g(), b.g()),
        mix(a.b(), b.b()),
        mix(a.a(), b.a()),
    )
}

pub fn with_alpha(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

/// Плавный коэффициент открытия панели 0..=1 (в headless-тестах с time==0 отдаёт целевое значение).
pub fn panel_t(ctx: &egui::Context, id: &str, open: bool) -> f32 {
    if ctx.input(|i| i.time) == 0.0 {
        return if open { 1.0 } else { 0.0 };
    }
    let raw = ctx.animate_bool_with_time(egui::Id::new(id), open, 0.18);
    if open {
        ease_out_cubic(raw)
    } else {
        1.0 - ease_out_cubic(1.0 - raw)
    }
}

/// Инженерный кейкап для горячих клавиш (signature-деталь KAIC Studio).
pub fn keycap(ui: &mut egui::Ui, label: &str) {
    let pal = palette::p();
    egui::Frame::new()
        .fill(pal.surface_2)
        .stroke(Stroke::new(1.0, pal.line))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(
                RichText::new(label)
                    .monospace()
                    .size(10.5)
                    .color(pal.ink_muted),
            );
        });
}

/// Оригинальный трёхкурсорный знак KAIC: спокойный в покое (без glow/качания),
/// мягко реагирует на наведение и оживает каскадным движением курсоров во время генерации.
pub fn animated_mark(ui: &mut egui::Ui, size: f32, busy: bool) {
    let pal = palette::p();
    let (alloc, resp) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    let time = ui.input(|i| i.time) as f32;
    let hovered = if time > 0.0 {
        ease_out_cubic(ui.ctx().animate_bool_with_time(
            ui.id().with("kaic-mark-hover"),
            resp.hovered(),
            0.16,
        ))
    } else {
        0.0
    };
    let rect = egui::Rect::from_center_size(alloc.center(), Vec2::splat(size));
    let p = ui.painter();
    let s = size / 64.0;
    let rounding = (16.0 * s).round() as u8;

    p.rect_filled(rect, rounding, pal.brand);

    for (idx, (dx, color)) in [
        (0.0_f32, pal.ceramic_200),
        (11.0_f32, pal.ink),
        (22.0_f32, pal.on_brand),
    ]
    .into_iter()
    .enumerate()
    {
        let (shift_x, shift_y) = if busy && time > 0.0 {
            let phase = time * 3.4 - idx as f32 * 0.55;
            (phase.sin() * 1.4 * s, phase.cos() * 0.8 * s)
        } else {
            // При наведении три курсора слегка раскрываются веером (1–2px)
            ((idx as f32 - 1.0) * hovered * 1.5 * s, -hovered * 1.0 * s)
        };
        let pt = |x: f32, y: f32| rect.min + egui::vec2(x * s + shift_x, y * s + shift_y);
        p.add(egui::Shape::convex_polygon(
            vec![
                pt(11.0 + dx, 16.0),
                pt(31.0 + dx, 35.0),
                pt(17.5 + dx, 37.0),
                pt(11.0 + dx, 43.0),
            ],
            color,
            Stroke::new((1.6 * s).max(0.8), color),
        ));
        p.add(egui::Shape::convex_polygon(
            vec![
                pt(17.5 + dx, 35.5),
                pt(22.5 + dx, 35.0),
                pt(27.0 + dx, 44.8),
                pt(22.0 + dx, 47.0),
            ],
            color,
            Stroke::new((1.2 * s).max(0.6), color),
        ));
    }

    if busy && time > 0.0 {
        ui.ctx().request_repaint();
    }
}

/// Живая точка статуса: статичная в покое, мягкое кольцо-радар ТОЛЬКО при активной генерации.
pub fn pulse_dot(ui: &mut egui::Ui, color: Color32, _breathing: bool, radar: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    let c = rect.center();
    let p = ui.painter();
    let time = ui.input(|i| i.time) as f32;

    if radar && time > 0.0 {
        let wave = (time * 1.35) % 1.0;
        let r = 3.5 + ease_out_cubic(wave) * 4.5;
        let alpha = ((1.0 - wave) * 150.0) as u8;
        p.circle_stroke(c, r, Stroke::new(1.2, with_alpha(color, alpha)));
        ui.ctx().request_repaint();
    }

    p.circle_filled(c, 3.2, color);
}

/// Анимация ожидания ответа модели до первого токена: три ритмичные точки + бегущая полоска.
pub fn thinking_wave(ui: &mut egui::Ui, status: &str) {
    let pal = palette::p();
    let time = ui.input(|i| i.time) as f32;
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(36.0, 16.0), egui::Sense::hover());
            let p = ui.painter();
            for i in 0..3 {
                let phase = (time * 5.0 - i as f32 * 0.75).sin();
                let bounce = phase.max(0.0);
                let x = rect.left() + 6.0 + i as f32 * 11.0;
                let y = rect.center().y - bounce * 4.0;
                let r = 2.5 + bounce * 0.9;
                let col = lerp_color(pal.line_strong, pal.brand, bounce);
                p.circle_filled(egui::pos2(x, y), r, col);
            }
            ui.label(RichText::new(status).size(13.0).color(pal.ink_muted));
        });
        ui.add_space(4.0);
        let w = ui.available_width().clamp(140.0, 240.0);
        let (bar, _) = ui.allocate_exact_size(egui::vec2(w, 3.0), egui::Sense::hover());
        let p = ui.painter();
        p.rect_filled(bar, 2, pal.surface_2);
        let pos = ((time * 0.85) % 1.4) - 0.2;
        let seg_w = w * 0.32;
        let x0 = (bar.left() + pos * w).clamp(bar.left(), bar.right());
        let x1 = (x0 + seg_w).clamp(bar.left(), bar.right());
        if x1 > x0 + 1.0 {
            let seg =
                egui::Rect::from_min_max(egui::pos2(x0, bar.top()), egui::pos2(x1, bar.bottom()));
            p.rect_filled(seg, 2, with_alpha(pal.brand, 185));
        }
    });
    if time > 0.0 {
        ui.ctx().request_repaint();
    }
}

/// Живая мигающая каретка стриминга в конце ответа ассистента.
pub fn streaming_cursor(ui: &mut egui::Ui) {
    let pal = palette::p();
    let time = ui.input(|i| i.time) as f32;
    let pulse = 0.5 + 0.5 * (time * TAU * 1.1).sin();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 14.0), egui::Sense::hover());
    ui.painter().rect_filled(
        egui::Rect::from_center_size(rect.center(), egui::vec2(2.5, 13.0)),
        1,
        with_alpha(pal.brand, (70.0 + pulse * 185.0) as u8),
    );
    if time > 0.0 {
        ui.ctx().request_repaint();
    }
}

/// Компактная телеметрия скорости `tok/s` / `t/c` под сообщением ассистента.
pub fn tps_badge(
    ui: &mut egui::Ui,
    tps: f32,
    tokens: Option<u32>,
    seconds: Option<f32>,
    live: bool,
    lang: super::i18n::Lang,
) {
    let pal = palette::p();
    let border = if live {
        with_alpha(pal.brand, 110)
    } else {
        pal.line
    };
    egui::Frame::new()
        .fill(pal.surface_1)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if live {
                    pulse_dot(ui, pal.brand, false, true);
                } else {
                    pulse_dot(ui, pal.ceramic_200, false, false);
                }
                ui.label(
                    RichText::new(format!("{tps:.1} {}", lang.t("tok/s", "t/c")))
                        .monospace()
                        .strong()
                        .size(11.5)
                        .color(if live { pal.ink } else { pal.brand_text }),
                );
                if let Some(tok) = tokens {
                    ui.label(
                        RichText::new(format!("· {tok} {}", lang.t("tok", "ток")))
                            .monospace()
                            .size(11.0)
                            .color(pal.ink_muted),
                    );
                }
                if let Some(sec) = seconds {
                    ui.label(
                        RichText::new(format!("· {sec:.2} {}", lang.t("s", "с")))
                            .monospace()
                            .size(11.0)
                            .color(pal.ink_muted),
                    );
                }
            });
        });
}
