# KAIC Control Center Reliability Implementation Plan

> **For agentic workers:** Execute this plan inline, task by task, with tests first and fresh verification before completion.

**Goal:** Authenticate every Electron Control Center request with the runtime token and reject empty text-model output without weakening existing task-control behavior.

**Architecture:** Electron main owns `KAIC_CONTROL_TOKEN` and performs a fixed allowlisted set of HTTP operations. A context-isolated preload bridge exposes only those operations to the renderer. The text agent loop rejects `GenerateResponse::Text` whose trimmed content is empty, so the existing error path atomically claims `Failed` without overwriting cancel/pause.

**Tech Stack:** Electron/CommonJS, React/TypeScript/Vite, Node test runner, Rust/Tokio/Axum.

**Spec:** `/home/kirpich/.codex/attachments/aaa1330a-2cf4-4df5-a657-f392be649edc/pasted-text.txt`

## Global Constraints

- Work only in the clean worktree based on `origin/media-pipeline-and-scheduler-work` at `28f064e4b1f960df642debd4d9ee8dba9ef10b96`.
- Keep the token out of the repository, URL, renderer state/storage, DOM, logs, and errors.
- Preserve `contextIsolation: true`, `nodeIntegration: false`, public typed UI methods, HTTP/empty-body semantics, human/tool flow, and media fallbacks.
- Do not add dependencies or change Scheduler, SQLite schema, Blender/MCP configuration, media pipeline, or existing closed contracts.

---

### Task 1: Electron-owned authenticated Control Center client

**Files:**
- Create: `kaic-project/kaic-ui/electron/control-center.js`
- Create: `kaic-project/kaic-ui/electron/control-center.test.js`
- Create: `kaic-project/kaic-ui/electron/preload.js`
- Modify: `kaic-project/kaic-ui/electron/main.js`
- Modify: `kaic-project/kaic-ui/src/api/controlCenter.ts`
- Modify: `kaic-project/kaic-ui/src/vite-env.d.ts`
- Modify: `kaic-project/kaic-ui/package.json`

**Interfaces:**
- Main consumes `KAIC_CONTROL_TOKEN` and fixed backend base `http://127.0.0.1:4545`.
- Preload produces `window.kaicControlCenter` with the existing operations only.
- Renderer keeps the existing `controlCenter` method names and `ControlCenterError` behavior.

- [ ] Write Node integration tests using a local HTTP server: verify Bearer header, missing-token fail-closed, 401/network/empty response semantics, token redaction, and rejection of unknown operations.
- [ ] Run `npm test` and record the expected RED caused by the absent main-process authenticated client.
- [ ] Implement the fixed operation table, token-owned request executor, preload bridge, IPC registration, and renderer adapter.
- [ ] Run focused tests and `npm run build`; keep the operation surface fixed and destination impossible to supply from renderer.

### Task 2: Empty text output fails the task

**Files:**
- Modify: `kaic-project/kaic-backend/src/control_center_api.rs`

**Interfaces:**
- `agent_steps` continues to return `LoopOutcome::Answer` for non-empty text and returns an error for empty/whitespace text.
- `run_task_pipeline_inner` continues to use its existing error and `claim_finish` paths.

- [ ] Add table-driven/focused pipeline tests for empty, whitespace, and non-empty responses, asserting status and context roles.
- [ ] Add race coverage showing cancel/pause remains authoritative when empty output arrives.
- [ ] Run the focused Rust tests and record the expected RED (`Done` for empty/whitespace).
- [ ] Add the minimal validation in the text agent loop only, with a clear Russian failure reason.
- [ ] Re-run focused tests plus the existing human/tool-flow tests.

### Task 3: Linux runbook, full verification, and report

**Files:**
- Modify: `kaic-project/README.md`
- Create: `REPORT-kaic-control-center-reliability.md`

- [ ] Document one exported runtime secret inherited by backend and Electron UI, plus the main/preload/IPC boundary.
- [ ] Run all backend tests, all UI tests, UI/backend builds/checks, and `git diff --check`.
- [ ] Run Linux smoke with an ephemeral token: unauthenticated 401, authenticated success, and one Electron-boundary read-only request without 401; use a strict local HTTP double if LM Studio blocks full backend startup.
- [ ] Re-check source checkout status and record base, branch, worktree, root causes, RED→GREEN evidence, changed files, fresh counts, smoke result, cleanup, blockers, and out-of-scope issues.

### Task 4: Acceptance hardening of the privileged Electron boundary

**Files:**
- Modify: `kaic-project/kaic-ui/electron/control-center.js`
- Modify: `kaic-project/kaic-ui/electron/control-center.test.js`
- Modify: `kaic-project/kaic-ui/electron/main.js`
- Create: `kaic-project/kaic-ui/electron/e2e-main.js`
- Create: `kaic-project/kaic-ui/electron/e2e.test.js`
- Modify: `kaic-project/kaic-ui/src/vite-env.d.ts`
- Modify: `kaic-project/kaic-ui/src/api/controlCenter.ts`

**Interfaces:**
- The operation table validates exact arity and boundary-level argument types before building any HTTP request.
- IPC accepts calls only from the expected main frame and trusted renderer entry URL.
- Error envelopes expose `configuration`, `transport`, `http`, `protocol`, or `boundary` without backend bodies or stacks.

- [ ] Replace the aggregate request assertion with nine literal operation cases. Each case independently asserts exact method, path, Bearer value, and body (`undefined`, exact JSON, or empty-response handling).
- [ ] Add behavioral tests that send non-array args, wrong arity/type/range, an unknown operation, an untrusted sender, and malformed JSON; assert the HTTP server sees zero requests for boundary rejects and protocol errors are not transport errors.
- [ ] Run `npm test` in the host network namespace and capture RED for the present missing validation, sender trust, and malformed-response classification.
- [ ] Implement minimal operation-schema validation, safe error kinds, sender/main-frame checks, and navigation/window-open restrictions.
- [ ] Add a real Electron runner whose renderer executes `window.kaicControlCenter.getStatus()` through the production preload and IPC handler; assert the strict HTTP double independently observes `GET /status` and exact Bearer, while child output contains no token/base URL.
- [ ] Run the E2E under `xvfb-run` with lockfile-installed Electron and capture exit code/output.

### Task 5: Locked Rust and final acceptance

**Files:**
- Modify only if a behavioral RED reveals a defect: `kaic-project/kaic-backend/src/control_center_api.rs`
- Create: `REPORT-kaic-control-center-reliability-acceptance.md`

- [ ] Fetch only `Cargo.lock` dependencies with `cargo fetch --locked`, then run the empty/whitespace/non-empty and pause/cancel focused tests.
- [ ] Run full `cargo test --locked` and `cargo build --locked`; do not start LM Studio or use GPU.
- [ ] Run lockfile-exact `npm test`, Electron E2E, `npm run build`, `tsc --noEmit`, JS syntax checks, and `git diff --check`.
- [ ] Compare the complete source-checkout status hash to the initial SHA-256 and report any mismatch rather than relying on an entry count.
- [ ] Report tracked diff and untracked files separately, remove only task-owned temporary resources, and copy the updated plan/report to the central `Промт` folder.
