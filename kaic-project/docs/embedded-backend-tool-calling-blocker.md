# EmbeddedBackend tool-calling blocker

Status: blocked; no production Rust or Cargo manifest changes were made.

The requested adapter cannot be integrated reproducibly under the current dependency boundary without relaxing one constraint:

- `kaic-backend` depends on crates.io `llama-cpp-4 = 0.4.2`.
- `llama-cpp-4` exposes the public C API and grammar sampler, but its Rust-facing `llama-cpp-sys-4` bindings do not expose the C++-only `common_chat_parse` or `json_schema_to_grammar` APIs.
- Those APIs are declared in the vendored `llama.cpp/common/chat.h` and `json-schema-to-grammar.h`, use C++ types (`std::string`, vectors, and nlohmann JSON), and may throw exceptions.
- The cached `llama-cpp-sys-4` crate has an `ext_shim` build hook, but that hook is inside the cached dependency. A dependent KAIC `build.rs` cannot add source to that build or receive a reproducible dependency source/build-output contract from Cargo.
- Editing Cargo cache is explicitly disallowed. A Cargo patch would require a project-owned copy/fork of the dependency's vendored llama.cpp build inputs, which is also explicitly disallowed.

A project-owned C++17 shim is therefore implementable only after one boundary is authorized: either an upstream/dependency patch that exposes a documented shim hook and `llama-common` link metadata, or an explicit vendored source/link-input policy for KAIC. Until then, adding Rust declarations would be an ABI guess and would violate the no-fake-capability requirement.

Evidence collected:

- `cargo check --manifest-path kaic-backend/Cargo.toml --features embedded-backend` succeeds only because `src/embedded_backend.rs` remains a `todo!()` scaffold; it does not prove tool calling.
- The non-embedded baseline test suite passes: 160 passed, 0 failed, 1 ignored.
- Cached headers confirm the required APIs are C++-only and absent from the Rust bindings.
- The cached dependency's existing `ext_shim` compiles only through its own dependency `build.rs`, confirming the missing integration boundary.

Next unblock: authorize one minimal reproducible dependency/build integration (preferably an upstream patch exposing a KAIC-selected C++ shim hook and explicit `llama-common` link inputs), then add RED tests for schema-to-grammar and parsed ToolCall before implementing `EmbeddedBackend`.
