//! Низкоуровневое ускорение динамической мультиагентной работы на ассемблере x86_64
//! (базовый ABI x86_64 + SSE2, без требования AVX2/SSE4.2) с переносимым Rust-fallback
//! для остальных архитектур.
//!
//! Зачем здесь ASM:
//! 1. `AtomicSeatGate`: аппаратно-атомарный арбитраж слотов агентных курсоров/клавиатур
//!    (`lock cmpxchg` / `lock and`) и одноинструкционный аварийный стоп (`xchg` с неявным `LOCK`
//!    по Intel SDM Vol. 2B), единый для оркестратора, подзадач и UI-потока без мьютексов.
//! 2. `asm_count_byte`: SSE2-сканирование байта (в частности `'\n'`) блоками по 16 байт
//!    (`movdqu` + `pcmpeqb` + `pmovmskb`) для мгновенного вычисления координат строк при
//!    динамическом редактировании файлов.
//! 3. `asm_common_prefix_len`: SSE2-поиск первого отличающегося байта между версиями буфера
//!    (`movdqu` + `pcmpeqb` + `pmovmskb` + `bsf`) для точного позиционирования агентного курсора.
//! 4. `asm_read_tsc`: сериализованное (`lfence` + `rdtsc`) чтение счётчика тактов без syscall
//!    для телеметрии задержек диспетчеризации курсоров/клавиатур.

use std::cell::UnsafeCell;

#[cfg(not(target_arch = "x86_64"))]
use std::sync::atomic::{AtomicU64, Ordering};

/// Бит 63 слова состояния `AtomicSeatGate` — аппаратный стоп-кран (`control-live` emergency stop).
pub const SEAT_EMERGENCY_STOP_BIT: u64 = 1u64 << 63;

/// Биты `0..=3` — активные аренды слотов агентных курсоров и виртуальных клавиатур (`seat-1..4`).
pub const SEAT_SLOTS_MASK: u64 = 0x0F;

/// Имя активного низкоуровневого бэкенда ускорения (отображается честно в UI и диагностике).
pub fn accel_backend_label() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        "x86_64 inline ASM (SSE2 + LOCK CAS/XCHG + RDTSC)"
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        "portable Rust fallback (non-x86_64)"
    }
}

/// Аппаратно-атомарный арбитр мультиагентных слотов (выровнен по кэш-линии 64 байта
/// во избежание false sharing между потоками оркестратора, подзадач и UI).
#[repr(align(64))]
pub struct AtomicSeatGate {
    #[cfg(target_arch = "x86_64")]
    state: UnsafeCell<u64>,
    #[cfg(not(target_arch = "x86_64"))]
    state: AtomicU64,
}

#[cfg(target_arch = "x86_64")]
unsafe impl Send for AtomicSeatGate {}
#[cfg(target_arch = "x86_64")]
unsafe impl Sync for AtomicSeatGate {}

impl Default for AtomicSeatGate {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for AtomicSeatGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AtomicSeatGate")
            .field("snapshot_hex", &format_args!("0x{:016x}", self.snapshot()))
            .field("emergency_stopped", &self.is_emergency_stopped())
            .field("active_slots_mask", &self.active_slots_mask())
            .finish()
    }
}

impl AtomicSeatGate {
    pub const fn new() -> Self {
        Self {
            #[cfg(target_arch = "x86_64")]
            state: UnsafeCell::new(0),
            #[cfg(not(target_arch = "x86_64"))]
            state: AtomicU64::new(0),
        }
    }

    /// Атомарно захватывает слот `0..=3`.
    /// Возвращает `false`, если слот уже занят другим потоком ИЛИ активен `SEAT_EMERGENCY_STOP_BIT`.
    #[inline(never)]
    pub fn try_acquire_slot(&self, slot: u8) -> bool {
        if slot >= 4 {
            return false;
        }
        let bit = 1u64 << slot;
        #[cfg(target_arch = "x86_64")]
        {
            let ptr = self.state.get();
            let stop_or_bit = SEAT_EMERGENCY_STOP_BIT | bit;
            let ok: u64;
            // SAFETY: `ptr` указывает на выровненный по 64 байтам `u64` внутри `&self`.
            // `lock cmpxchg` гарантирует полную SeqCst-атомарность на x86_64.
            // Регистр `rax` зарезервирован под операнд сравнения `cmpxchg`, стек не используется.
            unsafe {
                core::arch::asm!(
                    "mov rax, qword ptr [{ptr}]",
                    "2:",
                    "test rax, {stop_or_bit}",
                    "jnz 3f",
                    "mov {desired}, rax",
                    "or {desired}, {bit}",
                    "lock cmpxchg qword ptr [{ptr}], {desired}",
                    "jne 2b",
                    "mov {ok}, 1",
                    "jmp 4f",
                    "3:",
                    "xor {ok:e}, {ok:e}",
                    "4:",
                    ptr = in(reg) ptr,
                    bit = in(reg) bit,
                    stop_or_bit = in(reg) stop_or_bit,
                    desired = out(reg) _,
                    ok = lateout(reg) ok,
                    out("rax") _,
                    options(nostack),
                );
            }
            ok != 0
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let stop_or_bit = SEAT_EMERGENCY_STOP_BIT | bit;
            let mut cur = self.state.load(Ordering::Acquire);
            loop {
                if (cur & stop_or_bit) != 0 {
                    return false;
                }
                match self.state.compare_exchange_weak(
                    cur,
                    cur | bit,
                    Ordering::SeqCst,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return true,
                    Err(next) => cur = next,
                }
            }
        }
    }

    /// Атомарно освобождает слот `0..=3` (`lock and`).
    #[inline(never)]
    pub fn release_slot(&self, slot: u8) {
        if slot >= 4 {
            return;
        }
        let clear_mask = !(1u64 << slot);
        #[cfg(target_arch = "x86_64")]
        {
            let ptr = self.state.get();
            // SAFETY: `ptr` валиден и выровнен; `lock and` атомарно снимает бит слота.
            unsafe {
                core::arch::asm!(
                    "lock and qword ptr [{ptr}], {clear_mask}",
                    ptr = in(reg) ptr,
                    clear_mask = in(reg) clear_mask,
                    options(nostack),
                );
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            self.state.fetch_and(clear_mask, Ordering::SeqCst);
        }
    }

    /// Одноинструкционный аппаратный аварийный стоп: выставляет `SEAT_EMERGENCY_STOP_BIT`
    /// и одновременно сбрасывает все активные слоты `0..=3`, возвращая предыдущее состояние.
    #[inline(never)]
    pub fn trip_emergency_stop(&self) -> u64 {
        #[cfg(target_arch = "x86_64")]
        {
            let ptr = self.state.get();
            let mut val = SEAT_EMERGENCY_STOP_BIT;
            // SAFETY: по Intel SDM Vol. 2B инструкция `xchg mem, reg` всегда выполняется
            // с аппаратным протоколом блокировки шины (неявный LOCK, SeqCst).
            unsafe {
                core::arch::asm!(
                    "xchg qword ptr [{ptr}], {val}",
                    ptr = in(reg) ptr,
                    val = inout(reg) val,
                    options(nostack),
                );
            }
            val
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            self.state.swap(SEAT_EMERGENCY_STOP_BIT, Ordering::SeqCst)
        }
    }

    /// Снимает флаг аварийного стопа после явного разрешения пользователя (`lock and`).
    #[inline(never)]
    pub fn clear_emergency_stop(&self) {
        let clear_mask = !SEAT_EMERGENCY_STOP_BIT;
        #[cfg(target_arch = "x86_64")]
        {
            let ptr = self.state.get();
            // SAFETY: `ptr` валиден и выровнен; `lock and` атомарно снимает бит 63.
            unsafe {
                core::arch::asm!(
                    "lock and qword ptr [{ptr}], {clear_mask}",
                    ptr = in(reg) ptr,
                    clear_mask = in(reg) clear_mask,
                    options(nostack),
                );
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            self.state.fetch_and(clear_mask, Ordering::SeqCst);
        }
    }

    /// Атомарное чтение текущего 64-битного слова состояния.
    #[inline]
    pub fn snapshot(&self) -> u64 {
        #[cfg(target_arch = "x86_64")]
        {
            let ptr = self.state.get();
            let out: u64;
            // SAFETY: на x86_64 TSO выровненное 64-битное `mov` атомарно с семантикой Acquire;
            // отсутствие `nomem` запрещает компилятору переупорядочивать чтения вокруг барьера.
            unsafe {
                core::arch::asm!(
                    "mov {out}, qword ptr [{ptr}]",
                    ptr = in(reg) ptr,
                    out = lateout(reg) out,
                    options(nostack, readonly, preserves_flags),
                );
            }
            out
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            self.state.load(Ordering::Acquire)
        }
    }

    #[inline]
    pub fn is_emergency_stopped(&self) -> bool {
        (self.snapshot() & SEAT_EMERGENCY_STOP_BIT) != 0
    }

    #[inline]
    pub fn active_slots_mask(&self) -> u8 {
        (self.snapshot() & SEAT_SLOTS_MASK) as u8
    }
}

/// Переносимая чисто-Rust реализация подсчёта байта (используется на не-x86_64 и в дифф-тестах).
#[inline]
pub fn portable_count_byte(haystack: &[u8], needle: u8) -> usize {
    haystack.iter().filter(|&&b| b == needle).count()
}

/// Переносимая чисто-Rust реализация поиска длины общего байтового префикса.
#[inline]
pub fn portable_common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// Подсчёт вхождений байта `needle` в срезе `haystack` через SSE2 x86_64 ASM
/// (`movdqu` + `pcmpeqb` + `pmovmskb` по 16 байт + точный скалярный хвост).
#[inline(never)]
pub fn asm_count_byte(haystack: &[u8], needle: u8) -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        let ptr = haystack.as_ptr();
        let len = haystack.len();
        if len == 0 {
            return 0;
        }
        let mut count: usize = 0;
        let needle_u32 = needle as u32;
        // SAFETY: `ptr` действителен ровно на `len` байт; цикл читает 16 байт только при `len >= 16`
        // и по 1 байту при `len > 0`. Все модифицируемые регистры (`xmm0`, `xmm1`, `mask`, `tmp`,
        // `ptr`, `len`, `count`, флаги) объявлены явно; стек не затрагивается.
        unsafe {
            core::arch::asm!(
                "movd xmm0, {needle_u32:e}",
                "punpcklbw xmm0, xmm0",
                "punpcklwd xmm0, xmm0",
                "pshufd xmm0, xmm0, 0",
                "cmp {len}, 16",
                "jb 3f",
                "2:",
                "movdqu xmm1, [{ptr}]",
                "pcmpeqb xmm1, xmm0",
                "pmovmskb {mask:e}, xmm1",
                "test {mask:e}, {mask:e}",
                "jz 4f",
                "5:",
                "lea {tmp:e}, [{mask:e} - 1]",
                "add {count}, 1",
                "and {mask:e}, {tmp:e}",
                "jnz 5b",
                "4:",
                "add {ptr}, 16",
                "sub {len}, 16",
                "cmp {len}, 16",
                "jae 2b",
                "3:",
                "test {len}, {len}",
                "jz 7f",
                "6:",
                "cmp byte ptr [{ptr}], {needle_b}",
                "jne 8f",
                "add {count}, 1",
                "8:",
                "add {ptr}, 1",
                "sub {len}, 1",
                "jnz 6b",
                "7:",
                ptr = inout(reg) ptr => _,
                len = inout(reg) len => _,
                count = inout(reg) count,
                needle_u32 = in(reg) needle_u32,
                needle_b = in(reg_byte) needle,
                mask = out(reg) _,
                tmp = out(reg) _,
                out("xmm0") _,
                out("xmm1") _,
                options(nostack, readonly),
            );
        }
        count
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        portable_count_byte(haystack, needle)
    }
}

/// Длина общего байтового префикса двух буферов через SSE2 x86_64 ASM
/// (`movdqu` + `pcmpeqb` + `pmovmskb` + `xor 0xffff` + `bsf`).
/// Используется для мгновенного определения первой изменившейся строки/колонки при живом редактировании.
#[inline(never)]
pub fn asm_common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let min_len = a.len().min(b.len());
    if min_len == 0 {
        return 0;
    }
    #[cfg(target_arch = "x86_64")]
    {
        let pa = a.as_ptr();
        let pb = b.as_ptr();
        let rem = min_len;
        let mut matched: usize = 0;
        // SAFETY: `pa` и `pb` действительны как минимум на `rem == min_len` байт.
        // 16-байтовое чтение выполняется только при `rem >= 16`.
        // `bsf` вызывается только при ненулевом `mask` (верхние 32 бита обнулены 32-битной операцией).
        unsafe {
            core::arch::asm!(
                "cmp {rem}, 16",
                "jb 3f",
                "2:",
                "movdqu xmm0, [{pa}]",
                "movdqu xmm1, [{pb}]",
                "pcmpeqb xmm0, xmm1",
                "pmovmskb {mask:e}, xmm0",
                "xor {mask:e}, 0xffff",
                "jnz 5f",
                "add {pa}, 16",
                "add {pb}, 16",
                "add {matched}, 16",
                "sub {rem}, 16",
                "cmp {rem}, 16",
                "jae 2b",
                "jmp 3f",
                "5:",
                "bsf {mask:e}, {mask:e}",
                "add {matched}, {mask}",
                "jmp 7f",
                "3:",
                "test {rem}, {rem}",
                "jz 7f",
                "6:",
                "mov {tmp}, byte ptr [{pa}]",
                "cmp {tmp}, byte ptr [{pb}]",
                "jne 7f",
                "add {pa}, 1",
                "add {pb}, 1",
                "add {matched}, 1",
                "sub {rem}, 1",
                "jnz 6b",
                "7:",
                pa = inout(reg) pa => _,
                pb = inout(reg) pb => _,
                rem = inout(reg) rem => _,
                matched = inout(reg) matched,
                mask = out(reg) _,
                tmp = out(reg_byte) _,
                out("xmm0") _,
                out("xmm1") _,
                options(nostack, readonly),
            );
        }
        matched
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        portable_common_prefix_len(a, b)
    }
}

/// Подсчёт числа переводов строк `'\n'` в буфере (обёртка над `asm_count_byte`).
#[inline]
pub fn count_newlines(bytes: &[u8]) -> usize {
    asm_count_byte(bytes, b'\n')
}

/// Вычисляет 1-based `(line, col)` первого различия между двумя UTF-8 версиями текста
/// с использованием `asm_common_prefix_len` и `asm_count_byte`.
pub fn first_diff_line_col(before: &str, after: &str) -> (usize, usize) {
    let common_raw = asm_common_prefix_len(before.as_bytes(), after.as_bytes());
    // Откатываемся до ближайшей валидной UTF-8 границы (на случай многобайтного символа).
    let mut boundary = common_raw.min(after.len());
    while boundary > 0 && !after.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let prefix = &after.as_bytes()[..boundary];
    let line = count_newlines(prefix) + 1;
    let line_start = prefix
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let col = after[line_start..boundary].chars().count() + 1;
    (line, col)
}

/// Сериализованное чтение счётчика тактов процессора (`lfence` + `rdtsc`) без syscall.
/// Возвращает `0` на архитектурах без x86_64 TSC.
#[inline(never)]
pub fn asm_read_tsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        let lo: u64;
        // SAFETY: `lfence` и `rdtsc` — непривилегированные базовые инструкции x86_64/SSE2
        // (в пользовательском пространстве Linux `CR4.TSD` снят).
        unsafe {
            core::arch::asm!(
                "lfence",
                "rdtsc",
                "shl rdx, 32",
                "or rax, rdx",
                out("rax") lo,
                out("rdx") _,
                options(nostack, nomem),
            );
        }
        lo
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering as StdOrdering},
    };

    #[test]
    fn asm_count_byte_matches_scalar_across_lengths_alignments_and_utf8() {
        // Проверяем все длины 0..=130 и невыровненные смещения 0..16
        let mut data = Vec::with_capacity(200);
        for i in 0..200u8 {
            data.push(
                if i % 7 == 0 || i == 15 || i == 16 || i == 17 || i == 31 || i == 32 {
                    b'\n'
                } else {
                    b'a' + (i % 26)
                },
            );
        }
        for offset in 0..16 {
            for len in 0..=130 {
                let slice = &data[offset..offset + len];
                let expected = slice.iter().filter(|&&b| b == b'\n').count();
                let actual = asm_count_byte(slice, b'\n');
                assert_eq!(actual, expected, "offset={offset}, len={len}");
            }
        }
        // Крупный UTF-8 буфер (кириллица + многострочный код)
        let big = "fn кириллица() -> usize { 42 }\n".repeat(2048);
        assert_eq!(asm_count_byte(big.as_bytes(), b'\n'), 2048);
        assert_eq!(asm_count_byte(b"", b'\n'), 0);
        assert_eq!(
            asm_count_byte(b"\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n", b'\n'),
            17
        );
    }

    #[test]
    fn asm_common_prefix_len_matches_scalar_at_every_mismatch_position() {
        let base: Vec<u8> = (0..160u8).map(|i| b'A' + (i % 26)).collect();
        for offset in 0..16 {
            for len in 0..=96 {
                let a = &base[offset..offset + len];
                assert_eq!(asm_common_prefix_len(a, a), len);
                for diff_pos in 0..len {
                    let mut b = a.to_vec();
                    b[diff_pos] ^= 0x01;
                    let expected = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
                    let actual = asm_common_prefix_len(a, &b);
                    assert_eq!(
                        actual, expected,
                        "offset={offset} len={len} diff_pos={diff_pos}"
                    );
                }
            }
        }
        // UTF-8 многобайтовая разница
        assert_eq!(
            first_diff_line_col("привет\nмир\n", "привет\nмиру\n"),
            (2, 4)
        );
        assert_eq!(
            first_diff_line_col("строка1\nальфа", "строка1\nбета"),
            (2, 1)
        );
    }

    #[test]
    fn atomic_seat_gate_enforces_mutual_exclusion_and_hardware_emergency_stop() {
        let gate = Arc::new(AtomicSeatGate::new());
        assert_eq!(gate.snapshot(), 0);
        assert!(!gate.try_acquire_slot(4)); // слот вне 0..=3 запрещён

        // 4 параллельных потока конкурируют за 4 слота
        let active_in_slot0 = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for tid in 0..4u8 {
            let g = Arc::clone(&gate);
            let s0 = Arc::clone(&active_in_slot0);
            handles.push(std::thread::spawn(move || {
                for _ in 0..2000 {
                    if g.try_acquire_slot(0) {
                        let prev = s0.fetch_add(1, StdOrdering::SeqCst);
                        assert_eq!(
                            prev, 0,
                            "взаимное исключение слота 0 нарушено потоком {tid}"
                        );
                        s0.fetch_sub(1, StdOrdering::SeqCst);
                        g.release_slot(0);
                    }
                    if g.try_acquire_slot(tid) {
                        g.release_slot(tid);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(gate.active_slots_mask(), 0);

        // Проверяем аварийный стоп: захватываем слоты 1 и 2, затем дёргаем стоп-кран
        assert!(gate.try_acquire_slot(1));
        assert!(gate.try_acquire_slot(2));
        assert_eq!(gate.active_slots_mask(), 0b0110);

        let prev = gate.trip_emergency_stop();
        assert_eq!(prev & SEAT_SLOTS_MASK, 0b0110);
        assert!(gate.is_emergency_stopped());
        assert_eq!(gate.active_slots_mask(), 0);
        // Пока стоп-кран взведён, ни один слот не может быть захвачен
        assert!(!gate.try_acquire_slot(0));
        assert!(!gate.try_acquire_slot(1));

        // После явного снятия стоп-крана работа восстанавливается
        gate.clear_emergency_stop();
        assert!(!gate.is_emergency_stopped());
        assert!(gate.try_acquire_slot(0));
        gate.release_slot(0);
    }

    #[test]
    fn asm_read_tsc_is_monotonic_on_x86_64() {
        let t1 = asm_read_tsc();
        let t2 = asm_read_tsc();
        #[cfg(target_arch = "x86_64")]
        assert!(t2 >= t1 && t2 > 0, "t1={t1}, t2={t2}");
        #[cfg(not(target_arch = "x86_64"))]
        assert_eq!((t1, t2), (0, 0));
    }

    #[test]
    fn portable_fallback_matches_x86_64_asm_for_all_edge_cases() {
        let mut buf = Vec::with_capacity(96);
        for i in 0..96u8 {
            buf.push(if i % 9 == 0 { b'\n' } else { b'x' });
        }
        for start in 0..17 {
            for len in 0..=40 {
                let s = &buf[start..start + len];
                assert_eq!(portable_count_byte(s, b'\n'), asm_count_byte(s, b'\n'));
                let mut mutated = s.to_vec();
                if !mutated.is_empty() {
                    let mid = mutated.len() / 2;
                    mutated[mid] ^= 0x20;
                    assert_eq!(
                        portable_common_prefix_len(s, &mutated),
                        asm_common_prefix_len(s, &mutated)
                    );
                } else {
                    assert_eq!(
                        portable_common_prefix_len(s, b"abc"),
                        asm_common_prefix_len(s, b"abc")
                    );
                }
            }
        }
    }
}
