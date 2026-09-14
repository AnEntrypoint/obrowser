# OxiBrowser AGENTS.md

> This file is loaded into **every** agent session. It is the only context guaranteed to be present
> at the start. Keep it short — detail belongs in `docs/`, not here. Read those files only when the
> task requires them.

## WHAT

OxiBrowser is a **headless browser built in pure Rust**, designed for AI agents and automation.
No Chromium, no V8. Single static binary (C toolchain needed for TLS backend build).

- **Rust-First** — `boa_engine` (JS), `html5ever` (HTML) are pure Rust. TLS via `btls` (BoringSSL C binding) + `ring` (C/asm crypto). Build requires C compiler + cmake.
- **CDP-compatible** — Puppeteer and Playwright connect without knowing they're talking to OxiBrowser.
- **AI-agent extensions** — `OXI.getMarkdown`, `OXI.getPageInfo` via CDP.
- **Agent-first CLI** — `--json` opt-in, `describe` for schema, `skill` for prompts, `session` for multi-step.

4 crates, ~30K lines of Rust:

| Crate | Role |
|-------|------|
| `oxibrowser` | CLI binary: `fetch`, `extract`, `run`, `session`, `serve`, `describe`, `skill`, `version` |
| `oxibrowser-core` | Engine: Browser→Session→Page→Frame, JS runtime, CSS rendering, network |
| `oxibrowser-cdp` | CDP server: WebSocket + 10 domain handlers |
| `oxibrowser-webapi` | DOM: Document, Node, Tree |

## WHY

Existing headless browsers require Chromium — hundreds of MB, slow startup, massive memory.
OxiBrowser provides the same CDP interface at a fraction of the cost, purpose-built for AI agent
workflows: scraping, automation, screenshot capture, Markdown extraction.

OxiBrowser is a **headless browser built in pure Rust**, designed for AI agents and automation.
No Chromium, no V8. Single static binary (C toolchain needed for TLS backend build).

## HOW

### Build & Run

```bash
cargo build                          # Build everything
cargo test --workspace               # Run all tests
cargo test --workspace -- --ignored   # Include real-website integration tests
cargo run -- fetch <url>             # Fetch and render a URL (markdown default)
cargo run -- serve                   # Start CDP server (default :9222)
cargo run -- session                 # Start interactive JSON REPL
```

### Key Entry Points

| Task | Start here |
|------|-----------|
| Add a CLI subcommand | `crates/oxibrowser/src/main.rs` → add clap variant + handler |
| Add a session command | `crates/oxibrowser/src/session/parser.rs` + `executor.rs` |
| Add a CDP command | `crates/oxibrowser-cdp/src/domains/mod.rs` → add domain file |
| Add a JS Web API | `crates/oxibrowser-core/src/js/runtime.rs` → `create_context()` |
| Add DOM operation | `crates/oxibrowser-core/src/js/dom_snapshot.rs` → `DomSnapshot` + `DomMutation` |
| Add network feature | `crates/oxibrowser-core/src/network/` |
| Add CSS rendering | `crates/oxibrowser-core/src/css/` |

### Architecture at a Glance

Core hierarchy: `Browser` → `Session` → `Page` → `Frame`. Each level owns its children and has a unique atomic ID.

JS (`boa_engine`) runs on a dedicated `std::thread` because `Context` is `!Send`. Communication with
the async main thread goes through `mpsc` channels — one bridge each for fetch, localStorage, and
DOM snapshot sync.

### Thread Safety

All shared state uses `Arc<RwLock>` and `AtomicU64`. No exceptions.

### Wasm plugin (`oxibrowser-core/src/wasm_dispatch.rs`)

- Build: `cargo build --release -p oxibrowser-core --no-default-features --target wasm32-wasip1`. The artifact ships as `oxibrowser.wasm` plus its `.sha256` sidecar in the `AnEntrypoint/obrowser-bin` release `v<workspace version>`; agentplug-runner installs it as the `oxibrowser` plugin behind gm's `serp` verb.
- Every verb takes an optional `page` field (default `default`). Each page is its own `Session` with its own cookie jar, at most 16 pages, least recently used evicted. `list-pages` and `close-page` manage them; `capabilities` reports the surface.
- A page's `Session` is built directly, not through `Browser::new_session()`: `Browser` keeps an `Arc` clone of every session for tab bookkeeping, so the take-and-return pattern failed with "session Arc has other owners".
- This repository has no CI since it was re-created. The publishing workflow was a `release.yml` calling `AnEntrypoint/rs-plugkit/.github/workflows/wasm-plugin-release.yml@main` (inputs `wasm_artifact_name: oxibrowser_core.wasm`, `published_asset_basename: oxibrowser`, `release_repo: AnEntrypoint/obrowser-bin`, `release_title_prefix: obrowser`, the build command above) with `PUBLISHER_TOKEN` and `RELEASE_SIGNING_KEY` secrets. Restoring it needs the `PUBLISHER_TOKEN` repository secret first; until then a release is built locally and uploaded with `gh release create`.

## Detailed Docs (read when the task needs them)


