//! Официальный логотип и знак KAIC для терминального интерфейса (`Desige/kaic-mark.svg`,
//! `Desige/kaic-wordmark-on-dark.svg`, `Desige/kaic-wordmark-on-light.svg`).
//!
//! Настольный маскот зарезервирован исключительно за GUI (`Studio/src/ui/`)
//! и полностью отсутствует в CLI и TUI.
//!
//! Поддерживается каскад протоколов отображения с плавной деградацией:
//! 1. `KittyGraphics` — встроенный PNG-рендер официального SVG через протокол Kitty (`\x1b_G...`);
//! 2. `Iterm2Inline` — встроенный PNG-рендер официального SVG через OSC 1337 (`iTerm2` / `WezTerm`);
//! 3. `HalfBlock` — точная растеризация `Studio/assets/kaic-mark.svg` через `resvg` в сетку `10×10`
//!    субпикселей (`5` строк × `10` колонок символов `▀`, пропорция `1:1`) с каноническими цветами
//!    `#D2602C` (`kiln-500`), `#F0C9AE` (`ceramic-200`), `#F4F1EC` (`bone-100`) и `#0B0B0C` (`carbon-950`);
//! 4. `TextFallback` — чистый геометрический знак `[▸▸▸]` без управляющих кодов для `NO_COLOR`,
//!    `--no-color` и голых TTY (`TERM=dumb`).

use super::term::{ColorMode, Rgb, StyleRole, TuiTheme, style_themed, theme_tokens};
use crate::attachments::{base64_encode, render_svg_dynamic_image};
use image::GenericImageView;
use std::sync::OnceLock;

const KAIC_MARK_SVG: &[u8] = include_bytes!("../../assets/kaic-mark.svg");
const KAIC_WORDMARK_DARK_SVG: &[u8] = include_bytes!("../../assets/kaic-wordmark-on-dark.svg");
const KAIC_WORDMARK_LIGHT_SVG: &[u8] = include_bytes!("../../assets/kaic-wordmark-on-light.svg");

const GRID_SIDE: usize = 10;

/// Протокол вывода официального логотипа KAIC в терминале.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogoProtocol {
    KittyGraphics,
    Iterm2Inline,
    HalfBlock,
    TextFallback,
}

impl LogoProtocol {
    /// Определяет оптимальный протокол логотипа для текущего окружения (учитывая SSH и TTY).
    pub fn detect(color: ColorMode) -> Self {
        if color == ColorMode::Plain {
            return Self::TextFallback;
        }
        if let Ok(forced) = std::env::var("KAIC_TUI_GRAPHICS") {
            match forced.trim().to_ascii_lowercase().as_str() {
                "kitty" => return Self::KittyGraphics,
                "iterm2" | "iterm" | "wezterm" => return Self::Iterm2Inline,
                "halfblock" | "unicode" | "blocks" => return Self::HalfBlock,
                "none" | "text" | "ascii" | "off" => return Self::TextFallback,
                _ => {}
            }
        }
        if color == ColorMode::Ansi16 {
            if let Ok(term) = std::env::var("TERM") {
                let low = term.to_ascii_lowercase();
                if low == "vt100" || low == "vt220" {
                    return Self::TextFallback;
                }
            }
            return Self::HalfBlock;
        }
        // По SSH без явного KAIC_TUI_GRAPHICS используем надёжный HalfBlock без графических escape-запросов.
        let over_ssh = std::env::var_os("SSH_CONNECTION").is_some()
            || std::env::var_os("SSH_TTY").is_some()
            || std::env::var_os("TMUX").is_some();
        if !over_ssh {
            let term = std::env::var("TERM")
                .unwrap_or_default()
                .to_ascii_lowercase();
            let term_prog = std::env::var("TERM_PROGRAM")
                .unwrap_or_default()
                .to_ascii_lowercase();
            if std::env::var_os("KITTY_WINDOW_ID").is_some()
                || term.contains("xterm-kitty")
                || term_prog == "ghostty"
            {
                return Self::KittyGraphics;
            }
            if term_prog == "iterm.app" || term_prog == "wezterm" {
                return Self::Iterm2Inline;
            }
        }
        Self::HalfBlock
    }
}

/// Результат рендера логотипа для компактной приветственной секции.
#[derive(Clone, Debug)]
pub struct RenderedLogo {
    /// Видимая ширина левой колонки знака в символах терминала.
    pub width_cols: usize,
    /// Построчное представление знака (для `Kitty`/`iTerm2` последовательность помещается в первую строку,
    /// остальные строки резервируют геометрию ячейки).
    pub lines: Vec<String>,
    /// Скрывает ли графический рендер текстовый wordmark `KAIC` (так как SVG wordmark уже содержит `KAIC`).
    pub includes_wordmark: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkCell {
    Transparent,
    Kiln,    // #D2602C
    Ceramic, // #F0C9AE
    Bone,    // #F4F1EC
    Carbon,  // #0B0B0C
}

impl MarkCell {
    fn rgb(self) -> Option<Rgb> {
        match self {
            Self::Transparent => None,
            Self::Kiln => Some((0xD2, 0x60, 0x2C)),
            Self::Ceramic => Some((0xF0, 0xC9, 0xAE)),
            Self::Bone => Some((0xF4, 0xF1, 0xEC)),
            Self::Carbon => Some((0x0B, 0x0B, 0x0C)),
        }
    }
}

fn classify_mark_pixel(r: u8, g: u8, b: u8, a: u8) -> MarkCell {
    if a < 128 {
        return MarkCell::Transparent;
    }
    let candidates = [
        (MarkCell::Kiln, (0xD2i32, 0x60i32, 0x2Ci32)),
        (MarkCell::Ceramic, (0xF0, 0xC9, 0xAE)),
        (MarkCell::Bone, (0xF4, 0xF1, 0xEC)),
        (MarkCell::Carbon, (0x0B, 0x0B, 0x0C)),
    ];
    let mut best = MarkCell::Kiln;
    let mut best_dist = i32::MAX;
    for (cell, (cr, cg, cb)) in candidates {
        let dr = r as i32 - cr;
        let dg = g as i32 - cg;
        let db = b as i32 - cb;
        let dist = dr * dr + dg * dg + db * db;
        if dist < best_dist {
            best_dist = dist;
            best = cell;
        }
    }
    best
}

fn rasterized_mark_grid() -> &'static [[MarkCell; GRID_SIDE]; GRID_SIDE] {
    static GRID: OnceLock<[[MarkCell; GRID_SIDE]; GRID_SIDE]> = OnceLock::new();
    GRID.get_or_init(|| {
        let mut out = [[MarkCell::Transparent; GRID_SIDE]; GRID_SIDE];
        if let Ok(img) = render_svg_dynamic_image(KAIC_MARK_SVG, 320) {
            let (w, h) = img.dimensions();
            for (gy, row) in out.iter_mut().enumerate() {
                let py = (((gy as f32 + 0.5) * h as f32) / GRID_SIDE as f32)
                    .floor()
                    .clamp(0.0, h.saturating_sub(1) as f32) as u32;
                for (gx, slot) in row.iter_mut().enumerate() {
                    let px = (((gx as f32 + 0.5) * w as f32) / GRID_SIDE as f32)
                        .floor()
                        .clamp(0.0, w.saturating_sub(1) as f32) as u32;
                    let rgba = img.get_pixel(px, py).0;
                    *slot = classify_mark_pixel(rgba[0], rgba[1], rgba[2], rgba[3]);
                }
            }
        }
        out
    })
}

fn wordmark_png_base64(theme: TuiTheme) -> Option<&'static str> {
    static DARK_B64: OnceLock<Option<String>> = OnceLock::new();
    static LIGHT_B64: OnceLock<Option<String>> = OnceLock::new();
    let slot = match theme {
        TuiTheme::Carbon => &DARK_B64,
        TuiTheme::Bone => &LIGHT_B64,
    };
    let svg = match theme {
        TuiTheme::Carbon => KAIC_WORDMARK_DARK_SVG,
        TuiTheme::Bone => KAIC_WORDMARK_LIGHT_SVG,
    };
    slot.get_or_init(|| {
        let img = render_svg_dynamic_image(svg, 420).ok()?;
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .ok()?;
        Some(base64_encode(&png))
    })
    .as_deref()
}

/// Рендерит официальный знак KAIC с учётом темы (`Carbon` / `Bone`), ширины терминала и протокола.
pub fn render_official_logo(
    color: ColorMode,
    theme: TuiTheme,
    protocol: LogoProtocol,
    cols: usize,
) -> RenderedLogo {
    if color == ColorMode::Plain || protocol == LogoProtocol::TextFallback || cols < 54 {
        let mark = if color == ColorMode::Plain {
            "[▸▸▸]".to_string()
        } else {
            format!(
                "{}{}{}",
                style_themed(StyleRole::CeramicTrail, color, theme, "▸"),
                style_themed(StyleRole::Ink, color, theme, "▸"),
                style_themed(StyleRole::BrandBadge, color, theme, " ▸ "),
            )
        };
        let width_cols = 5;
        return RenderedLogo {
            width_cols,
            lines: vec![mark],
            includes_wordmark: false,
        };
    }

    match protocol {
        LogoProtocol::KittyGraphics if cols >= 72 => {
            if let Some(b64) = wordmark_png_base64(theme) {
                let w_cells = 16usize;
                let h_cells = 4usize;
                // Разбиваем payload чанками <= 4096 байт по спецификации Kitty graphics protocol
                let bytes = b64.as_bytes();
                let mut seq = String::with_capacity(b64.len() + 128);
                let chunks: Vec<&[u8]> = bytes.chunks(4096).collect();
                for (i, ch) in chunks.iter().enumerate() {
                    let more = if i + 1 < chunks.len() { 1 } else { 0 };
                    let s = std::str::from_utf8(ch).unwrap_or("");
                    if i == 0 {
                        seq.push_str(&format!(
                            "\x1b_Ga=T,f=100,c={w_cells},r={h_cells},C=1,q=2,m={more};{s}\x1b\\"
                        ));
                    } else {
                        seq.push_str(&format!("\x1b_Gm={more};{s}\x1b\\"));
                    }
                }
                let mut lines = Vec::with_capacity(h_cells);
                lines.push(format!("{seq}{}", " ".repeat(w_cells)));
                for _ in 1..h_cells {
                    lines.push(" ".repeat(w_cells));
                }
                return RenderedLogo {
                    width_cols: w_cells,
                    lines,
                    includes_wordmark: true,
                };
            }
        }
        LogoProtocol::Iterm2Inline if cols >= 72 => {
            if let Some(b64) = wordmark_png_base64(theme) {
                let w_cells = 16usize;
                let h_cells = 4usize;
                let seq = format!(
                    "\x1b]1337;File=inline=1;width={w_cells};height={h_cells};preserveAspectRatio=1:{b64}\x07"
                );
                let mut lines = Vec::with_capacity(h_cells);
                lines.push(seq);
                for _ in 1..h_cells {
                    lines.push(" ".repeat(w_cells));
                }
                return RenderedLogo {
                    width_cols: w_cells,
                    lines,
                    includes_wordmark: true,
                };
            }
        }
        _ => {}
    }

    render_halfblock_mark(color, theme)
}

fn ansi16_fg(cell: MarkCell) -> &'static str {
    match cell {
        MarkCell::Transparent => "39",
        MarkCell::Kiln => "33",
        MarkCell::Ceramic => "37",
        MarkCell::Bone => "97",
        MarkCell::Carbon => "30",
    }
}

fn ansi16_bg(cell: MarkCell) -> &'static str {
    match cell {
        MarkCell::Transparent => "49",
        MarkCell::Kiln => "43",
        MarkCell::Ceramic => "47",
        MarkCell::Bone => "107",
        MarkCell::Carbon => "40",
    }
}

fn render_halfblock_mark(color: ColorMode, theme: TuiTheme) -> RenderedLogo {
    let grid = rasterized_mark_grid();
    let surface = theme_tokens(theme).surface_0;
    let rows_count = GRID_SIDE / 2;
    let mut lines = Vec::with_capacity(rows_count);

    for pair in 0..rows_count {
        let top_row = &grid[pair * 2];
        let bot_row = &grid[pair * 2 + 1];
        let mut line = String::with_capacity(GRID_SIDE * 28);
        for x in 0..GRID_SIDE {
            let top = top_row[x];
            let bot = bot_row[x];
            match (top, bot) {
                (MarkCell::Transparent, MarkCell::Transparent) => {
                    line.push_str("\x1b[0m ");
                }
                (t, MarkCell::Transparent) => match color {
                    ColorMode::TrueColor => {
                        let (r, g, b) = t.rgb().unwrap_or(surface);
                        line.push_str(&format!("\x1b[0;38;2;{r};{g};{b}m▀"));
                    }
                    ColorMode::Ansi16 => {
                        line.push_str(&format!("\x1b[0;{}m▀", ansi16_fg(t)));
                    }
                    ColorMode::Plain => line.push('▀'),
                },
                (MarkCell::Transparent, b_cell) => match color {
                    ColorMode::TrueColor => {
                        let (r, g, b) = b_cell.rgb().unwrap_or(surface);
                        line.push_str(&format!("\x1b[0;38;2;{r};{g};{b}m▄"));
                    }
                    ColorMode::Ansi16 => {
                        line.push_str(&format!("\x1b[0;{}m▄", ansi16_fg(b_cell)));
                    }
                    ColorMode::Plain => line.push('▄'),
                },
                (t, b_cell) if t == b_cell => match color {
                    ColorMode::TrueColor => {
                        let (r, g, b) = t.rgb().unwrap_or(surface);
                        line.push_str(&format!("\x1b[0;38;2;{r};{g};{b}m█"));
                    }
                    ColorMode::Ansi16 => {
                        line.push_str(&format!("\x1b[0;{}m█", ansi16_fg(t)));
                    }
                    ColorMode::Plain => line.push('█'),
                },
                (t, b_cell) => match color {
                    ColorMode::TrueColor => {
                        let (tr, tg, tb) = t.rgb().unwrap_or(surface);
                        let (br, bg, bb) = b_cell.rgb().unwrap_or(surface);
                        line.push_str(&format!("\x1b[0;38;2;{tr};{tg};{tb};48;2;{br};{bg};{bb}m▀"));
                    }
                    ColorMode::Ansi16 => {
                        line.push_str(&format!("\x1b[0;{};{}m▀", ansi16_fg(t), ansi16_bg(b_cell)));
                    }
                    ColorMode::Plain => line.push('▀'),
                },
            }
        }
        line.push_str("\x1b[0m");
        lines.push(line);
    }

    RenderedLogo {
        width_cols: GRID_SIDE,
        lines,
        includes_wordmark: false,
    }
}

/// Официальный текстовый wordmark `KAIC` с цветами дизайн-системы `Desige/kaic-palette.json`.
pub fn render_wordmark_inline(color: ColorMode, theme: TuiTheme) -> String {
    if color == ColorMode::Plain {
        return "KAIC".to_string();
    }
    style_themed(StyleRole::Ink, color, theme, "KAIC")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::term::{display_width, strip_ansi_visible};

    #[test]
    fn halfblock_logo_rasterizes_official_kaic_mark_svg_with_exact_palette_colors() {
        let rendered = render_official_logo(
            ColorMode::TrueColor,
            TuiTheme::Carbon,
            LogoProtocol::HalfBlock,
            100,
        );
        assert_eq!(rendered.width_cols, 10);
        assert_eq!(rendered.lines.len(), 5);
        assert!(!rendered.includes_wordmark);

        for line in &rendered.lines {
            let visible = strip_ansi_visible(line);
            assert_eq!(display_width(&visible), 10);
        }

        let joined = rendered.lines.join("\n");
        // Официальный kiln-500 (#D2602C -> 210;96;44)
        assert!(joined.contains("210;96;44"));
        // Каскад из 3 курсоров из kaic-mark.svg: ceramic-200 (#F0C9AE), bone-100 (#F4F1EC), carbon-950 (#0B0B0C)
        assert!(joined.contains("240;201;174"));
        assert!(joined.contains("244;241;236"));
        assert!(joined.contains("11;11;12"));
    }

    #[test]
    fn kitty_and_iterm2_protocols_encode_official_wordmark_svg_and_plain_has_no_ansi() {
        let kitty = render_official_logo(
            ColorMode::TrueColor,
            TuiTheme::Carbon,
            LogoProtocol::KittyGraphics,
            100,
        );
        assert!(kitty.includes_wordmark);
        assert!(kitty.lines[0].starts_with("\x1b_Ga=T,f=100"));

        let iterm = render_official_logo(
            ColorMode::TrueColor,
            TuiTheme::Bone,
            LogoProtocol::Iterm2Inline,
            100,
        );
        assert!(iterm.includes_wordmark);
        assert!(iterm.lines[0].starts_with("\x1b]1337;File=inline=1"));

        let plain = render_official_logo(
            ColorMode::Plain,
            TuiTheme::Carbon,
            LogoProtocol::TextFallback,
            80,
        );
        assert_eq!(plain.lines, vec!["[▸▸▸]".to_string()]);
        assert!(!plain.lines[0].contains('\x1b'));
    }
}
