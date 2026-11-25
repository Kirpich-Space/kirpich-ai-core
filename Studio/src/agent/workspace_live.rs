//! Мультиагентная подсистема динамического редактирования:
//! независимые курсоры (`VirtualPointer`), виртуальные клавиатуры (`VirtualKeyboard`),
//! эксклюзивные блокировки файлов/путей (`SeatManager`) и честная проверка системных OS input-бэкендов.
use super::{
    FeatureStatus, WorkspaceGuard,
    asm_accel::{self, AtomicSeatGate},
    subagents::{is_path_owned, normalize_rel, paths_overlap},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, io::Read, sync::Arc};

/// До 4 одновременных агентных мест — соответствует 4 цветам `agents` в `kaic-palette.json`.
pub const MAX_AGENT_SEATS: usize = 4;

const MAX_LIVE_EDIT_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VirtualPointer {
    pub enabled: bool,
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_file: Option<String>,
    /// 1-based номер строки в документе.
    pub line: usize,
    /// 1-based позиция колонки в строке.
    pub col: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_end_line: Option<usize>,
}

impl Default for VirtualPointer {
    fn default() -> Self {
        Self {
            enabled: true,
            active: false,
            target_file: None,
            line: 1,
            col: 1,
            selection_end_line: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VirtualKeyboard {
    pub keyboard_id: String,
    pub enabled: bool,
    pub typing: bool,
    #[serde(default)]
    pub last_keys: String,
    #[serde(default)]
    pub ops_count: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentSeat {
    pub seat_id: String,
    pub agent_name: String,
    /// Индекс палитры `0..=3` (agent-1..agent-4 из `kaic-palette.json`); в UI всегда подписывается именем.
    pub color_slot: u8,
    #[serde(default)]
    pub owned_paths: Vec<String>,
    pub pointer: VirtualPointer,
    pub keyboard: VirtualKeyboard,
    pub control_live: bool,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub last_op_tsc: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LiveEditOp {
    MoveCursor {
        file: String,
        line: usize,
        col: usize,
    },
    InsertAt {
        file: String,
        line: usize,
        col: usize,
        text: String,
    },
    ReplaceLineRange {
        file: String,
        start_line: usize,
        end_line: usize,
        replacement: String,
    },
    ReplaceText {
        file: String,
        old_text: String,
        new_text: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveEditResult {
    pub seat_id: String,
    pub file: String,
    pub new_line: usize,
    pub new_col: usize,
    pub sha256: String,
    pub changed_lines: (usize, usize),
}

#[derive(Clone, Debug)]
pub struct SeatManager {
    seats: Vec<AgentSeat>,
    file_locks: HashMap<String, String>,
    next_seq: usize,
    control_live: bool,
    /// Динамическое взаимодействие агента с курсорами и клавиатурами по умолчанию ОТКЛЮЧЕНО;
    /// требует явной активации пользователем.
    dynamic_interaction_enabled: bool,
    gate: Arc<AtomicSeatGate>,
}

impl Default for SeatManager {
    fn default() -> Self {
        Self {
            seats: Vec::new(),
            file_locks: HashMap::new(),
            next_seq: 0,
            control_live: false,
            dynamic_interaction_enabled: false,
            gate: Arc::new(AtomicSeatGate::new()),
        }
    }
}

struct SeatSlotLease {
    gate: Arc<AtomicSeatGate>,
    slot: u8,
}

impl Drop for SeatSlotLease {
    fn drop(&mut self) {
        self.gate.release_slot(self.slot);
    }
}

impl SeatManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_default_orchestrator() -> Self {
        let mut m = Self::new();
        let _ = m.add_seat("agent-1 (оркестратор)", vec![]);
        m
    }

    pub fn seats(&self) -> &[AgentSeat] {
        &self.seats
    }

    pub fn is_control_live(&self) -> bool {
        self.control_live || self.seats.iter().any(|s| s.control_live)
    }

    pub fn is_dynamic_interaction_enabled(&self) -> bool {
        self.dynamic_interaction_enabled
    }

    pub fn set_dynamic_interaction(&mut self, enabled: bool) {
        self.dynamic_interaction_enabled = enabled;
        if enabled {
            self.gate.clear_emergency_stop();
            for seat in &mut self.seats {
                seat.pointer.enabled = true;
                seat.keyboard.enabled = true;
                if seat.status == "остановлено пользователем"
                    || seat.status == "отключено по умолчанию"
                {
                    seat.status = "готов".into();
                }
            }
        } else {
            self.control_live = false;
            self.file_locks.clear();
            for seat in &mut self.seats {
                seat.control_live = false;
                seat.pointer.enabled = false;
                seat.pointer.active = false;
                seat.keyboard.enabled = false;
                seat.keyboard.typing = false;
                seat.status = "отключено по умолчанию".into();
            }
        }
    }

    pub fn gate(&self) -> Arc<AtomicSeatGate> {
        Arc::clone(&self.gate)
    }

    pub fn is_emergency_stopped(&self) -> bool {
        self.gate.is_emergency_stopped()
    }

    pub fn clear_emergency_stop(&mut self) {
        self.gate.clear_emergency_stop();
    }

    pub fn add_seat(
        &mut self,
        agent_name: impl Into<String>,
        owned_paths: Vec<String>,
    ) -> Result<&AgentSeat> {
        if self.seats.len() >= MAX_AGENT_SEATS {
            bail!(
                "Достигнут лимит курсоров и виртуальных клавиатур ({MAX_AGENT_SEATS}); удалите неиспользуемое место перед добавлением нового"
            );
        }
        let name = agent_name.into();
        if name.trim().is_empty() {
            bail!("Укажите имя агента для нового курсора/клавиатуры");
        }
        let normalized: Vec<String> = owned_paths
            .into_iter()
            .map(|p| normalize_rel(&p))
            .filter(|p| !p.is_empty())
            .collect();

        // Проверяем отсутствие пересечения owned_paths с существующими местами.
        for existing in &self.seats {
            for ep in &existing.owned_paths {
                for np in &normalized {
                    if paths_overlap(ep, np) {
                        bail!(
                            "Конфликт владения путями: «{np}» пересекается с «{ep}» ({})",
                            existing.seat_id
                        );
                    }
                }
            }
        }

        self.next_seq += 1;
        let idx = self.next_seq;
        let color_slot = ((idx - 1) % MAX_AGENT_SEATS) as u8;
        let seat = AgentSeat {
            seat_id: format!("seat-{idx}"),
            agent_name: name.trim().to_string(),
            color_slot,
            owned_paths: normalized,
            pointer: VirtualPointer {
                enabled: self.dynamic_interaction_enabled,
                ..VirtualPointer::default()
            },
            keyboard: VirtualKeyboard {
                keyboard_id: format!("kbd-{idx}"),
                enabled: self.dynamic_interaction_enabled,
                typing: false,
                last_keys: String::new(),
                ops_count: 0,
            },
            control_live: false,
            status: if self.dynamic_interaction_enabled {
                "готов".into()
            } else {
                "отключено по умолчанию".into()
            },
            last_op_tsc: asm_accel::asm_read_tsc(),
        };
        self.seats.push(seat);
        Ok(self.seats.last().unwrap())
    }

    pub fn upsert_runtime_seat(&mut self, incoming: AgentSeat) {
        if let Some(existing) = self
            .seats
            .iter_mut()
            .find(|s| s.seat_id == incoming.seat_id)
        {
            *existing = incoming;
        } else if self.seats.len() < MAX_AGENT_SEATS {
            self.seats.push(incoming);
        }
        self.control_live = self.seats.iter().any(|s| s.control_live);
    }

    pub fn remove_seat(&mut self, seat_id: &str) -> bool {
        let before = self.seats.len();
        self.seats.retain(|s| s.seat_id != seat_id);
        self.file_locks.retain(|_, owner| owner != seat_id);
        self.control_live = self.seats.iter().any(|s| s.control_live);
        self.seats.len() < before
    }

    pub fn toggle_pointer(&mut self, seat_id: &str, enabled: bool) -> Result<()> {
        if enabled && self.gate.is_emergency_stopped() {
            self.gate.clear_emergency_stop();
        }
        let seat = self
            .seats
            .iter_mut()
            .find(|s| s.seat_id == seat_id)
            .ok_or_else(|| anyhow::anyhow!("Место {seat_id} не найдено"))?;
        seat.pointer.enabled = enabled;
        if !enabled {
            seat.pointer.active = false;
        }
        Ok(())
    }

    pub fn toggle_keyboard(&mut self, seat_id: &str, enabled: bool) -> Result<()> {
        if enabled && self.gate.is_emergency_stopped() {
            self.gate.clear_emergency_stop();
        }
        let seat = self
            .seats
            .iter_mut()
            .find(|s| s.seat_id == seat_id)
            .ok_or_else(|| anyhow::anyhow!("Место {seat_id} не найдено"))?;
        seat.keyboard.enabled = enabled;
        if !enabled {
            seat.keyboard.typing = false;
        }
        Ok(())
    }

    pub fn release_seat_locks(&mut self, seat_id: &str) {
        self.file_locks.retain(|_, owner| owner != seat_id);
        if let Some(seat) = self.seats.iter_mut().find(|s| s.seat_id == seat_id) {
            seat.control_live = false;
            seat.pointer.active = false;
            seat.keyboard.typing = false;
            seat.status = "ожидание".into();
        }
        self.control_live = self.seats.iter().any(|s| s.control_live);
    }

    /// Аварийный стоп: немедленно взводит аппаратный `AtomicSeatGate` (`xchg` с неявным LOCK),
    /// отключает все виртуальные клавиатуры/курсоры агентов и снимает блокировки.
    pub fn emergency_stop(&mut self) {
        let _ = self.gate.trip_emergency_stop();
        self.control_live = false;
        self.file_locks.clear();
        for seat in &mut self.seats {
            seat.control_live = false;
            seat.pointer.active = false;
            seat.keyboard.typing = false;
            seat.keyboard.enabled = false;
            seat.status = "остановлено пользователем".into();
            seat.last_op_tsc = asm_accel::asm_read_tsc();
        }
    }

    fn acquire_gate_lease(&self, seat_id: &str) -> Result<SeatSlotLease> {
        let slot = self
            .seats
            .iter()
            .find(|s| s.seat_id == seat_id)
            .map(|s| s.color_slot)
            .ok_or_else(|| anyhow::anyhow!("Место {seat_id} не найдено"))?;
        if !self.gate.try_acquire_slot(slot) {
            if self.gate.is_emergency_stopped() {
                bail!(
                    "Аппаратный аварийный стоп активен (AtomicSeatGate); включите курсор/клавиатуру или снимите стоп вручную"
                );
            }
            bail!("Слот #{slot} ({seat_id}) занят параллельной операцией в AtomicSeatGate");
        }
        Ok(SeatSlotLease {
            gate: Arc::clone(&self.gate),
            slot,
        })
    }

    fn check_and_lock_write(&mut self, seat_id: &str, rel_file: &str) -> Result<()> {
        let norm = normalize_rel(rel_file);
        let seat = self
            .seats
            .iter()
            .find(|s| s.seat_id == seat_id)
            .ok_or_else(|| anyhow::anyhow!("Место {seat_id} не найдено"))?;
        if !seat.keyboard.enabled {
            bail!(
                "Виртуальная клавиатура {} отключена пользователем",
                seat.keyboard.keyboard_id
            );
        }
        if !seat.owned_paths.is_empty() && !seat.owned_paths.iter().any(|o| is_path_owned(o, &norm))
        {
            bail!(
                "Место {seat_id} не владеет путём «{norm}» (owned_paths: {:?})",
                seat.owned_paths
            );
        }
        if let Some(holder) = self.file_locks.get(&norm)
            && holder != seat_id
        {
            bail!("Файл «{norm}» заблокирован другим курсором/клавиатурой ({holder})");
        }
        self.file_locks.insert(norm, seat_id.to_string());
        Ok(())
    }

    /// Применяет реальное динамическое редактирование файла от имени конкретного места (курсор + клавиатура)
    /// строго через `WorkspaceGuard::open_read_no_follow` и `atomic_write_no_follow` с аппаратной
    /// блокировкой слота в `AtomicSeatGate` и SSE2-подсчётом координат (`asm_accel`).
    pub fn apply_live_edit(
        &mut self,
        guard: &WorkspaceGuard,
        seat_id: &str,
        op: LiveEditOp,
    ) -> Result<LiveEditResult> {
        if !self.dynamic_interaction_enabled {
            bail!(
                "Динамическое взаимодействие отключено по умолчанию; включите его явно перед управлением курсорами и клавиатурами"
            );
        }
        let _lease = self.acquire_gate_lease(seat_id)?;
        match op {
            LiveEditOp::MoveCursor { file, line, col } => {
                let (rel, sha) = guard.hash_file_no_follow(&file)?;
                let norm = normalize_rel(&rel);
                let seat = self
                    .seats
                    .iter_mut()
                    .find(|s| s.seat_id == seat_id)
                    .ok_or_else(|| anyhow::anyhow!("Место {seat_id} не найдено"))?;
                if !seat.pointer.enabled {
                    bail!("Курсор места {seat_id} отключён пользователем");
                }
                let line = line.max(1);
                let col = col.max(1);
                seat.pointer.target_file = Some(norm.clone());
                seat.pointer.line = line;
                seat.pointer.col = col;
                seat.pointer.active = true;
                seat.status = format!("курсор → {norm}:{line}:{col}");
                seat.last_op_tsc = asm_accel::asm_read_tsc();
                Ok(LiveEditResult {
                    seat_id: seat_id.to_string(),
                    file: norm,
                    new_line: line,
                    new_col: col,
                    sha256: sha,
                    changed_lines: (line, line),
                })
            }
            LiveEditOp::InsertAt {
                file,
                line,
                col,
                text,
            } => {
                let (mut f, rel) = guard.open_read_no_follow(&file)?;
                let norm = normalize_rel(&rel);
                self.check_and_lock_write(seat_id, &norm)?;
                let mut original = String::new();
                f.by_ref()
                    .take((MAX_LIVE_EDIT_BYTES + 1) as u64)
                    .read_to_string(&mut original)
                    .context("Не удалось прочитать UTF-8 файл")?;
                drop(f);
                if original.len() + text.len() > MAX_LIVE_EDIT_BYTES {
                    bail!("Динамическая правка превышает лимит размера {MAX_LIVE_EDIT_BYTES} байт");
                }

                let target_line = line.max(1);
                let target_col = col.max(1);
                let had_trailing_nl = original.ends_with('\n');
                let mut lines: Vec<String> = if original.is_empty() {
                    vec![String::new()]
                } else {
                    original.lines().map(str::to_owned).collect()
                };
                while lines.len() < target_line {
                    lines.push(String::new());
                }
                let line_ref = &mut lines[target_line - 1];
                let byte_idx = char_col_to_byte_idx(line_ref, target_col);
                line_ref.insert_str(byte_idx, &text);

                let mut updated = lines.join("\n");
                if had_trailing_nl || !updated.ends_with('\n') {
                    updated.push('\n');
                }
                guard.atomic_write_no_follow(&norm, updated.as_bytes())?;
                let (_, sha) = guard.hash_file_no_follow(&norm)?;

                let added_newlines = asm_accel::count_newlines(text.as_bytes());
                let new_line = target_line + added_newlines;
                let new_col = if let Some((_, tail)) = text.rsplit_once('\n') {
                    tail.chars().count() + 1
                } else {
                    target_col + text.chars().count()
                };
                let preview: String = text.replace('\n', "↵").chars().take(48).collect();
                self.update_seat_after_write(
                    seat_id,
                    &norm,
                    new_line,
                    new_col,
                    Some(new_line),
                    format!("+{preview}"),
                );
                Ok(LiveEditResult {
                    seat_id: seat_id.to_string(),
                    file: norm,
                    new_line,
                    new_col,
                    sha256: sha,
                    changed_lines: (target_line, new_line),
                })
            }
            LiveEditOp::ReplaceLineRange {
                file,
                start_line,
                end_line,
                replacement,
            } => {
                let (mut f, rel) = guard.open_read_no_follow(&file)?;
                let norm = normalize_rel(&rel);
                self.check_and_lock_write(seat_id, &norm)?;
                let mut original = String::new();
                f.by_ref()
                    .take((MAX_LIVE_EDIT_BYTES + 1) as u64)
                    .read_to_string(&mut original)
                    .context("Не удалось прочитать UTF-8 файл")?;
                drop(f);
                let mut lines: Vec<String> = original.lines().map(str::to_owned).collect();
                let s = start_line.max(1);
                let e = end_line.max(s).min(lines.len().max(1));
                if s > lines.len().max(1) {
                    bail!("Диапазон строк {s}..={e} вне границ файла {norm}");
                }
                let rep_lines: Vec<String> = replacement.lines().map(str::to_owned).collect();
                let new_end = s + rep_lines.len().saturating_sub(1);
                if lines.is_empty() {
                    lines = rep_lines;
                } else {
                    lines.splice((s - 1)..e, rep_lines);
                }
                let mut updated = lines.join("\n");
                if !updated.ends_with('\n') {
                    updated.push('\n');
                }
                if updated.len() > MAX_LIVE_EDIT_BYTES {
                    bail!("Итоговый файл превышает лимит {MAX_LIVE_EDIT_BYTES} байт");
                }
                guard.atomic_write_no_follow(&norm, updated.as_bytes())?;
                let (_, sha) = guard.hash_file_no_follow(&norm)?;
                let (diff_line, diff_col) = asm_accel::first_diff_line_col(&original, &updated);
                let preview: String = replacement.replace('\n', "↵").chars().take(48).collect();
                self.update_seat_after_write(
                    seat_id,
                    &norm,
                    new_end,
                    diff_col,
                    Some(new_end),
                    format!("L{s}-{e}@L{diff_line}→{preview}"),
                );
                Ok(LiveEditResult {
                    seat_id: seat_id.to_string(),
                    file: norm,
                    new_line: new_end,
                    new_col: diff_col,
                    sha256: sha,
                    changed_lines: (s, new_end),
                })
            }
            LiveEditOp::ReplaceText {
                file,
                old_text,
                new_text,
            } => {
                if old_text.is_empty() {
                    bail!("old_text не может быть пустым");
                }
                let (mut f, rel) = guard.open_read_no_follow(&file)?;
                let norm = normalize_rel(&rel);
                self.check_and_lock_write(seat_id, &norm)?;
                let mut original = String::new();
                f.by_ref()
                    .take((MAX_LIVE_EDIT_BYTES + 1) as u64)
                    .read_to_string(&mut original)
                    .context("Не удалось прочитать UTF-8 файл")?;
                drop(f);
                let count = original.matches(&old_text).count();
                if count != 1 {
                    bail!(
                        "ReplaceText требует ровно 1 совпадение old_text в {norm} (найдено {count})"
                    );
                }
                let pos = original.find(&old_text).unwrap();
                let before = &original[..pos];
                let start_line = asm_accel::count_newlines(before.as_bytes()) + 1;
                let updated = original.replacen(&old_text, &new_text, 1);
                if updated.len() > MAX_LIVE_EDIT_BYTES {
                    bail!("Итоговый файл превышает лимит {MAX_LIVE_EDIT_BYTES} байт");
                }
                guard.atomic_write_no_follow(&norm, updated.as_bytes())?;
                let (_, sha) = guard.hash_file_no_follow(&norm)?;

                let end_line = start_line + asm_accel::count_newlines(new_text.as_bytes());
                let new_col = if let Some((_, tail)) = new_text.rsplit_once('\n') {
                    tail.chars().count() + 1
                } else {
                    let line_prefix = before.rsplit_once('\n').map(|(_, t)| t).unwrap_or(before);
                    line_prefix.chars().count() + new_text.chars().count() + 1
                };
                let preview: String = new_text.replace('\n', "↵").chars().take(48).collect();
                self.update_seat_after_write(
                    seat_id,
                    &norm,
                    end_line,
                    new_col,
                    Some(end_line),
                    format!("edit:{preview}"),
                );
                Ok(LiveEditResult {
                    seat_id: seat_id.to_string(),
                    file: norm,
                    new_line: end_line,
                    new_col,
                    sha256: sha,
                    changed_lines: (start_line, end_line),
                })
            }
        }
    }

    fn update_seat_after_write(
        &mut self,
        seat_id: &str,
        file: &str,
        line: usize,
        col: usize,
        sel_end: Option<usize>,
        keys_preview: String,
    ) {
        if let Some(seat) = self.seats.iter_mut().find(|s| s.seat_id == seat_id) {
            seat.pointer.target_file = Some(file.to_string());
            seat.pointer.line = line;
            seat.pointer.col = col;
            seat.pointer.selection_end_line = sel_end;
            seat.pointer.active = true;
            seat.keyboard.typing = true;
            seat.keyboard.last_keys = keys_preview;
            seat.keyboard.ops_count += 1;
            seat.control_live = true;
            seat.status = format!("печатает в {file}:{line}:{col}");
            seat.last_op_tsc = asm_accel::asm_read_tsc();
        }
        self.control_live = true;
    }
}

fn char_col_to_byte_idx(line: &str, col_1based: usize) -> usize {
    let target_chars = col_1based.saturating_sub(1);
    line.char_indices()
        .nth(target_chars)
        .map(|(b, _)| b)
        .unwrap_or(line.len())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OsInputBackendCapabilities {
    pub id: String,
    pub label: String,
    pub status: FeatureStatus,
    pub multi_pointer: bool,
    pub multi_keyboard: bool,
    pub linux_status: FeatureStatus,
    pub freebsd_status: FeatureStatus,
    pub notes: String,
}

/// Честная проверка доступности подсистем курсоров и виртуальных клавиатур на хосте.
pub fn probe_os_input_backends() -> Vec<OsInputBackendCapabilities> {
    let uinput_writable = unsafe {
        let p = c"/dev/uinput";
        libc::access(p.as_ptr(), libc::W_OK) == 0
    };
    vec![
        OsInputBackendCapabilities {
            id: "internal_workspace_seats".into(),
            label: "KAIC Live Workspace Seats (внутри приложения)".into(),
            status: FeatureStatus::Tested,
            multi_pointer: true,
            multi_keyboard: true,
            linux_status: FeatureStatus::Tested,
            freebsd_status: FeatureStatus::Unavailable,
            notes: "До 4 независимых агентных курсоров и виртуальных клавиатур (seat-1..4 / kbd-1..4) с эксклюзивными блокировками owned_paths, kernel openat(O_NOFOLLOW) и аварийным стопом.".into(),
        },
        OsInputBackendCapabilities {
            id: "linux_uinput".into(),
            label: "Linux /dev/uinput (виртуальные устройства ОС)".into(),
            status: FeatureStatus::Unavailable,
            multi_pointer: false,
            multi_keyboard: false,
            linux_status: FeatureStatus::Unavailable,
            freebsd_status: FeatureStatus::Unavailable,
            notes: if uinput_writable {
                "/dev/uinput доступен на запись, но системный драйвер инъекции ввода намеренно не включён без отдельного протокола подтверждения и изоляции.".into()
            } else {
                "/dev/uinput недоступен на запись текущему пользователю; системная инъекция ввода не включена.".into()
            },
        },
        OsInputBackendCapabilities {
            id: "wayland_virtual_input".into(),
            label: "Wayland zwlr_virtual_pointer_v1 / zwp_virtual_keyboard_v1".into(),
            status: FeatureStatus::Unavailable,
            multi_pointer: false,
            multi_keyboard: false,
            linux_status: FeatureStatus::Unavailable,
            freebsd_status: FeatureStatus::Unavailable,
            notes: "Запланировано для независимых композиторных seats; зависит от поддержки протокола композитором.".into(),
        },
        OsInputBackendCapabilities {
            id: "x11_xinput2_master".into(),
            label: "X11 XInput2 Master Pointer/Keyboard Seats".into(),
            status: FeatureStatus::Unavailable,
            multi_pointer: false,
            multi_keyboard: false,
            linux_status: FeatureStatus::Unavailable,
            freebsd_status: FeatureStatus::Unavailable,
            notes: "Запланировано для многопользовательских курсоров X11 (`xinput create-master`); пока не реализовано.".into(),
        },
        OsInputBackendCapabilities {
            id: "tmux_control_mode".into(),
            label: "tmux Control Mode (`tmux -CC`, управляемые PTY)".into(),
            status: FeatureStatus::Unavailable,
            multi_pointer: false,
            multi_keyboard: false,
            linux_status: FeatureStatus::Unavailable,
            freebsd_status: FeatureStatus::Unavailable,
            notes: "Планируемая архитектура для TTY-среды: выделенные PTY агентов через фоновый управляющий канал в пользовательских панелях tmux (пока не подключено).".into(),
        },
        OsInputBackendCapabilities {
            id: "freebsd_platform_input".into(),
            label: "FreeBSD platform input / evdev adapters".into(),
            status: FeatureStatus::Unavailable,
            multi_pointer: false,
            multi_keyboard: false,
            linux_status: FeatureStatus::Unavailable,
            freebsd_status: FeatureStatus::Unavailable,
            notes: "Планируемый адаптер для FreeBSD (sysctl/evdev/X11/tmux); на реальном железе FreeBSD не проверялся.".into(),
        },
    ]
}
