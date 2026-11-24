<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="Desige/kaic-wordmark-on-dark.svg">
    <img alt="KAIC — Kirpich AI Core" src="Desige/kaic-wordmark-on-light.svg" width="340">
  </picture>
</p>

<h3 align="center">Local-first native Rust AI agent runtime — CLI, native GUI, and messenger-ready NDJSON protocol.</h3>

<p align="center">
  <a href="LICENSE"><img alt="License GPL-3.0" src="https://img.shields.io/badge/license-GPL--3.0-blue?labelColor=141416"></a>
  <img alt="Pure Rust" src="https://img.shields.io/badge/core-Pure%20Rust-D2602C?labelColor=141416">
  <img alt="No Electron" src="https://img.shields.io/badge/Electron-removed%20%2F%20no%20browser-1C1C20?labelColor=0B0B0C">
  <img alt="Linux tested" src="https://img.shields.io/badge/Linux-tested-5DB57A?labelColor=141416">
  <img alt="FreeBSD unverified" src="https://img.shields.io/badge/FreeBSD-unverified-6E6E76?labelColor=141416">
  <img alt="Local-first" src="https://img.shields.io/badge/inference-local--first-5FC2B0?labelColor=141416">
</p>

**KAIC (Kirpich AI Core)** is a dynamic AI agent runtime and local model workbench written in **Rust**.
Instead of producing a one-shot text plan, KAIC acts step-by-step inside a selected workspace — reading files, making targeted edits, running real project builds and tests, adjusting to actual tool output, and delegating bounded subtasks — while keeping **files, execution policies, approvals, and providers strictly under your control**.

One native Rust binary (`kaic-studio`). **No Electron, no Chromium wrapper, no telemetry, no mandatory KAIC account.** Local inference runs directly against your own `llama.cpp` models on `127.0.0.1`; external subscriptions are used only through official locally authenticated CLIs with your explicit consent.

<p align="center">
  <img alt="KAIC Native Rust Architecture" src="Desige/kaic-architecture.svg" width="960">
</p>

> **Pure Rust — No Electron.** KAIC is built around a native **Rust CLI** (with streaming `--json` NDJSON events for terminal workflows and messenger bridges) plus an optional native `eframe/egui` desktop GUI. The earlier Electron/React UI is deprecated and no longer part of the active runtime.

**Target OS matrix: Linux and FreeBSD.** Currently **verified on Linux x86_64**; **FreeBSD is not yet tested** (`llama.rs` and `tools.rs` currently rely on Linux `/proc/net/tcp` and `PR_SET_PDEATHSIG`). Windows and macOS are out of scope.

---

## Current Status (Honest Matrix)

| Status | Component | Evidence / Notes |
|---|---|---|
| **Tested** (Linux x86_64) | Dynamic agent loop, 2-axis policy/tool gateway, single-use SHA-256 approvals, kernel `openat(O_NOFOLLOW)` workspace guard, process-group cancel, bounded subtasks (`<= 2`) with SHA-256 version-bound verification & edit invalidation | Automated Rust contract suite (`cargo test --locked`: 114 passed, 0 failed); real local single-agent E2E edit + `unittest` run on `granite-4.0-h-tiny-Q5_K_M.gguf` |
| **Tested** (Linux x86_64) | Native Rust CLI (`agent run --json`, `providers`, `models`, `--smoke-chat`, `--smoke-agent`) | CLI parser unit tests + live read-only `Codex CLI` smoke (`codex exec --json --sandbox read-only --ephemeral`) |
| **Implemented** | Native Rust GUI (`eframe/egui`, Wayland/X11): Agent Inspector (file policy + code exec policy + provider matrix), live Activity Timeline, subtask badges with SHA-256 hashes, Approve/Deny card, local chat, skills, media tools | Headless `egui` CPU layout/interaction tests pass; live OpenGL window QA pending free GPU |
| **Implemented** (live GPU run pending) | Bounded multi-agent delegation on local GPU model | Verified with deterministic loopback HTTP tests; live local-model 2-subtask GPU run pending |
| **Planned** | Messenger bridges (Telegram / Matrix) over `kaic-studio agent run --json` NDJSON stream | Streaming NDJSON protocol ready; messenger bridge services not started yet |
| **Unavailable** | `Claude Code CLI` & `Antigravity (agy)` adapters; **FreeBSD** port; cgroup v2 / PID-namespace isolation for detached `setsid()` processes | Honestly reported as `unavailable` in `kaic-studio providers` and UI |

---

## CLI-First & Messenger-Ready Interface

KAIC exposes a single native Rust binary for terminal work, automation scripts, messenger bridges, and optional desktop GUI:

```text
┌ KAIC (Pure Rust · Zero Electron) ────────────────────────────────────────────────────────────┐
│ CLI / NDJSON: kaic-studio agent run -C /path/ws -t "..." --json   → Telegram / Matrix bridge │
│ Native GUI  : kaic-studio gui                                     → eframe/egui (Carbon UI)  │
├ 2-Axis Policy Gateway ───────────────────────────────────────────────────────────────────────┤
│ Axis 1 · Files (AgentPolicy) : read-only | approval-required | workspace-write  [O_NOFOLLOW] │
│ Axis 2 · Code  (ExecPolicy)  : disabled  | safe-inspect | confirm-user-code | allow-builds   │
└ Providers: Local llama.cpp (127.0.0.1) · Codex CLI (--sandbox) · Claude/agy (unavailable) ───┘
```

### Key CLI Commands (`kaic-studio`, Rust 1.95+)

```bash
# Build and run automated tests
cargo test --locked
cargo build --locked

# Inspect host provider capabilities and local GGUF catalog
./target/debug/kaic-studio providers --json
./target/debug/kaic-studio models --json

# Run dynamic agent with explicit file + code policies and NDJSON event stream
./target/debug/kaic-studio agent run \
  --workspace /abs/path/to/project \
  --provider local \
  --model gemma26 \
  --policy workspace-write \
  --exec-policy confirm-user-code \
  --task "Inspect calc.py, fix the failing test, and verify with python3 -m unittest" \
  --json

# Run via official locally authenticated Codex CLI (explicit --allow-external required)
./target/debug/kaic-studio agent run \
  --workspace /abs/path/to/project \
  --provider codex \
  --policy read-only \
  --allow-external \
  --task "Summarize build errors without modifying files" \
  --json

# Launch optional native Rust GUI (no Electron)
./target/debug/kaic-studio gui
```

---

## Security & Control Model (EmbedIDE Philosophy)

1. **Separated File Policy vs. Code Execution Policy**:
   - **File tools** (`list_files`, `read_file`, `search_text`, `write_file`, `edit_file`) are confined to the canonical workspace via kernel step-by-step `openat(..., O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)`, `fstat`/`fstatat(AT_SYMLINK_NOFOLLOW)`, and `renameat` — closing TOCTOU symlink-swap escapes. Sensitive paths (`.env`, `.env.*`, `*.pem`, `*.key`, `id_rsa*`, `auth.json`, `.ssh/`, `.codex/`, `.git/config`) are always blocked.
   - **Code execution** (`run_check`) is governed by its own independent `ExecPolicy` (default `confirm-user-code`), even when `workspace-write` is enabled for files. Real build/test systems (`cargo`, `make`, `cmake`, `ctest`, `ninja`, `pytest`, `python3 -m unittest`, `npm`) are supported and **honestly labeled `IsolationLevel::NoneUserPrivileges`** — KAIC never disguises a command allowlist as an OS sandbox.
2. **Single-Use Cryptographically Bound Approvals**:
   - Every approval prompt binds `(request_id, SHA-256(task_id || tool || canonical_args))` and is consumed once (`ApprovalGate::verify_and_consume`). Replaying an old approval or altering arguments is rejected.
3. **Version-Bound Subtask Verification**:
   - Subtasks (`<= 2`) inherit `min(parent_policy, requested)` for both files and code execution, enforce directional `owned_paths` write boundaries, share one resident local GPU model sequentially, and always start as `verified: false` with disk `SHA-256` snapshots (`artifact_hashes`).
   - A subtask transitions to `verified: true` **only** after the orchestrator reads the exact disk `SHA-256` versions and passes a real verification check that actually covers those files. Any subsequent edit immediately invalidates `verified` (`invalidated_after_edit`).
4. **Honest Cancellation Boundary**:
   - Commands run in their own process group (`setpgid(0, 0)` + `PR_SET_PDEATHSIG`). Cancellation kills the entire group (`kill(-pgid, SIGKILL)`) promptly without hanging on inherited pipe descriptors.
   - **Documented limitation**: a child process that explicitly calls `setsid()` to detach into a new session is not terminated by `kill(-pgid, SIGKILL)`; trapping detached daemons requires Linux cgroup v2 (`cgroup.kill`) or a PID namespace (planned).
5. **Subscription Privacy**:
   - Official CLI adapters (`codex exec --json --sandbox ... --ephemeral`) use your already-authenticated local CLI session. KAIC never reads `~/.codex/auth.json`, browser cookies, or third-party credential databases, and refuses external transmission unless you explicitly enable consent.

---

## Provider Capabilities Matrix

| Provider | Status | Streaming | Tool Calling | Approval Granularity | Linux | FreeBSD |
|---|---|---|---|---|---|---|
| **Local `llama.cpp`** (`127.0.0.1`) | `implemented` / single-loop `tested` | Yes (SSE) | OpenAI `tool_calls` (model Jinja template) | Per action (`request_id` + SHA-256 digest) | Tested | Unverified |
| **Codex CLI** (`codex exec --json`) | `implemented` / read-only `tested` | Yes (JSONL) | Native CLI | Launch consent + CLI `--sandbox` | Tested | Unverified |
| **Claude Code CLI** | `unavailable` | — | — | — | Unavailable | Unverified |
| **Antigravity (`agy`)** | `unavailable` | — | — | — | Unavailable | Unverified |

---

## License

This project is licensed under the **GNU General Public License v3.0 (`GPL-3.0-only`)**.
See [`LICENSE`](LICENSE) for the full license text.

---

## Author

**Kirpich Space Company** — local-first Rust engineering tools alongside [EmbedIDE](https://github.com/Kirpich-Space/EmbedIDE).
