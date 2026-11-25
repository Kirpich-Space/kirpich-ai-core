<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/kaic-wordmark-on-dark.svg">
    <img alt="KAIC Studio" src="assets/kaic-wordmark-on-light.svg" width="320">
  </picture>
</p>

<h3 align="center">Native Rust AI Agent Studio — Terminal UI, Headless CLI (NDJSON), and Optional eframe/egui Desktop UI.</h3>

<p align="center">
  <a href="../LICENSE"><img alt="License GPL-3.0" src="https://img.shields.io/badge/license-GPL--3.0--only-blue?labelColor=141416"></a>
  <img alt="Rust Edition 2024" src="https://img.shields.io/badge/rust-edition%202024-D2602C?labelColor=141416">
  <img alt="Linux tested" src="https://img.shields.io/badge/Linux-tested-5DB57A?labelColor=141416">
  <img alt="FreeBSD unverified" src="https://img.shields.io/badge/FreeBSD-unverified-6E6E76?labelColor=141416">
</p>

`kaic-studio` is a single pure-**Rust** binary (zero Electron, zero Chromium, zero Node.js) combining:
- **Native Terminal UI** (`kaic-studio tui`, also auto-selected when `DISPLAY` & `WAYLAND_DISPLAY` are absent) — POSIX `termios` raw mode, streaming output, syntax highlighting, collapsible tool blocks, inline diffs, slash autocomplete, HITL confirmations, `Esc` cancellation, and `NO_COLOR`/`SSH`/bare-TTY support;
- **Headless CLI / NDJSON** (`agent run --json`, `providers --json`, `models --json`) for scripts and messenger bridges;
- **Optional native desktop GUI** (`kaic-studio gui`, `eframe/egui`) with Agent Inspector, Live Canvas (dynamic interaction **disabled by default**), local model management, skills, and media generation;
- **7 agent providers** split honestly by execution origin: `KAIC native` (`LocalLlama`, `OllamaLocal`, `OpenAiCompat`, `AnthropicApi`) vs `External CLI` (`CodexCli`, `ClaudeCli`, `AgyCli`).

## Quick Start (Linux x86_64)

```bash
cargo build --locked
cargo test --locked

# Launch Native Terminal UI (terminal / SSH / TTY)
./target/debug/kaic-studio tui --workspace /abs/path/to/workspace

# Inspect provider matrix and local verified GGUF catalog
./target/debug/kaic-studio providers
./target/debug/kaic-studio models

# Run headless dynamic agent turn
./target/debug/kaic-studio agent run \
  --workspace /abs/path/to/workspace \
  --provider local \
  --model gemma26 \
  --policy workspace-write \
  --exec-policy confirm-user-code \
  --task "Read files, fix bugs via apply_patch, and verify with run_check" \
  --json

# Launch optional native Rust GUI
./target/debug/kaic-studio gui
```

## Documentation

- [AGENT_BOUNDARIES.ru.md](AGENT_BOUNDARIES.ru.md) — Security model, 2-axis policies, unified 7-provider matrix, TUI safeguards, dynamic interaction defaults, and documented FreeBSD/`setsid` boundaries.
- [../kaic-project/docs/dynamic-interaction-architecture.md](../kaic-project/docs/dynamic-interaction-architecture.md) — `SeatManager`, `AtomicSeatGate` (`x86_64` ASM + portable Rust fallback), and planned Wayland/X11/`uinput`/FreeBSD/`tmux -CC` backends.
- [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) — Third-party attributions (including Ollama MIT notice).
- [../LICENSE](../LICENSE) — GNU General Public License v3.0 (`GPL-3.0-only`).
