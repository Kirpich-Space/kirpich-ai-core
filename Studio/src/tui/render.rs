//! Рендерер inline-интерфейса терминала KAIC (нативный scrollback без полноэкранного alt-screen).
//!
//! Строго следует официальной дизайн-системе `Desige/`:
//! - Палитра `Carbon` (`#0B0B0C`) и `Bone` (`#F4F1EC`) из `Desige/kaic-palette.json`;
//! - Фирменный акцент обжига `kiln-500` (`#D2602C`) и контрастный чёрный текст `on-brand` (`#0B0B0C`);
//! - Официальный знак `Desige/kaic-mark.svg` и wordmark `Desige/kaic-wordmark-on-{dark,light}.svg`
//!   с каскадом протоколов (`Kitty` -> `iTerm2` -> `HalfBlock` -> текстовый `[▸▸▸]`).

use super::{
    ActiveMenu, SLASH_COMMANDS, TranscriptItem, TuiState, filtered_model_indices,
    logo::{LogoProtocol, render_official_logo, render_wordmark_inline},
    matching_slash_commands,
    term::{
        ColorMode, StyleRole, TuiTheme, display_width, pad_right_cols, sanitize_terminal_text,
        style_themed, truncate_cols, wrap_text,
    },
};
use crate::agent::{AgentPolicy, ExecPolicy, ProviderKind};

const SPINNER_UNICODE: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_ASCII: &[&str] = &["|", "/", "-", "\\"];

/// Безопасная рабочая ширина строки (`cols - 1`), исключающая авто-перенос (`xenl`) на границе PTY.
fn safe_line_width(cols: u16) -> usize {
    (cols as usize).saturating_sub(1).max(20)
}

fn is_local_locality(state: &TuiState) -> bool {
    match state.provider {
        ProviderKind::LocalLlama | ProviderKind::OllamaLocal => true,
        ProviderKind::OpenAiCompat => {
            let ep = state.endpoint.trim();
            ep.contains("127.0.0.1") || ep.contains("localhost") || ep.contains("[::1]")
        }
        _ => false,
    }
}

fn human_policy_ru(policy: AgentPolicy) -> &'static str {
    match policy {
        AgentPolicy::ReadOnly => "только чтение файлов",
        AgentPolicy::ApprovalRequired => "запись файлов с подтверждением",
        AgentPolicy::WorkspaceWrite => "запись в рабочую область",
    }
}

fn human_exec_ru(exec: ExecPolicy) -> &'static str {
    match exec {
        ExecPolicy::Disabled => "запуск кода отключён",
        ExecPolicy::SafeInspectOnly => "безопасные проверки",
        ExecPolicy::ConfirmUserCode => "код с подтверждением",
        ExecPolicy::AllowWorkspaceBuilds => "сборки рабочей области",
    }
}

fn shorten_workspace_path(path: &std::path::Path, max_cols: usize) -> String {
    let raw = sanitize_terminal_text(&path.display().to_string());
    let shortened = if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
        && let Some(rest) = raw.strip_prefix(&home)
    {
        if rest.is_empty() {
            "~".to_string()
        } else {
            format!("~{rest}")
        }
    } else {
        raw
    };
    if display_width(&shortened) <= max_cols {
        return shortened;
    }
    if max_cols <= 6 {
        return truncate_cols(&shortened, max_cols);
    }
    let keep = max_cols - 1;
    let mut tail = String::new();
    let mut used = 0usize;
    for ch in shortened.chars().rev() {
        let cw = super::term::char_display_width(ch);
        if used + cw > keep {
            break;
        }
        tail.insert(0, ch);
        used += cw;
    }
    format!("…{tail}")
}

/// Компактная брендированная приветственная секция в верхней части сессии (без вертикального центрирования).
pub fn render_welcome_banner(
    state: &TuiState,
    cols: u16,
    color: ColorMode,
    protocol: LogoProtocol,
) -> Vec<String> {
    let w = safe_line_width(cols);
    let theme = state.theme;
    let logo = render_official_logo(color, theme, protocol, w);

    let active_model = if state.model.trim().is_empty() {
        state.provider.short_label()
    } else {
        state.model.trim()
    };
    let local_mode = is_local_locality(state);
    let (loc_dot, loc_desc, loc_role) = if local_mode {
        (
            "●",
            format!(
                "Локальная модель · {} ({}) · данные на этом ПК",
                active_model,
                state.provider.short_label()
            ),
            StyleRole::ModelLocal,
        )
    } else {
        (
            "◆",
            format!(
                "Внешний провайдер · {} · {}",
                state.provider.short_label(),
                active_model
            ),
            StyleRole::ModelCloud,
        )
    };

    let mut out = Vec::new();

    if logo.lines.len() >= 4 && w >= 54 {
        let right_w = w.saturating_sub(logo.width_cols + 4).max(20);
        let ws_str = shorten_workspace_path(&state.workspace, right_w.saturating_sub(9));

        let title_line = if logo.includes_wordmark {
            style_themed(
                StyleRole::Muted,
                color,
                theme,
                &truncate_cols("Нативная агентная среда · Rust + ASM", right_w),
            )
        } else {
            let wm = render_wordmark_inline(color, theme);
            let sub = truncate_cols(" · Нативная агентная среда", right_w.saturating_sub(4));
            format!("{wm}{}", style_themed(StyleRole::Muted, color, theme, &sub))
        };

        let model_raw = if right_w < 54 {
            if local_mode {
                format!("{loc_dot} Локально · {active_model}")
            } else {
                format!(
                    "{loc_dot} Внешний · {} · {active_model}",
                    state.provider.short_label()
                )
            }
        } else {
            format!("{loc_dot} {loc_desc}")
        };
        let model_plain = truncate_cols(&model_raw, right_w);
        let model_line = style_themed(loc_role, color, theme, &model_plain);

        let ws_line = style_themed(
            StyleRole::Muted,
            color,
            theme,
            &truncate_cols(&format!("Каталог: {ws_str}"), right_w),
        );

        let perms_raw = if right_w < 52 {
            format!(
                "Доступ: {} · {}",
                match state.policy {
                    AgentPolicy::ReadOnly => "чтение",
                    AgentPolicy::ApprovalRequired => "с подтвержд.",
                    AgentPolicy::WorkspaceWrite => "запись в проект",
                },
                match state.exec_policy {
                    ExecPolicy::Disabled => "код выкл.",
                    ExecPolicy::SafeInspectOnly => "проверки",
                    ExecPolicy::ConfirmUserCode => "код с подтвержд.",
                    ExecPolicy::AllowWorkspaceBuilds => "сборки",
                }
            )
        } else {
            format!(
                "Доступ: {} · {}",
                human_policy_ru(state.policy),
                human_exec_ru(state.exec_policy)
            )
        };
        let perms_line = style_themed(
            StyleRole::Muted,
            color,
            theme,
            &truncate_cols(&perms_raw, right_w),
        );

        let nav_line = style_themed(
            StyleRole::BrandText,
            color,
            theme,
            &truncate_cols("/ команды · /model выбор модели · /theme тема", right_w),
        );

        let info_lines = [title_line, model_line, ws_line, perms_line, nav_line];
        let rows_n = logo.lines.len().max(info_lines.len());
        for i in 0..rows_n {
            let left = if let Some(l) = logo.lines.get(i) {
                format!(" {l}   ")
            } else {
                " ".repeat(logo.width_cols + 4)
            };
            let right = info_lines.get(i).cloned().unwrap_or_default();
            out.push(format!("{left}{right}"));
        }
    } else {
        // Компактный текстовый/узкий режим (bare TTY, --no-color, узкий терминал < 54 колонок)
        let badge = logo.lines.first().map(String::as_str).unwrap_or("[▸▸▸]");
        let head = if color == ColorMode::Plain {
            truncate_cols(&format!(" {badge} KAIC · Нативная агентная среда"), w)
        } else {
            format!(
                " {badge} {} {}",
                render_wordmark_inline(color, theme),
                style_themed(
                    StyleRole::Muted,
                    color,
                    theme,
                    &truncate_cols("· Нативная агентная среда", w.saturating_sub(13)),
                )
            )
        };
        out.push(head);
        out.push(style_themed(
            loc_role,
            color,
            theme,
            &truncate_cols(&format!(" {loc_dot} {loc_desc}"), w),
        ));
        let ws_str = shorten_workspace_path(&state.workspace, w.saturating_sub(11));
        out.push(style_themed(
            StyleRole::Muted,
            color,
            theme,
            &truncate_cols(&format!(" Каталог: {ws_str}"), w),
        ));
        out.push(style_themed(
            StyleRole::Muted,
            color,
            theme,
            &truncate_cols(
                &format!(
                    " Доступ: {} · {}",
                    human_policy_ru(state.policy),
                    human_exec_ru(state.exec_policy)
                ),
                w,
            ),
        ));
    }

    let rule = "─".repeat(w);
    out.push(style_themed(StyleRole::Border, color, theme, &rule));
    out
}

/// Рендерит срез элементов диалога (`TranscriptItem`) в список физических строк для inline-потока.
pub fn render_transcript_items(
    theme: TuiTheme,
    items: &[TranscriptItem],
    cols: u16,
    color: ColorMode,
) -> Vec<String> {
    let w = safe_line_width(cols);
    let mut out = Vec::new();
    for item in items {
        format_transcript_item(theme, item, w, color, &mut out);
    }
    out
}

/// Рендерит активный ход сессии (`state.transcript[state.turn_start_idx..]`).
pub fn render_active_turn(state: &TuiState, cols: u16, color: ColorMode) -> Vec<String> {
    let start = state.turn_start_idx.min(state.transcript.len());
    render_transcript_items(state.theme, &state.transcript[start..], cols, color)
}

/// Рендерит нижнюю интерактивную панель непосредственно под контентом диалога:
/// плавный спиннер активности, карточку подтверждения инструмента, многострочный композер и интерактивные меню.
pub fn render_live_chrome(state: &TuiState, cols: u16, color: ColorMode) -> Vec<String> {
    let w = safe_line_width(cols);
    let theme = state.theme;
    let mut lines = Vec::new();

    // Предупреждение активного динамического управления (только когда явно включено пользователем!)
    if state.dynamic_interaction_enabled {
        let alert = pad_right_cols(
            " ▲ АКТИВНО УПРАВЛЕНИЕ МЕСТАМИ · Нажмите Esc для немедленной остановки",
            w,
        );
        lines.push(style_themed(StyleRole::LiveDanger, color, theme, &alert));
    }

    // Плавная анимация активности при работе агента
    if state.busy {
        let frames = if color == ColorMode::Plain {
            SPINNER_ASCII
        } else {
            SPINNER_UNICODE
        };
        let spinner = frames[state.anim_tick % frames.len()];
        let raw_status = if state.status_line.trim().is_empty() {
            "Выполняется…"
        } else {
            state.status_line.trim()
        };
        let clean_status = sanitize_terminal_text(raw_status);
        let elapsed = state.elapsed_run_secs();
        let meta = format!(" · {elapsed:.1} с · Esc остановить");
        let status_budget = w.saturating_sub(display_width(&meta) + 5).max(10);
        let clipped_status = truncate_cols(&clean_status, status_budget);
        if color == ColorMode::Plain {
            lines.push(truncate_cols(
                &format!(" {spinner} {clipped_status}{meta}"),
                w,
            ));
        } else {
            lines.push(format!(
                " {} {}{}",
                style_themed(StyleRole::Brand, color, theme, spinner),
                style_themed(StyleRole::BrandText, color, theme, &clipped_status),
                style_themed(StyleRole::Muted, color, theme, &meta),
            ));
        }
    }

    // Модальная карточка одноразового подтверждения инструмента (HITL)
    if let Some(req) = &state.pending_approval {
        let warn_sep = "─".repeat(w);
        lines.push(style_themed(StyleRole::Warn, color, theme, &warn_sep));
        let digest_short: String = req.action_digest.chars().take(12).collect();
        let title = truncate_cols(
            &format!(
                " ▲ Подтверждение действия · {} · {} · sha256:{digest_short}",
                sanitize_terminal_text(&req.tool),
                req.isolation.label_ru(),
            ),
            w,
        );
        lines.push(style_themed(StyleRole::Warn, color, theme, &title));
        for wl in wrap_text(&sanitize_terminal_text(&req.summary), w.saturating_sub(4))
            .into_iter()
            .take(3)
        {
            let row = pad_right_cols(&format!("   {wl}"), w);
            lines.push(style_themed(StyleRole::CardSurface, color, theme, &row));
        }
        if color == ColorMode::Plain || w < 54 {
            let act = truncate_cols(" [y] Разрешить   [n] Отклонить   [Esc] Остановить", w);
            lines.push(act);
        } else {
            let btn_ok = style_themed(StyleRole::BrandBadge, color, theme, " y · Разрешить ");
            let btn_no = style_themed(StyleRole::Keycap, color, theme, " n · Отклонить ");
            let btn_esc = style_themed(StyleRole::Keycap, color, theme, " Esc · Отмена ");
            let note = style_themed(
                StyleRole::Muted,
                color,
                theme,
                "  (пустой Enter не подтверждает)",
            );
            lines.push(format!(" {btn_ok}  {btn_no}  {btn_esc}{note}"));
        }
        return lines;
    }

    // Тонкая разделительная линия перед полем ввода (только когда выше есть диалог или спиннер —
    // избегаем двойной горизонтальной линии сразу под приветственной секцией!)
    if !state.transcript.is_empty() || state.busy || state.dynamic_interaction_enabled {
        let rule = "─".repeat(w);
        lines.push(style_themed(StyleRole::Border, color, theme, &rule));
    }

    // Многострочное поле ввода (композер) сразу после контента
    let inner_w = w.saturating_sub(4).max(10);
    if state.input.is_empty() && state.active_menu == ActiveMenu::None {
        let prompt_glyph = style_themed(StyleRole::Brand, color, theme, " ❯ ");
        let cursor_glyph = style_themed(StyleRole::Brand, color, theme, "▏");
        let placeholder = truncate_cols(
            "Введите задачу или / для меню команд…",
            inner_w.saturating_sub(1),
        );
        if color == ColorMode::Plain {
            lines.push(format!(" ❯ ▏{placeholder}"));
        } else {
            lines.push(format!(
                "{prompt_glyph}{cursor_glyph}{}",
                style_themed(StyleRole::Muted, color, theme, &placeholder)
            ));
        }
    } else {
        let with_cursor = insert_visual_cursor(&state.input, state.cursor);
        let mut wrapped_composer: Vec<(bool, String)> = Vec::new();
        for (line_idx, logical) in with_cursor.split('\n').enumerate() {
            let sub = wrap_text(logical, inner_w);
            for (sub_idx, part) in sub.into_iter().enumerate() {
                wrapped_composer.push((line_idx == 0 && sub_idx == 0, part));
            }
        }
        let max_comp = 6usize;
        let skip_n = wrapped_composer.len().saturating_sub(max_comp);
        for (is_first, part) in wrapped_composer.into_iter().skip(skip_n) {
            let pfx = if is_first { " ❯ " } else { " · " };
            let pfx_role = if is_first {
                StyleRole::Brand
            } else {
                StyleRole::BrandText
            };
            let clipped = truncate_cols(&part, inner_w);
            if color == ColorMode::Plain {
                lines.push(format!("{pfx}{clipped}"));
            } else {
                lines.push(format!(
                    "{}{}",
                    style_themed(pfx_role, color, theme, pfx),
                    style_themed(StyleRole::Ink, color, theme, &clipped),
                ));
            }
        }
    }

    // Интерактивное меню выбора модели (/model или Ctrl+P) либо меню slash-команд (/)
    match &state.active_menu {
        ActiveMenu::ModelPicker {
            filter,
            cursor,
            items,
        } => {
            render_model_picker_menu(filter, *cursor, items, state, w, color, &mut lines);
        }
        ActiveMenu::None => {
            if state.is_slash_menu_open() {
                render_slash_menu(state, w, color, &mut lines);
            } else {
                lines.push(render_subtle_hint(state, w, color));
            }
        }
    }

    lines
}

fn render_model_picker_menu(
    filter: &str,
    cursor: usize,
    items: &[super::ModelOption],
    state: &TuiState,
    w: usize,
    color: ColorMode,
    out: &mut Vec<String>,
) {
    let theme = state.theme;
    let filter_disp = if filter.is_empty() {
        "все".to_string()
    } else {
        sanitize_terminal_text(filter)
    };
    let head = pad_right_cols(
        &format!(
            " Выбор модели · фильтр: {filter_disp} · ↑↓ навигация · Enter выбрать · Esc закрыть"
        ),
        w,
    );
    out.push(style_themed(StyleRole::Keycap, color, theme, &head));

    let indices = filtered_model_indices(items, filter);
    if indices.is_empty() {
        let empty = pad_right_cols(
            "   Совпадений нет — измените текст фильтра или нажмите Backspace",
            w,
        );
        out.push(style_themed(StyleRole::CardSurface, color, theme, &empty));
        return;
    }

    let max_vis = 7usize;
    let sel = cursor.min(indices.len().saturating_sub(1));
    let win_start = sel
        .saturating_sub(max_vis / 2)
        .min(indices.len().saturating_sub(max_vis));
    let win_end = (win_start + max_vis).min(indices.len());

    for (vis_idx, &item_idx) in indices[win_start..win_end].iter().enumerate() {
        let actual_pos = win_start + vis_idx;
        let is_selected = actual_pos == sel;
        let opt = &items[item_idx];
        let is_current = opt.provider == state.provider && opt.model == state.model;
        let pointer = if is_selected { "▸" } else { " " };
        let loc_glyph = if opt.locality_local { "●" } else { "◆" };
        let active_mark = if is_current { " [текущая]" } else { "" };
        let row_plain = format!(
            " {pointer} {loc_glyph} {:22} │ {:18} │ {}{active_mark}",
            truncate_cols(&opt.model, 22),
            truncate_cols(&opt.badge_ru, 18),
            opt.detail_ru
        );
        let padded = pad_right_cols(&row_plain, w);
        if is_selected {
            out.push(style_themed(
                StyleRole::PopupSelected,
                color,
                theme,
                &padded,
            ));
        } else {
            out.push(style_themed(StyleRole::CardSurface, color, theme, &padded));
        }
    }
}

fn render_slash_menu(state: &TuiState, w: usize, color: ColorMode, out: &mut Vec<String>) {
    let theme = state.theme;
    let matches = matching_slash_commands(&state.input);
    if matches.is_empty() {
        return;
    }
    let max_vis = 6usize;
    let sel = state.autocomplete_idx % matches.len();
    let win_start = sel
        .saturating_sub(max_vis / 2)
        .min(matches.len().saturating_sub(max_vis));
    let win_end = (win_start + max_vis).min(matches.len());

    for (vis_i, &cmd_idx) in matches[win_start..win_end].iter().enumerate() {
        let actual_i = win_start + vis_i;
        let is_selected = actual_i == sel;
        let def = &SLASH_COMMANDS[cmd_idx];
        let pointer = if is_selected { "▸" } else { " " };
        let cmd_col = if def.args.is_empty() {
            def.cmd.to_string()
        } else {
            format!("{} {}", def.cmd, def.args)
        };
        let row = pad_right_cols(
            &format!(
                " {pointer} {:28} — {}",
                truncate_cols(&cmd_col, 28),
                def.desc
            ),
            w,
        );
        if is_selected {
            out.push(style_themed(StyleRole::PopupSelected, color, theme, &row));
        } else {
            out.push(style_themed(StyleRole::CardSurface, color, theme, &row));
        }
    }
}

fn render_subtle_hint(state: &TuiState, w: usize, color: ColorMode) -> String {
    let mut parts = Vec::new();
    if let Some(m) = state.context_metrics
        && m.used_tokens > 0
    {
        parts.push(format!("Контекст: {}%", m.utilization_pct));
    }
    if !state.busy && !state.status_line.trim().is_empty() {
        parts.push(sanitize_terminal_text(state.status_line.trim()));
    }
    if state.busy {
        parts.push("Esc остановить".to_string());
        parts.push("Ctrl+O инструмент".to_string());
    } else if parts.is_empty() {
        parts.push("Enter отправить".to_string());
        parts.push("Ctrl+J строка".to_string());
        parts.push("Ctrl+P модель".to_string());
    } else {
        parts.push("Ctrl+P модель".to_string());
        if state
            .transcript
            .iter()
            .any(|i| matches!(i, TranscriptItem::Tool(_)))
        {
            parts.push("Ctrl+O инструмент".to_string());
        }
        parts.push("Enter отправить".to_string());
    }
    let joined = truncate_cols(&format!(" {}", parts.join(" · ")), w);
    style_themed(StyleRole::Muted, color, state.theme, &joined)
}

/// Полный снимок inline-представления (приветственная секция + диалог + живая панель композера/меню)
/// для детерминированных проверок рендеринга на разных размерах терминала.
pub fn render_inline_view(
    state: &TuiState,
    cols: u16,
    _rows: u16,
    color: ColorMode,
) -> Vec<String> {
    let mut out = render_welcome_banner(state, cols, color, LogoProtocol::HalfBlock);
    if !state.transcript.is_empty() {
        out.extend(render_transcript_items(
            state.theme,
            &state.transcript,
            cols,
            color,
        ));
    }
    out.extend(render_live_chrome(state, cols, color));
    out
}

fn insert_visual_cursor(input: &str, cursor_byte: usize) -> String {
    let idx = cursor_byte.min(input.len());
    let mut safe_idx = idx;
    while safe_idx > 0 && !input.is_char_boundary(safe_idx) {
        safe_idx -= 1;
    }
    let mut out = String::with_capacity(input.len() + 4);
    out.push_str(&input[..safe_idx]);
    out.push('▏');
    out.push_str(&input[safe_idx..]);
    out
}

fn format_transcript_item(
    theme: TuiTheme,
    item: &TranscriptItem,
    w: usize,
    color: ColorMode,
    out: &mut Vec<String>,
) {
    match item {
        TranscriptItem::User { text } => {
            let clean = sanitize_terminal_text(text);
            for line in wrap_text(&clean, w.saturating_sub(4)) {
                if color == ColorMode::Plain {
                    out.push(format!(" ▎ {line}"));
                } else {
                    out.push(format!(
                        "{}{}",
                        style_themed(StyleRole::Brand, color, theme, " ▎ "),
                        style_themed(StyleRole::Ink, color, theme, &line),
                    ));
                }
            }
            out.push(String::new());
        }
        TranscriptItem::Assistant {
            text,
            streaming,
            metrics,
        } => {
            render_markdown_assistant(theme, text, *streaming, *metrics, w, color, out);
            out.push(String::new());
        }
        TranscriptItem::Tool(entry) => {
            let arrow = if entry.expanded { "▾" } else { "▸" };
            let (status_glyph, status_role) = match entry.ok {
                None => ("⋯", StyleRole::Warn),
                Some(true) => ("✓", StyleRole::Ok),
                Some(false) => ("✕", StyleRole::Err),
            };
            let raw_sum = sanitize_terminal_text(&entry.summary);
            let clean_sum = raw_sum
                .strip_prefix(&entry.name)
                .map(|rest| rest.trim_start_matches([' ', '→', ':', '·', '-']))
                .filter(|rest| !rest.is_empty())
                .unwrap_or(raw_sum.as_str());
            let hidden_lines = entry.output_preview.lines().count()
                + entry
                    .diff_preview
                    .as_deref()
                    .map_or(0, |d| d.lines().count());
            let collapse_hint = if !entry.expanded && hidden_lines > 0 {
                format!(" ({hidden_lines} стр. · Ctrl+O)")
            } else {
                String::new()
            };
            let name_clipped = truncate_cols(&entry.name, w.saturating_sub(14).max(6));
            let sum_budget =
                w.saturating_sub(display_width(&name_clipped) + display_width(&collapse_hint) + 10);
            let sum_clipped = truncate_cols(&clean_sum, sum_budget);

            if color == ColorMode::Plain {
                out.push(truncate_cols(
                    &format!(
                        "  {arrow} {status_glyph} {name_clipped} · {sum_clipped}{collapse_hint}"
                    ),
                    w,
                ));
            } else {
                out.push(format!(
                    "  {} {} {} · {}{}",
                    style_themed(StyleRole::Brand, color, theme, arrow),
                    style_themed(status_role, color, theme, status_glyph),
                    style_themed(StyleRole::Ink, color, theme, &name_clipped),
                    style_themed(StyleRole::Muted, color, theme, &sum_clipped),
                    style_themed(StyleRole::Muted, color, theme, &collapse_hint),
                ));
            }

            if entry.expanded {
                let code_w = w.saturating_sub(6).max(12);
                if let Some(diff) = &entry.diff_preview
                    && !diff.trim().is_empty()
                {
                    let clean_diff = sanitize_terminal_text(diff);
                    for dline in clean_diff.lines().take(80) {
                        let d_role = if dline.starts_with("+++")
                            || dline.starts_with("---")
                            || dline.starts_with("@@")
                        {
                            StyleRole::DiffHunk
                        } else if dline.starts_with('+') {
                            StyleRole::DiffAdd
                        } else if dline.starts_with('-') {
                            StyleRole::DiffDel
                        } else {
                            StyleRole::Muted
                        };
                        for wl in wrap_text(dline, code_w) {
                            if color == ColorMode::Plain {
                                out.push(format!("    │ {wl}"));
                            } else {
                                out.push(format!(
                                    "    {} {}",
                                    style_themed(StyleRole::Border, color, theme, "│"),
                                    style_themed(d_role, color, theme, &wl),
                                ));
                            }
                        }
                    }
                }
                if !entry.output_preview.trim().is_empty() {
                    let clean_out = sanitize_terminal_text(&entry.output_preview);
                    for oline in clean_out.lines().take(40) {
                        for wl in wrap_text(oline, code_w) {
                            if color == ColorMode::Plain {
                                out.push(format!("    │ {wl}"));
                            } else {
                                out.push(format!(
                                    "    {} {}",
                                    style_themed(StyleRole::Border, color, theme, "│"),
                                    style_themed(StyleRole::Muted, color, theme, &wl),
                                ));
                            }
                        }
                    }
                }
            }
        }
        TranscriptItem::Subtask(st) => {
            // Правило доступности Desige/kaic-palette.json: цвета agent-1..4 сопровождаются текстовым именем агента.
            let slot: u8 = if st.id.ends_with("-1") { 0 } else { 1 };
            let agent_label = format!("Агент {}", slot + 1);
            let (status_ru, role) = if st.verified {
                ("проверено", StyleRole::Ok)
            } else if st.status == "failed" || st.status == "blocked" {
                ("ошибка", StyleRole::Err)
            } else {
                ("в работе", StyleRole::Warn)
            };
            let title_clipped = truncate_cols(&sanitize_terminal_text(&st.title), w / 2);
            if color == ColorMode::Plain {
                out.push(truncate_cols(
                    &format!("  ◆ {agent_label} · {title_clipped} ({status_ru})"),
                    w,
                ));
            } else {
                out.push(format!(
                    "  {} · {} ({})",
                    style_themed(
                        StyleRole::Agent(slot),
                        color,
                        theme,
                        &format!("◆ {agent_label}"),
                    ),
                    style_themed(StyleRole::Ink, color, theme, &title_clipped),
                    style_themed(role, color, theme, status_ru),
                ));
            }
        }
        TranscriptItem::Notice { text, is_error } => {
            let role = if *is_error {
                StyleRole::Err
            } else {
                StyleRole::Muted
            };
            let pfx = if *is_error { "  ✕ " } else { "  · " };
            let clean = sanitize_terminal_text(text);
            for wl in wrap_text(&clean, w.saturating_sub(5)) {
                let plain = format!("{pfx}{wl}");
                out.push(style_themed(role, color, theme, &plain));
            }
        }
    }
}

fn style_inline_prose(line: &str, color: ColorMode, theme: TuiTheme) -> String {
    if color == ColorMode::Plain || !line.contains('`') {
        return style_themed(StyleRole::Ink, color, theme, line);
    }
    let mut out = String::with_capacity(line.len() + 24);
    let mut in_code = false;
    for seg in line.split('`') {
        if !seg.is_empty() {
            if in_code {
                out.push_str(&style_themed(StyleRole::ModelLocal, color, theme, seg));
            } else {
                out.push_str(&style_themed(StyleRole::Ink, color, theme, seg));
            }
        }
        in_code = !in_code;
    }
    out
}

fn render_markdown_assistant(
    theme: TuiTheme,
    raw: &str,
    streaming: bool,
    metrics: Option<(u32, f32, f32)>,
    w: usize,
    color: ColorMode,
    out: &mut Vec<String>,
) {
    let clean = sanitize_terminal_text(raw);
    let mut in_fence = false;
    let mut fence_lang = String::new();
    let text_w = w.saturating_sub(3).max(12);
    let code_w = w.saturating_sub(6).max(12);

    let mut lines: Vec<&str> = clean.split('\n').collect();
    if lines.is_empty() {
        lines.push("");
    }
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if let Some(after) = trimmed.strip_prefix("```") {
            if !in_fence {
                in_fence = true;
                fence_lang = after.trim().to_ascii_lowercase();
                let label = if fence_lang.is_empty() {
                    "код"
                } else {
                    fence_lang.as_str()
                };
                let bar = truncate_cols(&format!("  ┌─ {label} "), w);
                out.push(style_themed(StyleRole::BorderStrong, color, theme, &bar));
            } else {
                in_fence = false;
                fence_lang.clear();
                let bar = truncate_cols("  └─", w);
                out.push(style_themed(StyleRole::BorderStrong, color, theme, &bar));
            }
            continue;
        }

        if in_fence {
            for wl in wrap_text(line, code_w) {
                if color == ColorMode::Plain {
                    out.push(format!("  │ {wl}"));
                } else {
                    let hi = highlight_code_line(&fence_lang, &wl, color, theme);
                    out.push(format!(
                        "  {} {hi}",
                        style_themed(StyleRole::Border, color, theme, "│")
                    ));
                }
            }
            continue;
        }

        let is_heading =
            trimmed.starts_with("# ") || trimmed.starts_with("## ") || trimmed.starts_with("### ");
        let suffix = if streaming && idx + 1 == lines.len() {
            " ▍"
        } else {
            ""
        };
        let normalized = if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            format!("• {rest}{suffix}")
        } else {
            format!("{line}{suffix}")
        };
        for wl in wrap_text(&normalized, text_w) {
            if color == ColorMode::Plain {
                out.push(format!("  {wl}"));
            } else if is_heading {
                out.push(format!(
                    "  {}",
                    style_themed(StyleRole::BrandText, color, theme, &wl)
                ));
            } else {
                out.push(format!("  {}", style_inline_prose(&wl, color, theme)));
            }
        }
    }

    if streaming && clean.is_empty() {
        out.push(style_themed(StyleRole::Brand, color, theme, "  ▍"));
    }
    if let Some((tok, tps, secs)) = metrics {
        let plain = truncate_cols(&format!("  · {tok} ток. · {tps:.1} ток/с · {secs:.1} с"), w);
        out.push(style_themed(StyleRole::Muted, color, theme, &plain));
    }
}

pub fn highlight_code_line(lang: &str, line: &str, color: ColorMode, theme: TuiTheme) -> String {
    if color == ColorMode::Plain || line.is_empty() {
        return line.to_string();
    }
    let trimmed = line.trim_start();
    if trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with("--")
        || ((lang == "asm" || lang == "s") && trimmed.starts_with(';'))
    {
        return style_themed(StyleRole::Comment, color, theme, line);
    }

    let mut out = String::with_capacity(line.len() + 32);
    let mut chars = line.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        if ch == '"' || (ch == '\'' && lang != "rust" && lang != "rs") {
            let quote = ch;
            let mut end = line.len();
            let mut escaped = false;
            for (idx, c) in chars.by_ref() {
                if escaped {
                    escaped = false;
                    continue;
                }
                if c == '\\' {
                    escaped = true;
                } else if c == quote {
                    end = idx + c.len_utf8();
                    break;
                }
            }
            out.push_str(&style_themed(
                StyleRole::StringLit,
                color,
                theme,
                &line[start..end],
            ));
            continue;
        }
        if ch.is_ascii_digit()
            && (start == 0
                || !line[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|p| p.is_ascii_alphanumeric() || p == '_'))
        {
            let mut end = start + ch.len_utf8();
            while let Some(&(idx, c)) = chars.peek() {
                if c.is_ascii_hexdigit() || c == 'x' || c == '_' || c == '.' {
                    end = idx + c.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            out.push_str(&style_themed(
                StyleRole::NumberLit,
                color,
                theme,
                &line[start..end],
            ));
            continue;
        }
        if ch.is_ascii_alphabetic() || ch == '_' {
            let mut end = start + ch.len_utf8();
            while let Some(&(idx, c)) = chars.peek() {
                if c.is_ascii_alphanumeric() || c == '_' {
                    end = idx + c.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            let ident = &line[start..end];
            if is_code_keyword(lang, ident) {
                out.push_str(&style_themed(StyleRole::Keyword, color, theme, ident));
            } else {
                out.push_str(&style_themed(StyleRole::Ink, color, theme, ident));
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn is_code_keyword(lang: &str, word: &str) -> bool {
    match lang {
        "rust" | "rs" | "" => matches!(
            word,
            "fn" | "pub"
                | "let"
                | "mut"
                | "const"
                | "static"
                | "struct"
                | "enum"
                | "impl"
                | "trait"
                | "type"
                | "use"
                | "mod"
                | "match"
                | "if"
                | "else"
                | "for"
                | "while"
                | "loop"
                | "return"
                | "async"
                | "await"
                | "unsafe"
                | "where"
                | "true"
                | "false"
                | "Some"
                | "None"
                | "Ok"
                | "Err"
                | "Self"
        ),
        "python" | "py" => matches!(
            word,
            "def"
                | "class"
                | "import"
                | "from"
                | "as"
                | "if"
                | "elif"
                | "else"
                | "for"
                | "while"
                | "return"
                | "yield"
                | "with"
                | "try"
                | "except"
                | "finally"
                | "raise"
                | "async"
                | "await"
                | "True"
                | "False"
                | "None"
        ),
        "sh" | "bash" | "zsh" => matches!(
            word,
            "if" | "then"
                | "else"
                | "fi"
                | "for"
                | "do"
                | "done"
                | "case"
                | "esac"
                | "export"
                | "local"
                | "set"
        ),
        "json" => matches!(word, "true" | "false" | "null"),
        "asm" | "s" => matches!(
            word,
            "mov"
                | "lock"
                | "cmpxchg"
                | "xchg"
                | "and"
                | "xor"
                | "cmp"
                | "jne"
                | "je"
                | "ret"
                | "lfence"
                | "rdtsc"
        ),
        _ => matches!(
            word,
            "fn" | "def"
                | "if"
                | "else"
                | "for"
                | "while"
                | "return"
                | "struct"
                | "class"
                | "true"
                | "false"
        ),
    }
}
