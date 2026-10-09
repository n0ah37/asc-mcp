# Repository Guidelines

## Project Structure & Module Organization

`src/main.rs` is the thin stdio entry point; reusable behavior lives in `src/lib.rs`. Core concerns such as authentication, HTTP retries, uploads, report decoding, configuration, and response shaping are separate modules under `src/`. App Store Connect tool handlers are grouped by domain in `src/server/` (for example, `apps.rs`, `testflight.rs`, and `subscriptions.rs`). Integration-level Rust tests live in `tests/`; module unit tests stay beside the implementation. `scripts/` contains the live API harness and tool-reference generator. Packaging metadata is under `packaging/`, `plugins/`, and `server.json`, while generated tool documentation lives in `docs/TOOLS.md`.

## Build, Test, and Development Commands

- `cargo build --locked` builds the debug binary using `Cargo.lock`.
- `cargo build --release --locked` creates `target/release/asc-mcp`.
- `cargo test --locked` runs unit and integration tests without live Apple credentials.
- `cargo fmt --all -- --check` verifies Rust formatting.
- `cargo clippy --all-targets -- -D warnings` enforces the CI lint gate.
- `python3 scripts/integration_test.py --app <APP_ID>` runs the optional live, read-only API sweep after a release build. Add `--write` only when intentionally testing mutations.

The declared minimum Rust version is 1.88. The server speaks JSON-RPC over stdout, so all diagnostics must remain on stderr.

## Coding Style & Naming Conventions

Use standard `rustfmt` output (four-space indentation). Name modules, functions, variables, and MCP tools in `snake_case`; types and traits use `UpperCamelCase`; constants use `SCREAMING_SNAKE_CASE`. Keep handlers in the matching domain module and share transport behavior through existing client, retry, JSON, and upload helpers. Add safety annotations and catalog classification for every new tool.

## Testing Guidelines

Rust’s built-in test framework and `wiremock` cover API behavior. Name tests as behavior statements, such as `read_only_mode_withholds_every_tool_that_could_write`. Add regression tests for fixes and update `tests/tool_surface.rs` when the served tool contract changes. Packaging/version changes must satisfy `tests/packaging_metadata.rs`. There is no numeric coverage threshold; CI requires tests on Linux, macOS, and Windows plus formatting and Clippy.

## Commit & Pull Request Guidelines

Recent commits use concise imperative subjects, usually Conventional Commit prefixes such as `feat:`, `fix:`, `perf:`, `ci:`, `docs:`, `style:`, and `chore:`; scopes are optional. Keep commits focused. Pull requests should explain the user-visible effect, link relevant issues, call out destructive or API-contract changes, and list verification commands. Regenerate `docs/TOOLS.md` when tool metadata changes.

## Security & Configuration

Never commit App Store credentials, `.env`, `AuthKey_*.p8`, or `appstore-connect.txt`. Use `.env.example` as the configuration reference. Prefer `ASC_READ_ONLY=1` for exploratory live testing, and never log private keys or JWTs.

## graphify

This project has a graphify knowledge graph at graphify-out/.

Rules:
- Before answering architecture or codebase questions, read graphify-out/GRAPH_REPORT.md for god nodes and community structure
- If graphify-out/wiki/index.md exists, navigate it instead of reading raw files
- For cross-module "how does X relate to Y" questions, prefer `graphify query "<question>"`, `graphify path "<A>" "<B>"`, or `graphify explain "<concept>"` over grep — these traverse the graph's EXTRACTED + INFERRED edges instead of scanning files
- After modifying code files in this session, run `graphify update .` to keep the graph current (AST-only, no API cost)
