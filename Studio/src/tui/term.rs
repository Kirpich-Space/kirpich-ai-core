//! Низкоуровневый POSIX-терминал и официальная дизайн-система KAIC TUI:
//! - Загружает точные цветовые токены обеих тем (`Carbon` и `Bone`) напрямую из
//!   `Desige/kaic-palette.json` (`assets/kaic-palette.json`), не изобретая сторонних цветов;
//! - RAII `RawTerminalGuard` (`termios`, bracketed paste) БЕЗ alternate screen (`\x1b[?1049h`) —
//!   диалог выводится inline в основной буфер терминала с нативным scrollback;
//! - `InlineViewport` — дифференциальный inline-движок отрисовки живой области и scrollback;
//! - Инкрементальный парсер UTF-8, CSI/SS3, Alt/Shift+Enter, bracketed paste и `Esc`-различения;
//! - Unicode-ширина, перенос строк и санитизация ANSI-последовательностей из недоверенного вывода.

use serde_json::Value;
use std::io::{self, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

const PALETTE_JSON: &str = include_str!("../../assets/kaic-palette.json");

static RESIZE_PENDING: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn handle_sigwinch(_sig: libc::c_int) {
    RESIZE_PENDING.store(true, Ordering::Relaxed);
}

pub fn install_sigwinch_handler() {
    #[cfg(unix)]
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_sigwinch as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGWINCH, &sa, std::ptr::null_mut());
    }
}

pub fn take_resize_flag() -> bool {
    RESIZE_PENDING.swap(false, Ordering::SeqCst)
}

pub fn is_tty() -> bool {
    #[cfg(unix)]
    unsafe {
        libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDOUT_FILENO) == 1
    }
    #[cfg(not(unix))]
    {
        false
    }
}

pub fn terminal_size() -> (u16, u16) {
    #[cfg(unix)]
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0
            && ws.ws_col >= 20
            && ws.ws_row >= 6
        {
            return (ws.ws_col, ws.ws_row);
        }
    }
    (80, 24)
}

pub fn wait_stdin_ready(timeout_ms: i32) -> bool {
    #[cfg(unix)]
    unsafe {
        let mut pfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = libc::poll(&mut pfd, 1, timeout_ms);
        rc > 0 && (pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0
    }
    #[cfg(not(unix))]
    {
        let _ = timeout_ms;
        false
    }
}

pub fn read_stdin_nonblocking(buf: &mut [u8]) -> io::Result<usize> {
    #[cfg(unix)]
    unsafe {
        let n = libc::read(
            libc::STDIN_FILENO,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
        );
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
    #[cfg(not(unix))]
    {
        let _ = buf;
        Ok(0)
    }
}

/// Режим цветопередачи терминала.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    TrueColor,
    Ansi16,
    Plain,
}

impl ColorMode {
    pub fn detect(force_plain: bool) -> Self {
        if force_plain
            || std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
            || std::env::var("TERM").is_ok_and(|t| t.eq_ignore_ascii_case("dumb"))
        {
            return Self::Plain;
        }
        if let Ok(ct) = std::env::var("COLORTERM") {
            let low = ct.to_ascii_lowercase();
            if low.contains("truecolor") || low.contains("24bit") {
                return Self::TrueColor;
            }
        }
        if let Ok(term) = std::env::var("TERM") {
            let low = term.to_ascii_lowercase();
            if low == "linux" || low == "vt100" || low == "vt220" {
                return Self::Ansi16;
            }
            if low.contains("256color") || low.contains("kitty") || low.contains("alacritty") {
                return Self::TrueColor;
            }
        }
        Self::Ansi16
    }
}

/// Официальные темы из `Desige/kaic-palette.json`: `Carbon (основная)` и `Bone`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TuiTheme {
    #[default]
    Carbon,
    Bone,
}

impl TuiTheme {
    pub fn parse_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "carbon" | "dark" => Some(Self::Carbon),
            "bone" | "light" => Some(Self::Bone),
            _ => None,
        }
    }

    pub fn label_ru(self) -> &'static str {
        match self {
            Self::Carbon => "Carbon (основная тёмная)",
            Self::Bone => "Bone (светлая)",
        }
    }
}

pub type Rgb = (u8, u8, u8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThemeTokens {
    pub surface_0: Rgb,
    pub surface_1: Rgb,
    pub surface_2: Rgb,
    pub line: Rgb,
    pub line_strong: Rgb,
    pub ink: Rgb,
    pub ink_muted: Rgb,
    pub brand: Rgb,
    pub brand_text: Rgb,
    pub brand_fill: Rgb,
    pub on_brand: Rgb,
    pub ceramic_200: Rgb,
    pub selection: Rgb,
    pub agent_1: Rgb,
    pub agent_2: Rgb,
    pub agent_3: Rgb,
    pub agent_4: Rgb,
    pub model_local: Rgb,
    pub model_cloud: Rgb,
    pub notify: Rgb,
    pub control_live: Rgb,
    pub signal_ok: Rgb,
    pub signal_warn: Rgb,
    pub signal_error: Rgb,
}

struct PaletteStore {
    carbon: ThemeTokens,
    bone: ThemeTokens,
}

fn parse_hex_rgb(hex: &str) -> Rgb {
    let h = hex.trim().trim_start_matches('#');
    let r = u8::from_str_radix(&h[0..2], 16).expect("valid hex R");
    let g = u8::from_str_radix(&h[2..4], 16).expect("valid hex G");
    let b = u8::from_str_radix(&h[4..6], 16).expect("valid hex B");
    (r, g, b)
}

fn token_rgb(v: &Value, section: &str, name: &str, field: &str) -> Rgb {
    let hex = v[section]
        .as_array()
        .and_then(|arr| arr.iter().find(|e| e["name"].as_str() == Some(name)))
        .and_then(|e| e[field].as_str())
        .unwrap_or_else(|| {
            panic!("Токен {section}.{name}.{field} отсутствует в kaic-palette.json")
        });
    parse_hex_rgb(hex)
}

fn load_theme_tokens(v: &Value, key: &str) -> ThemeTokens {
    ThemeTokens {
        surface_0: token_rgb(v, "ui", "surface-0", key),
        surface_1: token_rgb(v, "ui", "surface-1", key),
        surface_2: token_rgb(v, "ui", "surface-2", key),
        line: token_rgb(v, "ui", "line", key),
        line_strong: token_rgb(v, "ui", "line-strong", key),
        ink: token_rgb(v, "ui", "ink", key),
        ink_muted: token_rgb(v, "ui", "ink-muted", key),
        brand: token_rgb(v, "ui", "brand", key),
        brand_text: token_rgb(v, "ui", "brand-text", key),
        brand_fill: token_rgb(v, "ui", "brand-fill", key),
        on_brand: token_rgb(v, "ui", "on-brand", key),
        ceramic_200: token_rgb(v, "primitives", "ceramic-200", "hex"),
        selection: token_rgb(v, "ui", "selection", key),
        agent_1: token_rgb(v, "agents", "agent-1", key),
        agent_2: token_rgb(v, "agents", "agent-2", key),
        agent_3: token_rgb(v, "agents", "agent-3", key),
        agent_4: token_rgb(v, "agents", "agent-4", key),
        model_local: token_rgb(v, "states", "model-local", key),
        model_cloud: token_rgb(v, "states", "model-cloud", key),
        notify: token_rgb(v, "states", "notify", key),
        control_live: token_rgb(v, "states", "control-live", key),
        signal_ok: token_rgb(v, "states", "signal-ok", key),
        signal_warn: token_rgb(v, "states", "signal-warn", key),
        signal_error: token_rgb(v, "states", "signal-error", key),
    }
}

pub fn theme_tokens(theme: TuiTheme) -> &'static ThemeTokens {
    static STORE: OnceLock<PaletteStore> = OnceLock::new();
    let store = STORE.get_or_init(|| {
        let v: Value =
            serde_json::from_str(PALETTE_JSON).expect("Некорректный JSON kaic-palette.json");
        PaletteStore {
            carbon: load_theme_tokens(&v, "dark"),
            bone: load_theme_tokens(&v, "light"),
        }
    });
    match theme {
        TuiTheme::Carbon => &store.carbon,
        TuiTheme::Bone => &store.bone,
    }
}

/// Семантические роли официальной дизайн-системы `Desige/kaic-palette.json`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StyleRole {
    /// Фон окна и терминала (`surface-0`: `#0B0B0C` / `#F4F1EC`).
    Surface0,
    /// Верхняя планка (`surface-1` + `ink`).
    HeaderBar,
    /// Нижняя статусная строка (`surface-1` + `ink-muted`).
    FooterBar,
    /// Карточки задач, попапы, статус-блоки (`surface-2` + `ink`).
    CardSurface,
    /// Инженерный кейкап (`surface-2` + `ink-muted`).
    Keycap,
    /// Фирменная плашка/кнопка KAIC: `brand-fill` `#D2602C` и ЧЁРНЫЙ текст `on-brand` `#0B0B0C` (5.1:1).
    BrandBadge,
    /// Фирменный акцент «обжиг» (`brand` / `kiln-500` `#D2602C`).
    Brand,
    /// Текстовый бренд-акцент (`brand-text` `#E8784A` на тёмном / `#A8431B` на светлом).
    BrandText,
    /// Дальний след курсора в знаке KAIC (`ceramic-200` `#F0C9AE`).
    CeramicTrail,
    /// Выделенная строка попапа/автодополнения (`selection` `#2D1A10` / `#F0C9AE`).
    PopupSelected,
    /// Локальная модель (`model-local` `#5FC2B0` / `#1A6E61`: данные не покидают машину).
    ModelLocal,
    /// Облачная/сетевая модель (`model-cloud` `#8FB4F2` / `#2F5FC0`).
    ModelCloud,
    /// Цвет агента 1..4 из `kaic-palette.json` (`agent-1..agent-4`).
    Agent(u8),
    /// Основной текст (`ink`).
    Ink,
    /// Вторичный текст (`ink-muted`).
    Muted,
    /// Сигнал успеха (`signal-ok` `#5DB57A`).
    Ok,
    /// Сигнал ошибки (`signal-error` `#F2645F`).
    Err,
    /// Требуется подтверждение (`signal-warn` `#E5AE45`).
    Warn,
    /// Сводка / уведомление (`notify` `#E6C35C`).
    Notify,
    /// Тонкие разделители (`line` `#2A2A2F`).
    Border,
    /// Контрастные рамки контролов и фокуса (`line-strong` `#6E6E76`).
    BorderStrong,
    /// Самый заметный сигнал управления курсором/клавиатурой (`control-live` `#F2645F`).
    LiveDanger,
    /// Синтаксис кода и unified diff на базе официальных токенов палитры.
    Keyword,
    StringLit,
    Comment,
    NumberLit,
    DiffAdd,
    DiffDel,
    DiffHunk,
}

fn fg_seq((r, g, b): Rgb, bold: bool) -> String {
    if bold {
        format!("\x1b[1;38;2;{r};{g};{b}m")
    } else {
        format!("\x1b[38;2;{r};{g};{b}m")
    }
}

fn fg_bg_seq((fr, fg, fb): Rgb, (br, bg, bb): Rgb, bold: bool) -> String {
    if bold {
        format!("\x1b[1;38;2;{fr};{fg};{fb};48;2;{br};{bg};{bb}m")
    } else {
        format!("\x1b[38;2;{fr};{fg};{fb};48;2;{br};{bg};{bb}m")
    }
}

impl StyleRole {
    fn truecolor_open(self, t: &ThemeTokens) -> String {
        match self {
            Self::Surface0 => fg_bg_seq(t.ink, t.surface_0, false),
            Self::HeaderBar => fg_bg_seq(t.ink, t.surface_1, false),
            Self::FooterBar => fg_bg_seq(t.ink_muted, t.surface_1, false),
            Self::CardSurface => fg_bg_seq(t.ink, t.surface_2, false),
            Self::Keycap => fg_bg_seq(t.ink_muted, t.surface_2, false),
            Self::BrandBadge => fg_bg_seq(t.on_brand, t.brand_fill, true),
            Self::Brand => fg_seq(t.brand, true),
            Self::BrandText | Self::Keyword => fg_seq(t.brand_text, true),
            Self::CeramicTrail => fg_seq(t.ceramic_200, false),
            Self::PopupSelected => fg_bg_seq(t.ink, t.selection, true),
            Self::ModelLocal | Self::NumberLit | Self::DiffHunk => fg_seq(t.model_local, false),
            Self::ModelCloud => fg_seq(t.model_cloud, false),
            Self::Agent(slot) => {
                let c = match slot % 4 {
                    0 => t.agent_1,
                    1 => t.agent_2,
                    2 => t.agent_3,
                    _ => t.agent_4,
                };
                fg_seq(c, true)
            }
            Self::Ink => fg_seq(t.ink, false),
            Self::Muted => fg_seq(t.ink_muted, false),
            Self::Ok | Self::StringLit | Self::DiffAdd => fg_seq(t.signal_ok, false),
            Self::Err | Self::DiffDel => fg_seq(t.signal_error, false),
            Self::Warn => fg_seq(t.signal_warn, true),
            Self::Notify => fg_seq(t.notify, false),
            Self::Border => fg_seq(t.line, false),
            Self::BorderStrong | Self::Comment => fg_seq(t.line_strong, false),
            Self::LiveDanger => fg_bg_seq(t.on_brand, t.control_live, true),
        }
    }

    fn ansi16_seq(self) -> &'static str {
        match self {
            Self::Surface0 | Self::Ink => "\x1b[0m",
            Self::HeaderBar | Self::FooterBar => "\x1b[7m",
            Self::CardSurface | Self::Keycap => "\x1b[2m",
            Self::BrandBadge => "\x1b[1;30;43m",
            Self::PopupSelected => "\x1b[1;7m",
            Self::Brand | Self::BrandText | Self::Warn | Self::Notify | Self::Keyword => {
                "\x1b[1;33m"
            }
            Self::CeramicTrail => "\x1b[37m",
            Self::ModelLocal | Self::NumberLit | Self::DiffHunk => "\x1b[36m",
            Self::ModelCloud => "\x1b[34m",
            Self::Agent(slot) => match slot % 4 {
                0 => "\x1b[1;33m",
                1 => "\x1b[1;36m",
                2 => "\x1b[33m",
                _ => "\x1b[1;34m",
            },
            Self::Muted | Self::Border | Self::BorderStrong | Self::Comment => "\x1b[2m",
            Self::Ok | Self::StringLit | Self::DiffAdd => "\x1b[32m",
            Self::Err | Self::DiffDel => "\x1b[31m",
            Self::LiveDanger => "\x1b[1;30;41m",
        }
    }
}

pub fn style_themed(role: StyleRole, mode: ColorMode, theme: TuiTheme, text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    match mode {
        ColorMode::Plain => text.to_string(),
        ColorMode::TrueColor => {
            let t = theme_tokens(theme);
            format!("{}{text}\x1b[0m", role.truecolor_open(t))
        }
        ColorMode::Ansi16 => format!("{}{text}\x1b[0m", role.ansi16_seq()),
    }
}

pub fn style(role: StyleRole, mode: ColorMode, text: &str) -> String {
    style_themed(role, mode, TuiTheme::Carbon, text)
}

pub fn surface_clear_seq(mode: ColorMode, theme: TuiTheme) -> String {
    match mode {
        ColorMode::TrueColor => {
            let (r, g, b) = theme_tokens(theme).surface_0;
            format!("\x1b[48;2;{r};{g};{b}m")
        }
        ColorMode::Ansi16 | ColorMode::Plain => String::new(),
    }
}

/// RAII-обёртка над POSIX raw mode + bracketed paste для inline-интерфейса (без alternate screen).
pub struct RawTerminalGuard {
    #[cfg(unix)]
    orig: Option<libc::termios>,
    active: bool,
}

impl RawTerminalGuard {
    pub fn enter() -> io::Result<Self> {
        #[cfg(unix)]
        {
            if !is_tty() {
                return Ok(Self {
                    orig: None,
                    active: false,
                });
            }
            install_sigwinch_handler();
            unsafe {
                let mut orig: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(libc::STDIN_FILENO, &mut orig) != 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut raw = orig;
                raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
                raw.c_iflag &= !(libc::IXON | libc::ICRNL | libc::BRKINT | libc::ISTRIP);
                raw.c_cc[libc::VMIN] = 1;
                raw.c_cc[libc::VTIME] = 0;
                if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut out = io::stdout();
                // Никакого \x1b[?1049h и \x1b[2J: сохраняем нативный scrollback терминала!
                out.write_all(b"\r\x1b[?2004h\x1b[?25l")?;
                out.flush()?;
                Ok(Self {
                    orig: Some(orig),
                    active: true,
                })
            }
        }
        #[cfg(not(unix))]
        {
            Ok(Self { active: false })
        }
    }

    pub fn is_raw(&self) -> bool {
        self.active
    }
}

impl Drop for RawTerminalGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        #[cfg(unix)]
        unsafe {
            let mut out = io::stdout();
            let _ = out.write_all(b"\r\x1b[0m\x1b[?2004l\x1b[?25h");
            let _ = out.flush();
            if let Some(ref orig) = self.orig {
                let _ = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, orig);
            }
        }
        self.active = false;
    }
}

/// Дифференциальный движок inline-рендеринга терминала (архитектура нативного scrollback):
/// - постоянные строки (приветственная секция, завершённые реплики и переполнившиеся строки
///   длинного ответа) записываются с `\r\n` в нативный scrollback эмулятора терминала;
/// - активная нижняя зона (`live_rows_drawn` строк, гарантированно `< rows`) перерисовывается
///   на месте через `\r\x1b[{N}A\x1b[J` без мерцания и без полноэкранного alt-screen.
#[derive(Default, Debug)]
pub struct InlineViewport {
    live_rows_drawn: usize,
    flushed_turn_lines: usize,
}

impl InlineViewport {
    pub fn new() -> Self {
        Self::default()
    }

    fn rewind_and_clear(&mut self, buf: &mut String) {
        buf.push('\r');
        if self.live_rows_drawn > 1 {
            buf.push_str(&format!("\x1b[{}A", self.live_rows_drawn - 1));
        }
        buf.push_str("\x1b[J");
        self.live_rows_drawn = 0;
    }

    /// Записывает постоянные строки (например, приветственный блок в начале сессии) прямо в scrollback.
    pub fn write_permanent<W: Write>(
        &mut self,
        out: &mut W,
        lines: &[String],
        color: ColorMode,
    ) -> io::Result<()> {
        let mut buf = String::new();
        if self.live_rows_drawn > 0 {
            self.rewind_and_clear(&mut buf);
        } else {
            buf.push_str("\r\x1b[J");
        }
        let reset = if color == ColorMode::Plain {
            ""
        } else {
            "\x1b[0m"
        };
        for line in lines {
            buf.push_str(line);
            buf.push_str(reset);
            buf.push_str("\r\n");
        }
        out.write_all(buf.as_bytes())?;
        out.flush()
    }

    /// Фиксирует оставшиеся строки текущего хода в нативный scrollback и сбрасывает счётчик хода.
    pub fn commit_turn<W: Write>(
        &mut self,
        out: &mut W,
        turn_lines: &[String],
        color: ColorMode,
    ) -> io::Result<()> {
        let mut buf = String::new();
        if self.live_rows_drawn > 0 {
            self.rewind_and_clear(&mut buf);
        } else {
            buf.push_str("\r\x1b[J");
        }
        let reset = if color == ColorMode::Plain {
            ""
        } else {
            "\x1b[0m"
        };
        let start = self.flushed_turn_lines.min(turn_lines.len());
        for line in &turn_lines[start..] {
            buf.push_str(line);
            buf.push_str(reset);
            buf.push_str("\r\n");
        }
        self.flushed_turn_lines = 0;
        self.live_rows_drawn = 0;
        out.write_all(buf.as_bytes())?;
        out.flush()
    }

    /// Обновляет живую область (`turn_lines` текущего хода + `chrome_lines` композера/спиннера/меню).
    /// Если суммарная высота превышает высоту окна `rows - 1`, верхняя часть `turn_lines`
    /// бесшовно выталкивается в нативный scrollback.
    pub fn update_live<W: Write>(
        &mut self,
        out: &mut W,
        turn_lines: &[String],
        chrome_lines: &[String],
        rows: u16,
        color: ColorMode,
    ) -> io::Result<()> {
        let max_live = (rows as usize).saturating_sub(1).max(4);
        let mut buf = String::with_capacity(2048);
        if self.live_rows_drawn > 0 {
            self.rewind_and_clear(&mut buf);
        } else {
            buf.push_str("\r\x1b[J");
        }

        let reset = if color == ColorMode::Plain {
            ""
        } else {
            "\x1b[0m"
        };

        if turn_lines.len() < self.flushed_turn_lines {
            self.flushed_turn_lines = turn_lines.len();
        }
        let chrome_keep = chrome_lines.len().min(max_live);
        let avail_turn = max_live.saturating_sub(chrome_keep);
        let unflushed = turn_lines.len().saturating_sub(self.flushed_turn_lines);

        if unflushed > avail_turn {
            let overflow = unflushed - avail_turn;
            for line in &turn_lines[self.flushed_turn_lines..self.flushed_turn_lines + overflow] {
                buf.push_str(line);
                buf.push_str(reset);
                buf.push_str("\r\n");
            }
            self.flushed_turn_lines += overflow;
        }

        let mut live: Vec<&str> = Vec::with_capacity(max_live);
        for line in &turn_lines[self.flushed_turn_lines..] {
            live.push(line.as_str());
        }
        let chrome_start = chrome_lines.len().saturating_sub(chrome_keep);
        for line in &chrome_lines[chrome_start..] {
            live.push(line.as_str());
        }
        if live.is_empty() {
            live.push("");
        }

        for (idx, line) in live.iter().enumerate() {
            if idx > 0 {
                buf.push_str("\r\n");
            }
            buf.push_str(line);
            buf.push_str(reset);
        }
        self.live_rows_drawn = live.len();

        out.write_all(buf.as_bytes())?;
        out.flush()
    }

    /// Обрабатывает изменение размеров окна (`SIGWINCH`), аккуратно очищая старую живую область.
    pub fn on_resize<W: Write>(&mut self, out: &mut W, new_rows: u16) -> io::Result<()> {
        let max_up = (new_rows as usize).saturating_sub(1);
        self.live_rows_drawn = self.live_rows_drawn.min(max_up);
        let mut buf = String::new();
        if self.live_rows_drawn > 0 {
            self.rewind_and_clear(&mut buf);
        } else {
            buf.push_str("\r\x1b[J");
        }
        out.write_all(buf.as_bytes())?;
        out.flush()
    }

    /// Очищает видимую область основного экрана (без входа в alt-screen) по `/clear` или `Ctrl+L`.
    pub fn clear_visible_screen<W: Write>(&mut self, out: &mut W) -> io::Result<()> {
        self.live_rows_drawn = 0;
        self.flushed_turn_lines = 0;
        out.write_all(b"\r\x1b[0m\x1b[2J\x1b[H")?;
        out.flush()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyEvent {
    Char(char),
    Enter,
    Newline,
    Backspace,
    Delete,
    Tab,
    BackTab,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Ctrl(char),
    Paste(String),
}

/// Инкрементальный парсер ввода терминала (UTF-8, CSI/SS3, bracketed paste, Alt/Shift+Enter, Esc).
#[derive(Default)]
pub struct InputParser {
    buf: Vec<u8>,
    in_paste: bool,
    paste_buf: Vec<u8>,
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

impl InputParser {
    pub fn has_pending_escape(&self) -> bool {
        !self.in_paste && self.buf == [0x1b]
    }

    pub fn feed(&mut self, incoming: &[u8], flush_incomplete: bool) -> Vec<KeyEvent> {
        self.buf.extend_from_slice(incoming);
        let mut events = Vec::new();

        loop {
            if self.in_paste {
                if let Some(pos) = find_subslice(&self.buf, PASTE_END) {
                    self.paste_buf.extend_from_slice(&self.buf[..pos]);
                    self.buf.drain(..pos + PASTE_END.len());
                    self.in_paste = false;
                    let text = sanitize_paste_bytes(&self.paste_buf);
                    self.paste_buf.clear();
                    if !text.is_empty() {
                        events.push(KeyEvent::Paste(text));
                    }
                    continue;
                }
                let keep = partial_suffix_match(&self.buf, PASTE_END);
                let move_len = self.buf.len().saturating_sub(keep);
                if move_len > 0 {
                    self.paste_buf.extend_from_slice(&self.buf[..move_len]);
                    self.buf.drain(..move_len);
                }
                break;
            }

            if self.buf.is_empty() {
                break;
            }

            let b = self.buf[0];
            if b == 0x1b {
                if self.buf.len() == 1 {
                    if flush_incomplete {
                        self.buf.drain(..1);
                        events.push(KeyEvent::Esc);
                    }
                    break;
                }
                if self.buf.starts_with(PASTE_START) {
                    self.buf.drain(..PASTE_START.len());
                    self.in_paste = true;
                    self.paste_buf.clear();
                    continue;
                }
                match self.buf[1] {
                    b'\r' | b'\n' => {
                        self.buf.drain(..2);
                        events.push(KeyEvent::Newline);
                        continue;
                    }
                    b'[' => match parse_csi(&self.buf[2..]) {
                        CsiStatus::Incomplete if !flush_incomplete => break,
                        CsiStatus::Incomplete => {
                            self.buf.drain(..1);
                            events.push(KeyEvent::Esc);
                            continue;
                        }
                        CsiStatus::Complete(consumed, maybe_ev) => {
                            self.buf.drain(..2 + consumed);
                            if let Some(ev) = maybe_ev {
                                events.push(ev);
                            }
                            continue;
                        }
                    },
                    b'O' => {
                        if self.buf.len() < 3 {
                            if flush_incomplete {
                                self.buf.drain(..1);
                                events.push(KeyEvent::Esc);
                            }
                            break;
                        }
                        let ev = match self.buf[2] {
                            b'H' => Some(KeyEvent::Home),
                            b'F' => Some(KeyEvent::End),
                            b'A' => Some(KeyEvent::Up),
                            b'B' => Some(KeyEvent::Down),
                            b'C' => Some(KeyEvent::Right),
                            b'D' => Some(KeyEvent::Left),
                            _ => None,
                        };
                        self.buf.drain(..3);
                        if let Some(ev) = ev {
                            events.push(ev);
                        }
                        continue;
                    }
                    other if other == 0x1b => {
                        self.buf.drain(..1);
                        events.push(KeyEvent::Esc);
                        continue;
                    }
                    _ => {
                        self.buf.drain(..1);
                        events.push(KeyEvent::Esc);
                        continue;
                    }
                }
            }

            if b < 0x80 {
                self.buf.drain(..1);
                match b {
                    b'\r' => events.push(KeyEvent::Enter),
                    b'\n' => events.push(KeyEvent::Newline),
                    b'\t' => events.push(KeyEvent::Tab),
                    0x7f | 0x08 => events.push(KeyEvent::Backspace),
                    0x01..=0x1a => {
                        let letter = (b'a' + b - 1) as char;
                        events.push(KeyEvent::Ctrl(letter));
                    }
                    0x20..=0x7e => events.push(KeyEvent::Char(b as char)),
                    _ => {}
                }
                continue;
            }

            let expected_len = utf8_seq_len(b);
            if expected_len == 0 {
                self.buf.drain(..1);
                continue;
            }
            if self.buf.len() < expected_len {
                if flush_incomplete {
                    self.buf.drain(..1);
                    continue;
                }
                break;
            }
            match std::str::from_utf8(&self.buf[..expected_len]) {
                Ok(s) => {
                    if let Some(ch) = s.chars().next() {
                        events.push(KeyEvent::Char(ch));
                    }
                    self.buf.drain(..expected_len);
                }
                Err(_) => {
                    self.buf.drain(..1);
                }
            }
        }

        events
    }
}

enum CsiStatus {
    Incomplete,
    Complete(usize, Option<KeyEvent>),
}

fn parse_csi(tail: &[u8]) -> CsiStatus {
    for (idx, &byte) in tail.iter().enumerate() {
        if idx >= 48 {
            return CsiStatus::Complete(idx + 1, None);
        }
        if (0x40..=0x7e).contains(&byte) {
            let params = std::str::from_utf8(&tail[..idx]).unwrap_or("");
            let ev = decode_csi(params, byte);
            return CsiStatus::Complete(idx + 1, ev);
        }
    }
    CsiStatus::Incomplete
}

fn decode_csi(params: &str, final_byte: u8) -> Option<KeyEvent> {
    match final_byte {
        b'A' => Some(KeyEvent::Up),
        b'B' => Some(KeyEvent::Down),
        b'C' => Some(KeyEvent::Right),
        b'D' => Some(KeyEvent::Left),
        b'H' => Some(KeyEvent::Home),
        b'F' => Some(KeyEvent::End),
        b'Z' => Some(KeyEvent::BackTab),
        b'u' => {
            if params == "13;2" || params == "13;5" || params == "13;3" {
                Some(KeyEvent::Newline)
            } else if params == "13" || params == "13;1" {
                Some(KeyEvent::Enter)
            } else if params == "27" || params == "27;1" {
                Some(KeyEvent::Esc)
            } else {
                None
            }
        }
        b'~' => {
            let mut parts = params.split(';');
            let first = parts.next().unwrap_or("");
            match first {
                "1" | "7" => Some(KeyEvent::Home),
                "3" => Some(KeyEvent::Delete),
                "4" | "8" => Some(KeyEvent::End),
                "5" => Some(KeyEvent::PageUp),
                "6" => Some(KeyEvent::PageDown),
                "27" if params.ends_with(";13") || params.contains(";13;") => {
                    Some(KeyEvent::Newline)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn utf8_seq_len(first: u8) -> usize {
    if first & 0x80 == 0 {
        1
    } else if first & 0xE0 == 0xC0 {
        2
    } else if first & 0xF0 == 0xE0 {
        3
    } else if first & 0xF8 == 0xF0 {
        4
    } else {
        0
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn partial_suffix_match(haystack: &[u8], needle: &[u8]) -> usize {
    let max_k = haystack.len().min(needle.len().saturating_sub(1));
    for k in (1..=max_k).rev() {
        if haystack[haystack.len() - k..] == needle[..k] {
            return k;
        }
    }
    0
}

fn sanitize_paste_bytes(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    let cleaned = sanitize_terminal_text(&s);
    cleaned.replace("\r\n", "\n").replace('\r', "\n")
}

/// Удаляет ESC/CSI/OSC/управляющие последовательности из недоверенного текста модели и инструментов.
pub fn sanitize_terminal_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i < bytes.len() {
                match bytes[i] {
                    b'[' => {
                        i += 1;
                        while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                            i += 1;
                        }
                        if i < bytes.len() {
                            i += 1;
                        }
                    }
                    b']' | b'_' | b'P' => {
                        i += 1;
                        while i < bytes.len() {
                            if bytes[i] == 0x07 {
                                i += 1;
                                break;
                            }
                            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                    }
                    _ => {
                        i += 1;
                    }
                }
            }
            continue;
        }
        let ch = s[i..].chars().next().unwrap_or('\0');
        let clen = ch.len_utf8();
        i += clen;
        match ch {
            '\n' => out.push('\n'),
            '\r' => {}
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

pub fn char_display_width(c: char) -> usize {
    let u = c as u32;
    if c.is_control() {
        return 0;
    }
    if matches!(
        u,
        0x0300..=0x036F
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x200B..=0x200F
            | 0x20D0..=0x20FF
            | 0xFE00..=0xFE0F
            | 0xFE20..=0xFE2F
    ) {
        return 0;
    }
    if matches!(
        u,
        0x1100..=0x115F
            | 0x2329..=0x232A
            | 0x2E80..=0x303E
            | 0x3040..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1FAFF
            | 0x20000..=0x2FFFD
            | 0x30000..=0x3FFFD
    ) {
        return 2;
    }
    1
}

pub fn display_width(s: &str) -> usize {
    s.chars().map(char_display_width).sum()
}

pub fn strip_ansi_visible(s: &str) -> String {
    sanitize_terminal_text(s)
}

pub fn truncate_cols(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let total = display_width(s);
    if total <= max_cols {
        return s.to_string();
    }
    if max_cols == 1 {
        return "…".into();
    }
    let budget = max_cols - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = char_display_width(ch);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

pub fn wrap_text(s: &str, max_cols: usize) -> Vec<String> {
    let width = max_cols.max(1);
    let mut out = Vec::new();
    for raw_line in s.split('\n') {
        if raw_line.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut cur_w = 0usize;
        let mut last_space_byte: Option<usize> = None;

        for ch in raw_line.chars() {
            let cw = char_display_width(ch).min(width);
            if cur_w + cw > width && !current.is_empty() {
                if let Some(sp) = last_space_byte.take() {
                    let tail = current[sp..].trim_start().to_string();
                    out.push(current[..sp].trim_end().to_string());
                    current = tail;
                    cur_w = display_width(&current);
                } else {
                    out.push(std::mem::take(&mut current));
                    cur_w = 0;
                }
            }
            if ch == ' ' {
                last_space_byte = Some(current.len());
            }
            current.push(ch);
            cur_w += cw;
        }
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

pub fn pad_right_cols(s: &str, cols: usize) -> String {
    let clipped = truncate_cols(s, cols);
    let w = display_width(&clipped);
    if w >= cols {
        clipped
    } else {
        format!("{clipped}{}", " ".repeat(cols - w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_palette_matches_desige_kaic_palette_json_tokens() {
        let carbon = theme_tokens(TuiTheme::Carbon);
        assert_eq!(carbon.surface_0, (0x0B, 0x0B, 0x0C));
        assert_eq!(carbon.surface_1, (0x14, 0x14, 0x16));
        assert_eq!(carbon.surface_2, (0x1C, 0x1C, 0x20));
        assert_eq!(carbon.line, (0x2A, 0x2A, 0x2F));
        assert_eq!(carbon.line_strong, (0x6E, 0x6E, 0x76));
        assert_eq!(carbon.ink, (0xF4, 0xF1, 0xEC));
        assert_eq!(carbon.ink_muted, (0xA8, 0xA6, 0xA2));
        assert_eq!(carbon.brand, (0xD2, 0x60, 0x2C));
        assert_eq!(carbon.brand_text, (0xE8, 0x78, 0x4A));
        assert_eq!(carbon.on_brand, (0x0B, 0x0B, 0x0C));
        assert_eq!(carbon.ceramic_200, (0xF0, 0xC9, 0xAE));
        assert_eq!(carbon.selection, (0x2D, 0x1A, 0x10));
        assert_eq!(carbon.model_local, (0x5F, 0xC2, 0xB0));
        assert_eq!(carbon.model_cloud, (0x8F, 0xB4, 0xF2));
        assert_eq!(carbon.signal_ok, (0x5D, 0xB5, 0x7A));
        assert_eq!(carbon.signal_warn, (0xE5, 0xAE, 0x45));
        assert_eq!(carbon.control_live, (0xF2, 0x64, 0x5F));

        let bone = theme_tokens(TuiTheme::Bone);
        assert_eq!(bone.surface_0, (0xF4, 0xF1, 0xEC));
        assert_eq!(bone.ink, (0x0B, 0x0B, 0x0C));
        assert_eq!(bone.brand_fill, (0xD2, 0x60, 0x2C));
        assert_eq!(bone.on_brand, (0x0B, 0x0B, 0x0C));

        // Фирменная плашка: чёрный текст #0B0B0C (11;11;12) на обжиге #D2602C (210;96;44)
        let badge = style_themed(
            StyleRole::BrandBadge,
            ColorMode::TrueColor,
            TuiTheme::Carbon,
            " KAIC ",
        );
        assert!(badge.contains("38;2;11;11;12;48;2;210;96;44"));
    }

    #[test]
    fn input_parser_handles_utf8_csi_multiline_paste_and_standalone_esc() {
        let mut p = InputParser::default();
        assert!(p.feed(&[0xD0], false).is_empty());
        assert_eq!(p.feed(&[0x9F], false), vec![KeyEvent::Char('П')]);

        assert_eq!(
            p.feed(b"\x1b[A\x1b[B\x1b[Z\x1b[3~", false),
            vec![
                KeyEvent::Up,
                KeyEvent::Down,
                KeyEvent::BackTab,
                KeyEvent::Delete
            ]
        );

        assert_eq!(
            p.feed(b"\x1b[13;2u\x1b\r\n", false),
            vec![KeyEvent::Newline, KeyEvent::Newline, KeyEvent::Newline]
        );

        assert!(p.feed(&[0x1b], false).is_empty());
        assert!(p.has_pending_escape());
        assert_eq!(p.feed(&[], true), vec![KeyEvent::Esc]);

        let evs = p.feed(b"\x1b[200~line1\n\x1b[31mline2\x1b[201~", false);
        assert_eq!(evs, vec![KeyEvent::Paste("line1\nline2".into())]);
    }

    #[test]
    fn sanitize_and_width_handle_cyrillic_emoji_and_ansi_injection() {
        let dirty = "Привет \x1b[31mкрасный\x1b[0m\x1b]0;hacked-title\x07 Мир\t!";
        let clean = sanitize_terminal_text(dirty);
        assert_eq!(clean, "Привет красный Мир    !");
        assert!(!clean.contains('\x1b'));
        assert_eq!(display_width("Привет"), 6);
        assert_eq!(display_width("🚀a"), 3);
        let wrapped = wrap_text("Привет кириллический агент KAIC Studio", 12);
        assert!(wrapped.iter().all(|l| display_width(l) <= 12));
    }

    #[test]
    fn inline_viewport_never_uses_alt_screen_and_flushes_overflow_to_scrollback() {
        let mut vp = InlineViewport::new();
        let mut buf = Vec::new();
        vp.write_permanent(
            &mut buf,
            &["Welcome 1".into(), "Welcome 2".into()],
            ColorMode::Plain,
        )
        .unwrap();
        let turn: Vec<String> = (1..=8).map(|i| format!("turn line {i}")).collect();
        let chrome = vec!["❯ input▏".into(), "hint".into()];
        // rows = 7 => max_live = 6, avail_turn = 4 => lines 1..=4 уходят в scrollback, 5..=8 + chrome в live!
        vp.update_live(&mut buf, &turn, &chrome, 7, ColorMode::Plain)
            .unwrap();
        vp.commit_turn(&mut buf, &turn, ColorMode::Plain).unwrap();
        let emitted = String::from_utf8(buf).unwrap();
        assert!(!emitted.contains("\x1b[?1049h"));
        assert!(!emitted.contains("\x1b[?1049l"));
        for i in 1..=8 {
            assert!(emitted.contains(&format!("turn line {i}\r\n")));
        }
    }
}
