# panel4ai

Panel4AI is a two-binary quota monitor for Codex and Claude subscriptions:

- `panel4ai-server` runs continuously on a VPS, polls every quota window, confirms reset transitions, persists state in SQLite, and sends reset email through Postmark.
- The Tauri desktop app reads the VPS snapshots over Tailscale and can fall back to local OAuth data when the VPS is unavailable.

The quota endpoints used by the consumer subscriptions are not documented public APIs. Provider adapters are isolated in `panel4ai-core`, rate limiting is respected, and a reset notification is emitted only when the provider's `reset_at` value advances. A missing reset time is never guessed.

## Features

- Continuous Codex and Claude subscription quota monitoring
- All returned session, weekly, model, and code-review windows are stored on the VPS
- Confirmed-reset email notifications with a durable SQLite outbox and retry schedule
- Bearer-authenticated VPS API, intended to bind only to a Tailscale address
- System tray integration with status indicators (ok/warning/danger)
- OAuth login support for both OpenAI and Claude
- Configurable refresh intervals and alert thresholds
- Desktop notifications when usage exceeds thresholds
- Multiple usage window views (session, weekly, code review)
- Auto-start on boot option
- Minimal UI footprint (360x420px panel)

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
- `/etc/panel4ai/postmark-token` (initially empty)
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

# Paste the Postmark server token without placing it in shell history
sudoedit /etc/panel4ai/postmark-token
sudo systemctl restart panel4ai-server
```

In the desktop app choose `VPS only` or `VPS with local fallback`, set the URL to `http://100.x.y.z:8787`, and paste the value from `/etc/panel4ai/api-token`. The settings screen can then send a test email.

Operational checks:

```bash
curl http://100.x.y.z:8787/health
sudo systemctl status panel4ai-server
sudo journalctl -u panel4ai-server -n 100 --no-pager
```

Postmark test mode can send only to verified recipient domains. Set the sender, recipient, and message stream with the `PANEL4AI_POSTMARK_*` installer variables shown above; the checked-in values are non-working examples by design.

## Release

Releases are automated via GitHub Actions. To create a new release:

1. Update the version in `src-tauri/tauri.conf.json` and `src-tauri/Cargo.toml`
2. Create and push a version tag:
   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```
3. The release workflow publishes Windows and macOS desktop installers plus a static Linux x86_64 VPS server binary and SHA-256 checksum.

## License

MIT
