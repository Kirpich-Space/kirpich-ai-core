# KAIC — Kirpich AI Core

> Local AI orchestration platform for managing multiple Large Language Models through intelligent routing, scheduling and resource management.

---

## Overview

KAIC (Kirpich AI Core) is a local-first AI orchestration platform designed to work with multiple language models as a single intelligent system.

Instead of interacting with one model directly, KAIC automatically selects the most appropriate model for each task, manages model loading and unloading, schedules GPU resources, and provides a unified API for desktop applications.

The project is written primarily in **Rust** with an **Electron** desktop interface.

---

# Goals

- Local-first AI platform
- Multi-model orchestration
- Efficient GPU resource management
- Modular architecture
- High performance
- Future-proof agent system
- Embedded model runtime (planned)

---

# Current Features

- Intelligent Router
- Task Scheduler
- Task Store
- Resource Registry
- Capability Registry
- Electron Desktop UI
- Local AI execution through LM Studio
- YAML-based configuration
- Modular backend architecture

---

# Planned Features

- Embedded GGUF Runtime
- Native llama.cpp backend
- Agent System
- Tool System
- Browser Agent
- File Agent
- CAD Agent
- EmbedIDE Agent
- Telegram Bridge
- Multi-session execution
- Model marketplace

---

# Architecture

```
                Electron UI
                     │
                     ▼
            Control Center API
                     │
        ┌────────────┴────────────┐
        │                         │
     Router                 Task Store
        │                         │
        └────────────┬────────────┘
                     │
                 Scheduler
                     │
              ModelBackend
                     │
        ┌────────────┴────────────┐
        │                         │
   LM Studio Backend      Embedded Backend
       (current)              (planned)
```

---

# Project Structure

```
kaic-project/

├── kaic-backend/
│   ├── router/
│   ├── scheduler/
│   ├── task_store/
│   ├── model_backend/
│   ├── registry/
│   └── api/
│
├── kaic-ui/
│   ├── electron/
│   ├── react/
│   └── assets/
│
├── experiments/
│   └── llama_cpp_smoke/
│
├── config/
│
└── README.md
```

---

# Development Philosophy

KAIC follows several engineering principles:

- Validate technology before integration.
- Replace implementations, not interfaces.
- Avoid premature abstraction.
- Keep architecture modular.
- Build using small vertical slices.
- Prefer deterministic Rust code over hidden magic.
- Local-first whenever possible.

---

# Current Development Status

KAIC is currently under active development.

The backend architecture is operational and continues to evolve.

Current inference backend:

- LM Studio

Future backend:

- Embedded llama.cpp runtime

---

# Technology Stack

## Backend

- Rust
- Tokio
- Axum
- Serde
- Rusqlite

## Frontend

- Electron
- React
- TypeScript

## AI

Current:

- LM Studio

Planned:

- llama.cpp
- GGUF
- Embedded Runtime

---

# Roadmap

- [x] Backend architecture
- [x] Electron interface
- [x] Router
- [x] Scheduler
- [x] Task Store
- [x] LM Studio backend
- [ ] Embedded Backend
- [ ] Agent System
- [ ] Tool System
- [ ] Browser Agent
- [ ] CAD Agent
- [ ] EmbedIDE Integration

---

# License

This project is licensed under the **GNU General Public License v2.0 (GPL-2.0)**.

See the LICENSE file for details.

---

# Author

**Kirpich Space**

Building local AI infrastructure for engineering, development and research.
