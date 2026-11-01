# panel4ai

Panel4AI is a quota monitor for AI subscriptions. It tracks Codex, Claude, Grok and Antigravity, and tells you when a limit resets.

- `panel4ai-server` runs continuously on a VPS. It collects every quota window, confirms reset transitions, persists state in SQLite, and notifies through Postmark email and Bark push.
- The Tauri desktop app is a thin client: it reads the server over Tailscale and draws whatever windows the server reports.
- `deploy/mac/antigravity_push.py` runs on a Mac and pushes Antigravity quota to the server.

## Where each provider's data comes from

| Provider | Source | Runs on |
|---|---|---|
| Codex | Official `codex app-server`, `account/rateLimits/read`; the CLI owns authentication and token refresh | VPS |
| Claude | Consumer usage endpoint with OAuth; not a documented public API, so the adapter stays isolated in `panel4ai-core` | VPS |
| Grok | Official `grok agent stdio`, `_x.ai/billing`; the login token lasts about 6 hours, so the CLI owns its refresh | VPS |
| Antigravity | Official `agy -p /usage --output-format json`, which runs no model turn | Mac, pushed to the VPS |

Antigravity stays on the Mac on purpose. Its Google login carries the broad `cloud-platform` scope, which a public VPS does not need for reading quota, and calling Google's internal quota API directly requires impersonating the official client. The cost is that a sleeping Mac sends nothing: resets it missed are detected at the next push, and a pusher silent for a day raises one alert. Gemini CLI login is not an option for consumer accounts, because [Google stopped serving it on 2026-06-18](https://developers.google.com/gemini-code-assist/docs/deprecations/code-assist-individuals).

## Reset detection

A provider's reset timestamp and usage corroborate each other:

- **Scheduled**: the old boundary has passed and the next one moved ahead, or the window went idle. Confirmed immediately. A window that was not used has nothing to report.
- **Early**: remaining quota rose by at least 10 points before the boundary. Confirmed across two successful observations, so a usage correction does not count.
- **Plan change**: when the provider reports another plan, its fresh windows would look like a reset. It is reported as a plan change instead, and the baselines restart.

Observations, pending candidates and outgoing notifications are durable in SQLite.

## Notifications

| Event | Email | Bark |
|---|---|---|
| Weekly or monthly reset | yes | yes |
| Early reset of any window | yes | yes |
| Scheduled 5-hour reset, under 50% left before it | no | yes |
| Scheduled 5-hour reset, 50% or more left before it | no | no |
| Account or plan change | yes | yes |
| Monitoring failure and recovery | yes | yes |

The `[notify]` section of the server config changes the 5-hour rows.

Failed polls back off: 5, 10, 20 minutes and so on up to an hour. A refused login is retried once an hour after its alert. A `Retry-After` from the provider is honored up to a day, so a malformed value cannot silence a provider until the next restart.

## Tech Stack

- **Frontend**: React 18 + TypeScript + Vite
- **Backend**: Rust, Axum, SQLite, and Tauri 2
- **Build**: npm + Cargo

## Development

### Prerequisites

- [Node.js](https://nodejs.org/) (LTS)
- [Rust](https://www.rust-lang.org/tools/install) (1.77.2+)
- [Tauri CLI](https://v2.tauri.app/start/prerequisites/)

### Setup

```bash
npm install
npm run tauri dev
```

### Build

```bash
npm run tauri build
cargo build --release -p panel4ai-server
```

### Test

```bash
npm run test
cargo test -p panel4ai-core -p panel4ai-server
python3 deploy/mac/test_antigravity_push.py
```

### Lint

```bash
npm run lint
```

## VPS deployment

Build `panel4ai-server` on a Linux system compatible with the VPS, then run:

```bash
sudo PANEL4AI_BIND_ADDR="100.x.y.z:8787" \
  PANEL4AI_POSTMARK_FROM="Panel4AI <quota@example.com>" \
  PANEL4AI_POSTMARK_TO="you@example.com" \
  PANEL4AI_POSTMARK_STREAM="outbound" \
  ./deploy/install-server.sh ./target/release/panel4ai-server
```

Use the VPS Tailscale IPv4 address for `PANEL4AI_BIND_ADDR`. Port 8787 is then unavailable on the public interface. The installer creates:

- `/etc/panel4ai/server.toml`
- `/etc/panel4ai/api-token` (generated once)
- `/etc/panel4ai/ingest-token` (generated once, for the Mac pusher)
- `/etc/panel4ai/postmark-token` and `/etc/panel4ai/bark-key` (initially empty)
- `/var/lib/panel4ai/panel4ai.sqlite3`
- `panel4ai-server.service`

Complete the credentials interactively on the VPS; do not copy a refresh-token file that is still in use on another machine:

```bash
# OpenAI's documented headless login flow
sudo -u panel4ai env HOME=/var/lib/panel4ai \
  CODEX_HOME=/var/lib/panel4ai/codex-home \
  /var/lib/panel4ai/.local/bin/codex login --device-auth

# Run Claude's login as the service account
sudo -u panel4ai env HOME=/var/lib/panel4ai \
  /var/lib/panel4ai/.local/bin/claude auth login

# Install the official Grok CLI from https://x.ai/cli for the service account,
# with its binary at /var/lib/panel4ai/.grok/bin/grok, then use the headless login
sudo -u panel4ai env HOME=/var/lib/panel4ai GROK_DISABLE_AUTOUPDATER=1 \
  /var/lib/panel4ai/.grok/bin/grok login --device-auth

# Paste the Postmark server token and Bark key without placing them in shell history
sudoedit /etc/panel4ai/postmark-token
sudoedit /etc/panel4ai/bark-key
sudo systemctl restart panel4ai-server
```

In the desktop app's settings, set the URL to `http://100.x.y.z:8787` and paste the value from `/etc/panel4ai/api-token`. The settings screen can then send a test email.

### Antigravity pusher on a Mac

Install and sign in to `agy`, put the value of `/etc/panel4ai/ingest-token` in the Keychain as the generic password `panel4ai-ingest`, then render `deploy/mac/com.sean.panel4ai-antigravity-push.plist.template` into `~/Library/LaunchAgents` (replace `@REPO@`, `@SERVER_URL@` and `@HOME@`) and load it with `launchctl bootstrap gui/$(id -u) <plist>`. Errors go to `~/.local/state/panel4ai/antigravity-push.log`.

Operational checks:

```bash
curl http://100.x.y.z:8787/health
sudo systemctl status panel4ai-server
sudo journalctl -u panel4ai-server -n 100 --no-pager
```

The health endpoint reports each provider independently. `stale` means a push
provider has sent nothing for an hour. `openai: "error"` means Codex data is
stale even if `codex login status` still finds a cached credential.
Re-authenticate on the VPS with:

```bash
sudo -u panel4ai env HOME=/var/lib/panel4ai \
  CODEX_HOME=/var/lib/panel4ai/codex-home \
  /var/lib/panel4ai/.local/bin/codex login --device-auth
```

Set `codex_use_app_server = false` only when running an older Codex CLI that
does not expose `account/rateLimits/read`; compatibility mode directly reads
the configured `codex_auth_path`.

Postmark test mode can send only to verified recipient domains. Set the sender, recipient, and message stream with the `PANEL4AI_POSTMARK_*` installer variables shown above; the checked-in values are non-working examples by design.

## Release

Releases are automated via GitHub Actions. To create a new release:

1. Update the version in `package.json`, `src-tauri/tauri.conf.json`, the three `Cargo.toml` files, and the tag in `src/App.tsx`
2. Create and push a version tag:
   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```
3. The release workflow publishes Windows and macOS desktop installers plus a static Linux x86_64 VPS server binary and SHA-256 checksum.

## License

MIT
