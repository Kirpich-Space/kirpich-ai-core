//! Локальный Markdown: форматирование, ссылки, списки, таблицы и код.
use super::icons;
use eframe::egui::{self, Color32, FontId, RichText, Stroke, TextFormat};

#[derive(Default, Clone, Copy)]
struct Style {
    strong: bool,
    italic: bool,
    code: bool,
}
struct Span {
    text: String,
    style: Style,
    url: Option<String>,
}
fn spans(text: &str, style: Style) -> Vec<Span> {
    let mut result = Vec::new();
    let mut rest = text;
    let mut plain = String::new();
    let flush = |plain: &mut String, result: &mut Vec<Span>| {
        if !plain.is_empty() {
            result.push(Span {
                text: std::mem::take(plain),
                style,
                url: None,
            });
        }
    };
    while !rest.is_empty() {
        if let Some(escaped) = rest.strip_prefix('\\')
            && let Some(c) = escaped.chars().next()
        {
            plain.push(c);
            rest = &escaped[c.len_utf8()..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find('`')
        {
            flush(&mut plain, &mut result);
            result.push(Span {
                text: after[..end].into(),
                style: Style {
                    code: true,
                    ..style
                },
                url: None,
            });
            rest = &after[end + 1..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('[')
            && let Some((label, target)) = after.split_once("](")
            && let Some(end) = target.find(')')
        {
            let url = &target[..end];
            if safe_link(url) {
                flush(&mut plain, &mut result);
                result.push(Span {
                    text: label.into(),
                    style,
                    url: Some(url.into()),
                });
                rest = &target[end + 1..];
                continue;
            }
        }
        let marker = if rest.starts_with("**") {
            Some(("**", true))
        } else if rest.starts_with('*') {
            Some(("*", false))
        } else {
            None
        };
        if let Some((marker, strong)) = marker
            && let Some(end) = rest[marker.len()..].find(marker)
        {
            flush(&mut plain, &mut result);
            let mut nested = style;
            if strong {
                nested.strong = true;
            } else {
                nested.italic = true;
            }
            result.extend(spans(&rest[marker.len()..marker.len() + end], nested));
            rest = &rest[marker.len() + end + marker.len()..];
            continue;
        }
        let c = rest.chars().next().unwrap();
        plain.push(c);
        rest = &rest[c.len_utf8()..];
    }
    flush(&mut plain, &mut result);
    result
}
fn safe_link(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https" | "mailto"))
}
fn inline(text: &str) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    for span in spans(text, Style::default()) {
        job.append(
            &span.text,
            0.0,
            TextFormat {
                font_id: if span.style.code {
                    FontId::monospace(14.0)
                } else {
                    FontId::proportional(16.0)
                },
                color: super::palette::p().ink,
                extra_letter_spacing: if span.style.strong { 0.25 } else { 0.0 },
                background: if span.style.code {
                    super::palette::p().surface_2
                } else {
                    Color32::TRANSPARENT
                },
                italics: span.style.italic,
                ..Default::default()
            },
        );
    }
    job
}
fn paragraph(ui: &mut egui::Ui, text: &str) {
    let parts = spans(text, Style::default());
    if !parts.iter().any(|s| s.url.is_some()) {
        ui.add(egui::Label::new(inline(text)).selectable(true).wrap());
    } else {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for span in parts {
                if let Some(url) = span.url {
                    ui.hyperlink_to(span.text, url);
                } else {
                    let mut text = RichText::new(span.text);
                    if span.style.strong {
                        text = text.strong();
                    }
                    if span.style.italic {
                        text = text.italics();
                    }
                    if span.style.code {
                        text = text
                            .monospace()
                            .background_color(super::palette::p().surface_2);
                    }
                    ui.add(egui::Label::new(text).selectable(true).wrap());
                }
            }
        });
    }
}
fn cells(line: &str) -> Vec<&str> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .collect()
}
fn table_separator(line: &str) -> bool {
    let columns = cells(line);
    !columns.is_empty()
        && columns
            .iter()
            .all(|s| s.trim_matches(':').len() >= 3 && s.chars().all(|c| c == '-' || c == ':'))
}
fn list_item(line: &str) -> Option<(&str, &str)> {
    let text = line.trim_start();
    if text.starts_with("- ") || text.starts_with("* ") || text.starts_with("+ ") {
        return Some(("•", &text[2..]));
    }
    let n = text.chars().take_while(char::is_ascii_digit).count();
    if n > 0 && text[n..].starts_with(". ") {
        return Some((&text[..n + 1], &text[n + 2..]));
    }
    None
}
pub(super) fn render(ui: &mut egui::Ui, text: &str) {
    let lines: Vec<_> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(language) = line.trim_start().strip_prefix("```") {
            i += 1;
            let start = i;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                i += 1;
            }
            let mut code = lines[start..i].join("\n");
            if !code.is_empty() {
                code.push('\n');
            }
            code_block(ui, language.trim(), &code);
            if i < lines.len() {
                i += 1;
            }
        } else if line.trim_start().starts_with("<svg")
            || (line.trim_start().starts_with("<?xml")
                && lines[i..].iter().take(4).any(|l| l.contains("<svg")))
        {
            let start = i;
            let mut found_end = false;
            while i < lines.len() {
                let l = lines[i];
                i += 1;
                if l.contains("</svg>") {
                    found_end = true;
                    break;
                }
            }
            let svg_code = lines[start..i].join("\n");
            if found_end {
                code_block(ui, "svg", &svg_code);
            } else {
                code_block(ui, "svg", &svg_code);
            }
        } else if i + 1 < lines.len() && line.contains('|') && table_separator(lines[i + 1]) {
            let headers = cells(line);
            i += 2;
            let start = i;
            while i < lines.len() && lines[i].contains('|') && !lines[i].trim().is_empty() {
                i += 1;
            }
            egui::ScrollArea::horizontal()
                .id_salt(ui.next_auto_id())
                .show(ui, |ui| {
                    egui::Grid::new(ui.next_auto_id())
                        .striped(true)
                        .max_col_width(240.0)
                        .show(ui, |ui| {
                            for h in &headers {
                                ui.label(RichText::new(*h).strong());
                            }
                            ui.end_row();
                            for row in &lines[start..i] {
                                let columns = cells(row);
                                for n in 0..headers.len() {
                                    paragraph(ui, columns.get(n).copied().unwrap_or(""));
                                }
                                ui.end_row();
                            }
                        });
                });
        } else if line.trim().is_empty() {
            ui.add_space(6.0);
            i += 1;
        } else if let Some((marker, text)) = list_item(line) {
            ui.horizontal_wrapped(|ui| {
                ui.label(marker);
                paragraph(ui, text);
            });
            i += 1;
        } else if let Some(quote) = line.strip_prefix("> ") {
            egui::Frame::new()
                .inner_margin(10)
                .stroke(Stroke::new(1.0, super::palette::p().line))
                .show(ui, |ui| paragraph(ui, quote));
            i += 1;
        } else {
            let n = line.chars().take_while(|&c| c == '#').count();
            if (1..=6).contains(&n) && line[n..].starts_with(' ') {
                ui.add(
                    egui::Label::new(
                        RichText::new(inline(line[n..].trim()).text)
                            .strong()
                            .size((28.0 - n as f32 * 2.0).max(17.0)),
                    )
                    .selectable(true)
                    .wrap(),
                );
                i += 1;
            } else {
                let start = i;
                i += 1;
                while i < lines.len()
                    && !lines[i].trim().is_empty()
                    && !lines[i].starts_with(['#', '>', '`'])
                    && list_item(lines[i]).is_none()
                    && !(i + 1 < lines.len() && table_separator(lines[i + 1]))
                {
                    i += 1;
                }
                paragraph(ui, &lines[start..i].join("\n"));
            }
        }
    }
}
fn code_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn code_block(ui: &mut egui::Ui, lang: &str, code: &str) {
    let ui_lang = ui
        .ctx()
        .data(|d| d.get_temp::<super::i18n::Lang>(egui::Id::new("kaic-lang")))
        .unwrap_or(super::i18n::Lang::En);
    let is_svg_lang = lang.eq_ignore_ascii_case("svg")
        || ((lang.is_empty()
            || lang.eq_ignore_ascii_case("xml")
            || lang.eq_ignore_ascii_case("html"))
            && crate::attachments::is_svg_bytes(code.as_bytes()));
    if is_svg_lang && svg_block(ui, code, ui_lang) {
        return;
    }
    egui::Frame::new()
        .fill(super::palette::p().surface_2)
        .stroke(Stroke::new(1.0, super::palette::p().line))
        .corner_radius(9)
        .inner_margin(12)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(if lang.is_empty() {
                        ui_lang.t("Code", "Код")
                    } else {
                        lang
                    })
                    .small()
                    .weak(),
                );
                icons::copy_button(ui, ui.id().with(("copy-code", code)), code, ui_lang);
            });
            egui::ScrollArea::horizontal()
                .id_salt(ui.next_auto_id())
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(code.trim_end()).monospace().size(14.0))
                            .selectable(true)
                            .extend(),
                    );
                });
        });
}

fn svg_block(ui: &mut egui::Ui, code: &str, ui_lang: super::i18n::Lang) -> bool {
    let hash = code_hash(code);
    let tex_id = egui::Id::new(("kaic-svg-tex", hash));
    let ctx = ui.ctx().clone();
    let cached: Option<Option<egui::TextureHandle>> = ctx.data(|d| d.get_temp(tex_id));
    let tex_opt = match cached {
        Some(v) => v,
        None => {
            let built = crate::attachments::render_svg_premultiplied(code.as_bytes(), 1200)
                .ok()
                .map(|(w, h, rgba)| {
                    ctx.load_texture(
                        format!("chat-svg-{hash}"),
                        egui::ColorImage::from_rgba_premultiplied([w as usize, h as usize], &rgba),
                        egui::TextureOptions::LINEAR,
                    )
                });
            ctx.data_mut(|d| d.insert_temp(tex_id, built.clone()));
            built
        }
    };
    let Some(tex) = tex_opt else {
        return false;
    };

    let mode_id = egui::Id::new(("kaic-svg-show-code", hash));
    let mut show_code = ctx.data(|d| d.get_temp::<bool>(mode_id)).unwrap_or(false);
    let pal = super::palette::p();
    let [tw, th] = tex.size();

    egui::Frame::new()
        .fill(pal.surface_2)
        .stroke(Stroke::new(1.0, pal.line))
        .corner_radius(9)
        .inner_margin(10)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("SVG · {tw}×{th}"))
                        .small()
                        .color(pal.ink_muted),
                );
                let toggle_lbl = if show_code {
                    ui_lang.t("Image", "Картинка")
                } else {
                    ui_lang.t("Code", "Код")
                };
                if ui.small_button(toggle_lbl).clicked() {
                    show_code = !show_code;
                    ctx.data_mut(|d| d.insert_temp(mode_id, show_code));
                }
                icons::copy_button(ui, ui.id().with(("copy-svg", hash)), code, ui_lang);
            });
            ui.add_space(6.0);
            if show_code {
                egui::ScrollArea::horizontal()
                    .id_salt(ui.next_auto_id())
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(code.trim_end()).monospace().size(13.5))
                                .selectable(true)
                                .extend(),
                        );
                    });
            } else {
                let max_w = ui.available_width().min(540.0).max(120.0);
                let aspect = (tw as f32 / (th as f32).max(1.0)).max(0.05);
                let mut disp_w = (tw as f32).min(max_w);
                let mut disp_h = disp_w / aspect;
                if disp_h > 420.0 {
                    disp_h = 420.0;
                    disp_w = disp_h * aspect;
                }
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(245, 243, 238))
                    .corner_radius(7)
                    .inner_margin(8)
                    .show(ui, |ui| {
                        ui.image((tex.id(), egui::vec2(disp_w, disp_h)));
                    });
            }
        });
    true
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finish_markdown_code_preserves_literal_markers() {
        assert_eq!(inline("`**literal**`").text, "**literal**");
    }
    #[test]
    fn finish_markdown_styles_links_lists_tables() {
        let formatted = spans(
            "**сильно** *наклон* `код` [сайт](https://example.com)",
            Style::default(),
        );
        assert!(formatted.iter().any(|s| s.style.strong));
        assert!(formatted.iter().any(|s| s.style.italic));
        assert!(formatted.iter().any(|s| s.style.code));
        assert!(
            formatted
                .iter()
                .any(|s| s.url.as_deref() == Some("https://example.com"))
        );
        assert!(!safe_link("javascript:alert(1)"));
        assert_eq!(list_item("12. пункт"), Some(("12.", "пункт")));
        assert!(table_separator("| :--- | ---: |"));
        assert!(!table_separator("| text | --- |"));
        assert_eq!(inline("\\*буквально\\* `a*b`").text, "*буквально* a*b");
    }
}
