# Local model directory and real GPT-OSS inference

Verified on Artix Linux, 2026-10-21. No model weights were downloaded.

## Directory contract

KAIC resolves `KAIC_MODELS_DIR` when explicitly set; otherwise it uses
`$HOME/models`, with a lowercase `m`. Missing/empty HOME requires an explicit
override. Empty overrides fail. Setup recursively creates the selected directory,
checks readability, preserves existing files and reports filesystem errors.
It never falls back to an uppercase or legacy mount directory.

Normal backend startup performs setup and one discovery scan before contacting
the inference provider. Offline setup needs neither a control token nor a server:

```bash
kaic --setup-models
```

The command prints JSON. It does not create a database, edit registry YAML,
download weights or load a model. Defaults are derived on every start; explicit
registry paths/provider IDs remain intact, including after temperature persistence.

The authorized API `GET /local-models` exposes the cached startup inventory.
`kaic-cli --local-models` and `/local-models` display it. Restart KAIC to rescan.
`--models` still shows Resource Registry entries and observed load state.
Files and serving models remain distinct: discovery never assigns capabilities,
memory estimates, provider IDs or readiness. Configure those explicitly in the
existing Resource/Capability registries. No parallel model registry was introduced.

Discovery recursively inspects regular GGUF files, sorts stable relative filename
IDs, skips directory symlinks and reports file symlinks/unreadable/invalid files.
It supports little-endian GGUF v2/v3, bounds metadata/descriptors and arithmetic,
validates known GGML tensor block sizes, offsets and payload extents without
reading weights. Zero-tensor tokenizer files, split models, unsupported tensor
types and truncated files are rejected. This is structural validation, not a
checksum or a guarantee of inference quality. JSON diagnostics tolerate non-UTF-8
filenames; internal paths retain their original OS representation.

## Observed model and runtime

The model was initially in **`/home/kirpich/Models`**, while the lowercase
directory was empty. After explicit filesystem approval, it was moved with
`mv -n` on the same filesystem into the canonical directory, without overwriting.
Default setup with `KAIC_MODELS_DIR` unset subsequently found it there:

| Property | Verified value |
| --- | --- |
| File | `/home/kirpich/models/gpt-oss-20b-MXFP4.gguf` |
| Size | 12,109,566,624 bytes |
| GGUF | v3, little endian |
| Tensor count | 459 |
| Architecture | `gpt-oss` |
| `general.file_type` | 38, `MOSTLY_MXFP4_MOE` in installed llama.cpp |
| Inventory ID | `gpt-oss-20b-MXFP4.gguf` |
| Provider ID / registry label | `gpt-oss-20b` / `Local` |
| llama-server | 0.7.0-dev, build 11706, commit `f74e2fa6a` |
| GPU / driver | RTX 4060, 8188 MiB / 615.78.08 |

The binary's RUNPATH points at `/home/kirpich/llama.cpp/build/bin`, whereas its
libraries are under `/home/kirpich/.llama.cpp/build/bin`. A process-scoped
`LD_LIBRARY_PATH` fixes this; no system linker configuration was changed.

The verified server command, after checking no existing server/occupied port:

```bash
LD_LIBRARY_PATH=/home/kirpich/.llama.cpp/build/bin \
/home/kirpich/.llama.cpp/build/bin/llama-server \
  -m /home/kirpich/models/gpt-oss-20b-MXFP4.gguf --alias gpt-oss-20b \
  --host 127.0.0.1 --port 18080 --no-webui --jinja \
  -ngl 12 --cpu-moe -c 2048 -np 1 -b 128 -ub 64 -n 256 \
  --flash-attn on --fit off --sleep-idle-seconds -1
```

Experts remain on CPU; offload is conservative and deterministic. Physical GPU
memory limits are enforced by llama.cpp/CUDA, not inferred from the GGUF size.

## Reproduce the explicit integration test

From the repository root, with that single-model server already running:

```bash
python3 kaic-project/kaic-backend/tests/real_gguf_inference.py \
  /tmp/kaic-audit-krrrb9ap/kaic-backend/target/debug/kaic \
  /tmp/kaic-tui-client-check/target/debug/kaic-cli \
  --models-dir /home/kirpich/models \
  --model-file /home/kirpich/models/gpt-oss-20b-MXFP4.gguf \
  --model-id gpt-oss-20b --footprint-mb 13597 --budget-mb 20000 \
  --artifacts /tmp/kaic-real-gguf-20261021/pipeline-canonical
```

Choose a new artifacts directory for every run. The test creates a fresh control
token in process environment, temporary registry configuration and SQLite,
starts its own KAIC, runs CLI discovery/backend diagnostics, sends a normal task,
polls it, checks idle Scheduler state and verifies persisted SQLite after shutdown.
It refuses an occupied KAIC port. It stops only its own KAIC; the operator owns
llama-server lifecycle. Default unit tests never run this test or require CUDA.

The actual CLI generation command, with the test's token in its environment:

```bash
/tmp/kaic-tui-client-check/target/debug/kaic-cli \
  --backend http://127.0.0.1:4545 \
  --send 'KAIC (Kirpich AI Core) is a local system coordinating AI models. Describe it in exactly one short Russian sentence.'
```

Read completion using the same token and `--show TASK_UUID`; `--send` alone
acknowledges task creation. The successful canonical run used task
`b73b1399-c3ac-43f5-83c7-63eec73d0c93` and returned:

> KAIC – локальная система, координирующая модели искусственного интеллекта.

Verified path: CLI POST with `category:null` → Router `Simple` → Capability Registry
`Local` → Scheduler's observed external model and guarded generation →
llama-server adapter → actual GGUF → Task Store → CLI. The server remains externally
managed: KAIC does not load/unload it. Its provider ID has exactly one resource
entry. Scheduler returned to zero running/busy tasks, no active operation or
recovery warning; SQLite has `Simple`, `done` and the assistant answer as YAML.

The first real run exposed a pre-existing completion race: `Done` could be observed
before the assistant entry was appended. `record_outcome` now uses the existing
atomic Task Store transaction. A deterministic SQLite-trigger regression failed
before the fix and passed after it; cancellation still records a system note.

## Measurements and validation

Before loading: GPU 578 MiB used / 7304 MiB free; host RAM available about
26,141 MiB. After canonical inference: GPU 1766 MiB used / 6117 MiB free
(whole GPU, including desktop); llama-server RSS 12,009,132 KiB, high-water RSS
12,362,184 KiB. Most RSS is file-backed mmap; RSS is not exclusive RAM consumption.
Available host RAM was 26,118,724 KiB; swap was absent.

Canonical request: 93 prompt tokens in 1726.32 ms; 156 generated tokens in
8828.97 ms, **17.56 tokens/s**; server total 10555.30 ms. Generated counts include
reasoning, not just the visible final sentence. This is one smoke measurement.

The test resource estimate is 13,597 MiB = ceil(GGUF bytes / MiB) + 2048 MiB
overhead allowance, against an explicit 20,000 MiB aggregate model-memory budget.
It is a conservative test-specific estimate, not measured VRAM and not a new
Scheduler constant. RAM/VRAM separation and live host pressure remain outside
Scheduler's aggregate accounting. Re-evaluate the estimate for other contexts,
batches, offload settings and concurrent workloads.

Both crates were checked independently because there is no common Cargo workspace.
Each ran `cargo build --workspace --locked --offline`,
`cargo test --workspace --locked --offline` and
`cargo clippy --workspace --all-targets --locked --offline`:

| Check | Result |
| --- | --- |
| Backend build / clippy | exit 0 / exit 0; existing warnings remain |
| Backend tests | 297 passed, 2 ignored, 0 failed |
| CLI build / clippy | exit 0 / exit 0, no clippy warnings |
| CLI tests | 33 library + 11 process + 5 HTTP = 49 passed |
| New directory tests | 25 passed, included in backend total |
| Completion regressions | 2 passed; atomic regression first confirmed red |
| Offline setup process check | PASS, temporary HOME only |
| Synthetic pipeline | all 9 scenarios PASS; independently identified as synthetic |
| Real canonical pipeline | PASS, including unauthenticated inventory rejection and SQLite |

Caches: `CARGO_HOME=/tmp/kaic-audit-krrrb9ap/cargo-home`; backend target
`/tmp/kaic-audit-krrrb9ap/kaic-backend/target`; CLI target
`/tmp/kaic-tui-client-check/target`. Logs use `/tmp/kaic-model-final-*` and
`/tmp/kaic-model-cli-final-*`; real artifacts and server logs use
`/tmp/kaic-real-gguf-20261021`; synthetic artifacts use
`/tmp/kaic-model-synthetic-final`. Temporary artifacts are not versioned.

No dependency or lockfile changes were made. Formatting was checked on changed
code; pre-existing formatting differences in registry tests and CLI `/ack` were
preserved. Two new permission-test literals rejected by clippy were corrected to
octal notation without suppressing lints.

## Skills and development coordination

Three native agents handled separate architecture, llama.cpp/CLI and test/review
responsibilities. Ownership was handed off explicitly; no overlapping writers.
Reports used bounded findings, narrow reads and no inherited full conversation.
Existing Cargo caches, fixtures and the CLI pipeline harness were reused. Exact
token savings were not measured and are not claimed.

Installed one official skill using the inspected system `skill-installer`:
[`cli-creator`](https://github.com/openai/skills/tree/49f948faa9258a0c61caceaf225e179651397431/skills/.curated/cli-creator).
It teaches durable, composable CLI design and fixture-based verification. Existing
`kirpich-rust`, orchestration and testing skills already cover core engineering;
duplicates and unrelated catalog entries were not installed. The new skill is
available on the next turn; it was not used to replace KAIC's existing CLI design.

Static audit: owner `openai`, revision pinned above, Apache-2.0 license, four files
reviewed (SKILL, CLI-pattern reference, agent YAML, license), no bundled executable
scripts/hooks/services. Installed bytes match reviewed SHA-256 values. SKILL hash:
`24058eabf0aafce6def3fb937ac93cbe3fa341c653c658dc589edb77f73d6152`.
Verdict: CONDITIONAL for on-demand future use; installing text does not prove
runtime benefit, save tokens automatically or authorize global PATH changes.
Only public skill content was downloaded; project sources were not uploaded.

Next milestone: explicit, measured model profiles for RAM/VRAM/context and optional
operator-owned server launch assistance. Split GGUF, unsupported/new tensor types,
rescan without restart, capability evaluation and embedded inference remain outside
this implementation. Independent long-term memory is unchanged.
