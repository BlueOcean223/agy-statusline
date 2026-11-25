# Architecture & Development Guide

[简体中文](development.md) | [English](development.en.md) | [Back to README](../README.en.md)

See [architecture.html](architecture.html) for the design comparison and data flow diagram.

---

## Module Layout

| Component | Files | Responsibility |
|---|---|---|
| **Sidecar Entry & HTTP** | `src/main.rs`, `src/server.rs`, `src/panel.rs` | Managed by `sidecars/statusline/sidecar.json`: runs the live engine, CDP injector, local HTTP side pane server (mutation routes and `GET /api/checkpoint` require UI token auth), relaunch watcher, and updater |
| **Live Telemetry Engine** | `src/live.rs`, `src/ls.rs`, `src/git.rs`, `src/transcript.rs` | Queries read-only `language_server` APIs incrementally, reads transcripts/annotations & on-demand `transcript_full.jsonl` checkpoints, polls workspace Git status |
| **Usage, Quota & Miss Analysis** | `src/usage.rs`, `src/quota.rs`, `src/cli_usage.rs`, `src/miss.rs` | Persists 400-day usage aggregates (`usage-history.json`), 8-day weekly quota samples (`quota-samples.json`), and CLI day index (`cli-index.json`); runs read-only SQLite backfill across Desktop and `agy CLI` DBs, per-day call/conversation drill-downs, 6-cause cache-miss analysis, and weekly quota period/early-reset (`↺`) state tracking |
| **CDP Renderer & i18n** | `src/cdp.rs`, `web/bar.js` | Watches `DevToolsActivePort` every 3s, injects stateless `bar.js` and `i18n/translator.js` (v12) into main and Swarm frames; syncs temporary English toggles, restores English immediately on disable, and auto-reloads stuck `"plugin unavailable"` side panels |
| **Cross-Platform Relaunch** | `src/relaunch.rs` | Detached macOS (`nohup`) and Windows (two-stage PowerShell escaping the Job Object) debug-port relaunch with main-process flag verification |
| **Updater & Data Sync** | `src/updater.rs` | Auto-detects env and OS proxy settings; verifies `SHA256SUMS` for GitHub Releases self-updates (with backup/rollback and `.git` dev-repo protection) and syncs `agy-zh-pack` dict/prices pinned to commit SHA |
| **Lifecycle Hook** | `src/hook.rs`, `hooks.json` | Fire-and-forget local UDP ping (≤10ms) to wake the sidecar right after session/tool events |
| **Side Pane UI** | `web/index.html`, `web/app.js`, `web/styles.css` | Binary-embedded 5-tab frontend (`Overview`, `Turns`, `Usage`, `Theme`, `Settings`) |

---

## Local Read-Only Data Sources

Antigravity passes `ANTIGRAVITY_LS_ADDRESS` and `ANTIGRAVITY_CSRF_TOKEN` to the sidecar (standalone CLI commands discover `language_server` via `ps`/`lsof`):

- **`GetAllCascadeTrajectories`**: conversation run status, step counts, parent/root relationships, workspace paths.
- **`GetCascadeTrajectoryGeneratorMetadata`**: incremental per-call tokens (including Claude cache writes and thinking/response output), TTFT, streaming duration, `responseModel`, and context window estimates via `generatorMetadataOffset`.
- **`GetCascadeTrajectorySteps`**: on-demand incremental step fetches for active/recently finished subagents (`WAITING` approval status, elapsed duration, recent step timeline).
- **`RetrieveUserQuotaSummary`**: 5-hour and weekly quota buckets with reset timestamps.
- **`GetUserStatus`**: account identity (immediately masked + FNV-1a hashed; raw email is never stored) and subscription tier.

**Local Files & Git**:
- `brain/<id>/.system_generated/logs/transcript.jsonl` & `transcript_full.jsonl`: incremental byte-offset reader for per-step timestamps, tool summaries, and historical subagent hints; `transcript_full.jsonl` is read on-demand for compaction checkpoints (guarded by UI token auth, never copied to disk).
- `annotations/<id>.pbtxt`: conversation titles and last-viewed timestamps.
- `conversations/*.db` & `~/.gemini/antigravity-cli/conversations/*.db` (plus `conversation_summaries.db`): opened read-only (`SQLITE_OPEN_READ_ONLY`) with a minimal protobuf wire parser for historical Desktop + `agy CLI` backfill and on-demand day breakdowns.
- Git status: runs `git status --porcelain=v2 --branch` and `git diff HEAD --numstat` with `GIT_OPTIONAL_LOCKS=0`, rate-limited to once every 5s per active workspace.

**Plugin Data Directory** (`~/.gemini/antigravity/plugin_data/agy-statusline/` on macOS, `%USERPROFILE%\.gemini\antigravity\plugin_data\agy-statusline\` on Windows; all state files use atomic writes and back up unparseable files as `*.bad-YYYYMMDDHHMM.*`):
- `config.toml`: settings, appearance, costly-call threshold, and `[[models]]` pricing rules
- `usage-history.json` / `quota-samples.json` / `cli-index.json`: 400-day daily usage (with ended cycles & reset events), 8-day weekly quota samples, and CLI DB day index
- `relaunch-state.json` / `relaunch.log`: relaunch guard state and detached script logs
- `release-cache.json` / `update-state.json` / `backup/`: release metadata, install status, rollback backups
- `remote/`: cached `zh-CN.json`, `zh-CN.rules.json`, `prices.json`, and sync metadata

---

## Building & Debugging

### Build & Hot-Reload the Running Sidecar

After symlinking your checkout into Antigravity's plugin folder (e.g., `ln -s "$(pwd)" ~/.gemini/config/plugins/agy-statusline`):

```bash
cargo build --release && cp target/release/agy-statusline bin/.new && mv -f bin/.new bin/agy-statusline
pkill -f 'agy-statusline sidecar'
```

Antigravity restarts the sidecar with the updated binary automatically within 1–2 seconds.

### CLI Helpers

```bash
# Print a two-line ANSI preview of the most recently viewed conversation
cargo run -- status [-c <conversation-id>]

# Print the raw JSON payload pushed to the in-page statusline (works outside sidecar)
cargo run -- live [<conversation-id>...]

# Attach CDP to a custom browser DevToolsActivePort file for rendering tests
AGY_DEVTOOLS_PORT_FILE=/path/to/DevToolsActivePort cargo run -- sidecar --port 47811
```

### Local Pre-Commit Checks (Mirrors CI)

```bash
cargo fmt --check
for f in web/*.js i18n/translator.js; do node --check "$f"; done
for f in plugin.json hooks.json sidecars/*/sidecar.json i18n/*.json; do python3 -m json.tool "$f" > /dev/null; done
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

---

## CI & Releases

- **`.github/workflows/ci.yml`**: runs `cargo fmt --check`, JS syntax checks, JSON manifest validation, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo test --locked` on macOS, Windows, and Linux.
- **`.github/workflows/release.yml`**: pushing a `v*` tag matching `Cargo.toml` and `plugin.json` builds the macOS universal binary (`aarch64` + `x86_64`) and Windows x64 binary, packages plugin archives with `SHA256SUMS`, and publishes a GitHub Release.

To publish a release:

```bash
# 1. Bump version in Cargo.toml, Cargo.lock, and plugin.json and commit
# 2. Tag and push to trigger the Release workflow
git tag v0.4.0 && git push origin v0.4.0
```
