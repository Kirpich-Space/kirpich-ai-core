//! Официальная палитра KAIC (Carbon — тёмная тема), загружаемая напрямую из
//! `KAIC/Desige/kaic-palette.json` (копия в `assets/kaic-palette.json`).
//! Ржаво-оранжевый (`brand`) используется как редкий акцент: знак, главная активная кнопка,
//! индикатор активной генерации. Красный (`err` / `control-live`) — для удаления и ошибок.
use eframe::egui::Color32;
use serde_json::Value;
use std::sync::OnceLock;

const PALETTE_JSON: &str = include_str!("../../assets/kaic-palette.json");

#[derive(Clone, Copy, Debug)]
pub struct KaicPalette {
    pub surface_0: Color32,
    pub surface_1: Color32,
    pub surface_2: Color32,
    pub line: Color32,
    pub line_strong: Color32,
    pub ink: Color32,
    pub ink_muted: Color32,
    pub brand: Color32,
    pub brand_text: Color32,
    pub on_brand: Color32,
    pub ceramic_200: Color32,
    pub selection: Color32,
    pub local: Color32,
    pub cloud: Color32,
    pub agent_3: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub err: Color32,
}

impl KaicPalette {
    /// Фирменный цвет агента 1..4 из `kaic-palette.json` (всегда подписывать именем агента).
    pub fn agent_color(&self, slot: u8) -> Color32 {
        match slot % 4 {
            0 => self.brand_text, // agent-1: #E8784A
            1 => self.local,      // agent-2: #5FC2B0
            2 => self.agent_3,    // agent-3: #E6C35C
            _ => self.cloud,      // agent-4: #8FB4F2
        }
    }
}

fn parse_hex(hex: &str) -> Color32 {
    let h = hex.trim().trim_start_matches('#');
    let r = u8::from_str_radix(&h[0..2], 16).expect("valid hex R");
    let g = u8::from_str_radix(&h[2..4], 16).expect("valid hex G");
    let b = u8::from_str_radix(&h[4..6], 16).expect("valid hex B");
    Color32::from_rgb(r, g, b)
}

fn find_token(v: &Value, section: &str, name: &str, field: &str) -> Color32 {
    let hex = v[section]
        .as_array()
        .and_then(|arr| arr.iter().find(|e| e["name"].as_str() == Some(name)))
        .and_then(|e| e[field].as_str())
        .unwrap_or_else(|| panic!("Токен {section}.{name}.{field} не найден в kaic-palette.json"));
    parse_hex(hex)
}

pub fn p() -> &'static KaicPalette {
    static PALETTE: OnceLock<KaicPalette> = OnceLock::new();
    PALETTE.get_or_init(|| {
        let v: Value =
            serde_json::from_str(PALETTE_JSON).expect("Некорректный JSON kaic-palette.json");
        KaicPalette {
            surface_0: find_token(&v, "ui", "surface-0", "dark"),
            surface_1: find_token(&v, "ui", "surface-1", "dark"),
            surface_2: find_token(&v, "ui", "surface-2", "dark"),
            line: find_token(&v, "ui", "line", "dark"),
            line_strong: find_token(&v, "ui", "line-strong", "dark"),
            ink: find_token(&v, "ui", "ink", "dark"),
            ink_muted: find_token(&v, "ui", "ink-muted", "dark"),
            brand: find_token(&v, "ui", "brand", "dark"),
            brand_text: find_token(&v, "ui", "brand-text", "dark"),
            on_brand: find_token(&v, "ui", "on-brand", "dark"),
            ceramic_200: find_token(&v, "primitives", "ceramic-200", "hex"),
            selection: find_token(&v, "ui", "selection", "dark"),
            local: find_token(&v, "states", "model-local", "dark"),
            cloud: find_token(&v, "states", "model-cloud", "dark"),
            agent_3: find_token(&v, "agents", "agent-3", "dark"),
            ok: find_token(&v, "states", "signal-ok", "dark"),
            warn: find_token(&v, "states", "signal-warn", "dark"),
            err: find_token(&v, "states", "signal-error", "dark"),
        }
    })
}

pub const SURFACE_0: Color32 = Color32::from_rgb(0x0B, 0x0B, 0x0C);
pub const SURFACE_1: Color32 = Color32::from_rgb(0x14, 0x14, 0x16);
pub const SURFACE_2: Color32 = Color32::from_rgb(0x1C, 0x1C, 0x20);
pub const LINE: Color32 = Color32::from_rgb(0x2A, 0x2A, 0x2F);
pub const LINE_STRONG: Color32 = Color32::from_rgb(0x6E, 0x6E, 0x76);
pub const INK: Color32 = Color32::from_rgb(0xF4, 0xF1, 0xEC);
pub const INK_MUTED: Color32 = Color32::from_rgb(0xA8, 0xA6, 0xA2);
pub const BRAND: Color32 = Color32::from_rgb(0xD2, 0x60, 0x2C);
pub const ON_BRAND: Color32 = Color32::from_rgb(0x0B, 0x0B, 0x0C);
pub const CERAMIC_200: Color32 = Color32::from_rgb(0xF0, 0xC9, 0xAE);
#[cfg_attr(not(test), allow(dead_code))]
pub const SELECTION: Color32 = Color32::from_rgb(0x2D, 0x1A, 0x10);
#[cfg_attr(not(test), allow(dead_code))]
pub const LOCAL: Color32 = Color32::from_rgb(0x5F, 0xC2, 0xB0);
#[cfg_attr(not(test), allow(dead_code))]
pub const OK: Color32 = Color32::from_rgb(0x5D, 0xB5, 0x7A);
#[cfg_attr(not(test), allow(dead_code))]
pub const WARN: Color32 = Color32::from_rgb(0xE5, 0xAE, 0x45);
pub const ERR: Color32 = Color32::from_rgb(0xF2, 0x64, 0x5F);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_loads_from_kaic_json_and_matches_tokens() {
        let pal = p();
        assert_eq!(pal.surface_0, SURFACE_0);
        assert_eq!(pal.surface_1, SURFACE_1);
        assert_eq!(pal.surface_2, SURFACE_2);
        assert_eq!(pal.line, LINE);
        assert_eq!(pal.line_strong, LINE_STRONG);
        assert_eq!(pal.ink, INK);
        assert_eq!(pal.ink_muted, INK_MUTED);
        assert_eq!(pal.brand, BRAND);
        assert_eq!(pal.on_brand, ON_BRAND);
        assert_eq!(pal.ceramic_200, CERAMIC_200);
        assert_eq!(pal.selection, SELECTION);
        assert_eq!(pal.local, LOCAL);
        assert_eq!(pal.ok, OK);
        assert_eq!(pal.warn, WARN);
        assert_eq!(pal.err, ERR);
    }
}
