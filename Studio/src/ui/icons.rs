use super::{anim, palette};
use eframe::egui::{self, Color32, Pos2, Stroke, Vec2};

#[derive(Clone, Copy)]
pub(super) enum Icon {
    Sidebar,
    RightPanel,
    NewChat,
    Attach,
    Send,
    Stop,
    More,
    Copy,
    Retry,
    Edit,
    Check,
    Settings,
}

pub(super) fn button(ui: &mut egui::Ui, icon: Icon, label: &str) -> egui::Response {
    let round = matches!(icon, Icon::Send | Icon::Stop);
    let quiet = matches!(icon, Icon::Copy | Icon::Retry | Icon::Edit | Icon::Check);
    let response = ui
        .add(
            egui::Button::new("")
                .frame(false)
                .min_size(Vec2::splat(if quiet { 28.0 } else { 32.0 })),
        )
        .on_hover_text(label);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    if response.has_focus() {
        ui.painter().rect_stroke(
            response.rect.expand(1.5),
            6,
            Stroke::new(1.5, palette::LINE_STRONG),
            egui::StrokeKind::Outside,
        );
    }
    let hover_t = if ui.input(|i| i.time) > 0.0 {
        ui.ctx()
            .animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.14)
    } else if response.hovered() {
        1.0
    } else {
        0.0
    };
    if round {
        let active_t = if ui.input(|i| i.time) > 0.0 {
            ui.ctx()
                .animate_bool_with_time(response.id.with("enabled"), ui.is_enabled(), 0.18)
        } else if ui.is_enabled() {
            1.0
        } else {
            0.0
        };
        let fill = anim::lerp_color(palette::SURFACE_2, palette::BRAND, active_t);
        let fill = if ui.is_enabled() {
            anim::lerp_color(fill, palette::INK, hover_t * 0.12)
        } else {
            fill
        };
        ui.painter()
            .rect_filled(response.rect.shrink(1.0 - hover_t * 0.5), 8, fill);
    } else if hover_t > 0.01 {
        ui.painter().rect_filled(
            response.rect,
            6,
            Color32::from_rgba_unmultiplied(244, 241, 236, (hover_t * 18.0) as u8),
        );
    }
    paint(
        ui,
        response.rect,
        icon,
        if round {
            if ui.is_enabled() {
                palette::ON_BRAND
            } else {
                palette::INK_MUTED
            }
        } else if quiet && !response.hovered() && !response.has_focus() {
            palette::INK_MUTED
        } else {
            palette::INK
        },
    );
    response
}

pub(super) fn paint(ui: &egui::Ui, rect: egui::Rect, icon: Icon, color: Color32) {
    let p = ui.painter();
    let c = rect.center();
    let point = |x, y| c + egui::vec2(x, y);
    let stroke = Stroke::new(1.4, color);
    let line = |a: Pos2, b: Pos2| {
        p.line_segment([a, b], stroke);
    };
    match icon {
        Icon::Sidebar => {
            p.rect_stroke(
                egui::Rect::from_center_size(c, egui::vec2(17.0, 14.0)),
                3,
                stroke,
                egui::StrokeKind::Inside,
            );
            line(point(-3.0, -6.5), point(-3.0, 6.5));
        }
        Icon::RightPanel => {
            p.rect_stroke(
                egui::Rect::from_center_size(c, egui::vec2(17.0, 14.0)),
                3,
                stroke,
                egui::StrokeKind::Inside,
            );
            line(point(3.0, -6.5), point(3.0, 6.5));
        }
        Icon::NewChat => {
            p.add(egui::Shape::line(
                vec![
                    point(7.0, 1.0),
                    point(7.0, 8.0),
                    point(-8.0, 8.0),
                    point(-8.0, -7.0),
                    point(-1.0, -7.0),
                ],
                stroke,
            ));
            line(point(-2.0, 2.0), point(8.0, -8.0));
            line(point(5.0, -8.0), point(8.0, -5.0));
        }
        Icon::Attach => {
            p.rect_stroke(
                egui::Rect::from_center_size(c, Vec2::splat(28.0)),
                8,
                Stroke::new(1.0, palette::LINE),
                egui::StrokeKind::Inside,
            );
            line(point(-5.5, 0.0), point(5.5, 0.0));
            line(point(0.0, -5.5), point(0.0, 5.5));
        }
        Icon::Send => {
            line(point(0.0, 7.0), point(0.0, -7.0));
            line(point(-5.0, -2.0), point(0.0, -7.0));
            line(point(0.0, -7.0), point(5.0, -2.0));
        }
        Icon::Stop => {
            p.rect_filled(egui::Rect::from_center_size(c, Vec2::splat(11.0)), 2, color);
        }
        Icon::Copy => {
            p.rect_stroke(
                egui::Rect::from_min_max(point(-3.0, -3.0), point(7.0, 8.0)),
                2,
                stroke,
                egui::StrokeKind::Inside,
            );
            p.add(egui::Shape::line(
                vec![
                    point(-6.0, 4.0),
                    point(-8.0, 4.0),
                    point(-8.0, -8.0),
                    point(3.0, -8.0),
                    point(3.0, -6.0),
                ],
                stroke,
            ));
        }
        Icon::Retry => {
            p.add(egui::Shape::line(
                vec![
                    point(7.0, 2.0),
                    point(5.0, 7.0),
                    point(-3.0, 8.0),
                    point(-8.0, 3.0),
                    point(-8.0, -3.0),
                    point(-3.0, -8.0),
                    point(4.0, -7.0),
                    point(7.0, -3.0),
                ],
                stroke,
            ));
            line(point(7.0, -9.0), point(7.0, -3.0));
            line(point(7.0, -3.0), point(1.0, -3.0));
        }
        Icon::Edit => {
            p.add(egui::Shape::closed_line(
                vec![
                    point(-7.0, 7.0),
                    point(-5.0, 1.0),
                    point(5.0, -9.0),
                    point(9.0, -5.0),
                    point(-1.0, 5.0),
                ],
                stroke,
            ));
            line(point(2.0, -6.0), point(6.0, -2.0));
        }
        Icon::Check => {
            line(point(-7.0, 0.0), point(-2.0, 5.0));
            line(point(-2.0, 5.0), point(8.0, -6.0));
        }
        Icon::More => {
            for x in [-6.0, 0.0, 6.0] {
                p.circle_filled(point(x, 0.0), 1.5, color);
            }
        }
        Icon::Settings => {
            // Прецизионная шестерёнка (8 зубьев + центральное отверстие)
            p.circle_stroke(c, 5.2, stroke);
            p.circle_stroke(c, 2.2, stroke);
            let tooth_stroke = Stroke::new(1.8, color);
            for i in 0..8 {
                let a = (i as f32) * std::f32::consts::FRAC_PI_4;
                let (sin, cos) = a.sin_cos();
                p.line_segment(
                    [
                        c + egui::vec2(cos * 5.2, sin * 5.2),
                        c + egui::vec2(cos * 7.6, sin * 7.6),
                    ],
                    tooth_stroke,
                );
            }
        }
    }
}

/// Отрисовать иконку фиксированного размера inline (например, в кнопке боковой панели).
pub(super) fn inline(ui: &mut egui::Ui, icon: Icon, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    paint(ui, rect, icon, color);
}

/// Чёткий цветовой индикатор-точка (без зависимости от глифа ● в шрифте).
pub(super) fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

/// Оригинальный трёхкурсорный знак KAIC (по координатам KAIC/Desige/kaic-mark.svg).
pub(super) fn mark_sized(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let p = ui.painter();
    let s = size / 64.0;
    let pt = |x: f32, y: f32| rect.min + egui::vec2(x * s, y * s);
    p.rect_filled(rect, (16.0 * s).round() as u8, palette::BRAND);
    // Три наложенных курсора (верхний треугольник + хвост) из kaic-mark.svg.
    for (dx, color) in [
        (0.0, palette::CERAMIC_200),
        (11.0, palette::INK),
        (22.0, palette::ON_BRAND),
    ] {
        // Главный треугольник курсора (выпуклый)
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
        // Хвостик курсора (выпуклый четырёхугольник)
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
}

/// Feedback is held per stable widget id; keyboard activation uses egui's Button.
pub(super) fn copy_button(ui: &mut egui::Ui, id: egui::Id, text: &str, lang: super::i18n::Lang) {
    use std::time::{Duration, Instant};
    let copied = ui
        .ctx()
        .data_mut(|d| d.get_temp::<Instant>(id))
        .is_some_and(|t| t.elapsed() < Duration::from_secs(2));
    if button(
        ui,
        if copied { Icon::Check } else { Icon::Copy },
        if copied {
            lang.t("Copied", "Скопировано")
        } else {
            lang.t("Copy", "Копировать")
        },
    )
    .clicked()
    {
        ui.ctx().copy_text(text.to_owned());
        ui.ctx().data_mut(|d| d.insert_temp(id, Instant::now()));
        ui.ctx().request_repaint();
    }
    if copied {
        ui.label(
            egui::RichText::new(lang.t("Copied", "Скопировано"))
                .small()
                .weak(),
        );
        ui.ctx().request_repaint_after(Duration::from_secs(2));
    }
}
