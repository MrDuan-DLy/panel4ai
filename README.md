# panel4ai

A lightweight desktop system tray application that monitors your AI API usage quotas for OpenAI and Anthropic Claude in real-time.

## Features

- Real-time usage monitoring for OpenAI and Claude APIs
- System tray integration with status indicators (ok/warning/danger)
- OAuth login support for both OpenAI and Claude
- Configurable refresh intervals and alert thresholds
- Desktop notifications when usage exceeds thresholds
- Multiple usage window views (session, weekly, code review)
- Auto-start on boot option
- Minimal UI footprint (360x420px panel)

## Tech Stack

- **Frontend**: React 18 + TypeScript + Vite
- **Backend**: Rust + Tauri 2
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
```

### Test

```bash
npm run test
```

### Lint

```bash
npm run lint
```

## Release

Releases are automated via GitHub Actions. To create a new release:

1. Update the version in `src-tauri/tauri.conf.json` and `src-tauri/Cargo.toml`
2. Create and push a version tag:
   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```
3. The CI will build the Windows installer and create a GitHub Release automatically.

## License

MIT
