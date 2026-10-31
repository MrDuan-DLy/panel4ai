# panel4ai

AI subscription quota monitor. See `README.md` for what it does and how providers work.

| Path | What |
|---|---|
| `crates/panel4ai-core` | Provider adapters (Codex, Claude, Grok) and the shared snapshot types |
| `server/` | `panel4ai-server`: polling, reset detection, SQLite, email and Bark outboxes, HTTP API |
| `src/`, `src-tauri/` | Desktop tray app; only reads the server, holds no provider logic |
| `deploy/` | VPS installer and unit; `deploy/mac/` is the Antigravity pusher |

## Test

```sh
cargo fmt --all -- --check
cargo clippy -p panel4ai-core -p panel4ai-server --all-targets -- -D warnings
cargo test -p panel4ai-core -p panel4ai-server
cargo test --manifest-path src-tauri/Cargo.toml
npm run lint && npx tsc --noEmit && npm run test
python3 deploy/mac/test_antigravity_push.py
```

## Rules

- Code, comments and commits are in English; email and Bark text is in Chinese.
- Keep `/api/v1/snapshots` backward compatible. The home dashboard (`~/Projects/tvdash`) reads it, and new JSON fields must be optional. Window types keep the `hourly5|weekly|daily|monthly[_scope]` shape that dashboard parses.
- Do not add a direct call to Google's `cloudcode-pa` API or put a Google login on the server. Antigravity goes through the official `agy` on the Mac.
- Never print or log tokens. Provider logins belong to their official CLIs wherever a CLI can read the quota.
- Push providers that stop sending are `staleProviders`, never `providerErrors`: the dashboard turns the latter into phone alerts, and a closed laptop is not a failure.

## Production

Runs on `azure-server` as the `panel4ai` service account; operating notes are in `~/Projects/tvdash/docs/vps.md`. A `v*` tag builds the static server binary and the desktop installers. The Mac pusher runs from this repo path through the launchd job `com.sean.panel4ai-antigravity-push`, so do not move the repo without updating that job.
