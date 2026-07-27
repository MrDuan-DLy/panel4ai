#!/usr/bin/env bash
set -euo pipefail

if [[ ${EUID} -ne 0 ]]; then
  echo "Run this installer as root." >&2
  exit 1
fi

binary=${1:-./panel4ai-server}
bind_addr=${PANEL4AI_BIND_ADDR:-127.0.0.1:8787}
postmark_from="${PANEL4AI_POSTMARK_FROM:-Panel4AI <quota@example.com>}"
postmark_to="${PANEL4AI_POSTMARK_TO:-you@example.com}"
postmark_stream="${PANEL4AI_POSTMARK_STREAM:-outbound}"

if [[ ! -x ${binary} ]]; then
  echo "Server binary is missing or not executable: ${binary}" >&2
  exit 1
fi

if ! getent group panel4ai >/dev/null; then
  groupadd --system panel4ai
fi
if ! id panel4ai >/dev/null 2>&1; then
  useradd --system --gid panel4ai --home-dir /var/lib/panel4ai --create-home --shell /usr/sbin/nologin panel4ai
fi

install -d -m 0750 -o panel4ai -g panel4ai /var/lib/panel4ai
install -d -m 0700 -o panel4ai -g panel4ai /var/lib/panel4ai/codex-home
install -d -m 0700 -o panel4ai -g panel4ai /var/lib/panel4ai/.claude
install -d -m 0750 -o root -g panel4ai /etc/panel4ai
install -m 0755 "${binary}" /usr/local/bin/panel4ai-server
install -m 0644 deploy/panel4ai-server.service /etc/systemd/system/panel4ai-server.service

if [[ ! -e /var/lib/panel4ai/codex-home/config.toml ]]; then
  install -m 0600 -o panel4ai -g panel4ai /dev/null /var/lib/panel4ai/codex-home/config.toml
  printf '%s\n' 'cli_auth_credentials_store = "file"' \
    > /var/lib/panel4ai/codex-home/config.toml
  chown panel4ai:panel4ai /var/lib/panel4ai/codex-home/config.toml
fi

if [[ ! -s /etc/panel4ai/api-token ]]; then
  umask 0027
  openssl rand -hex 32 > /etc/panel4ai/api-token
fi
chown root:panel4ai /etc/panel4ai/api-token
chmod 0640 /etc/panel4ai/api-token

if [[ ! -e /etc/panel4ai/postmark-token ]]; then
  install -m 0640 -o root -g panel4ai /dev/null /etc/panel4ai/postmark-token
fi

cat > /etc/panel4ai/server.toml <<EOF
bind_addr = "${bind_addr}"
database_path = "/var/lib/panel4ai/panel4ai.sqlite3"
codex_auth_path = "/var/lib/panel4ai/codex-home/auth.json"
codex_binary_path = "/var/lib/panel4ai/.local/bin/codex"
codex_use_app_server = true
claude_auth_path = "/var/lib/panel4ai/.claude/.credentials.json"
api_token_file = "/etc/panel4ai/api-token"
postmark_token_file = "/etc/panel4ai/postmark-token"
postmark_from = "${postmark_from}"
postmark_to = "${postmark_to}"
postmark_message_stream = "${postmark_stream}"
poll_interval_sec = 300
alert_threshold_percent = 20.0
EOF
chown root:panel4ai /etc/panel4ai/server.toml /etc/panel4ai/postmark-token
chmod 0640 /etc/panel4ai/server.toml /etc/panel4ai/postmark-token

systemctl daemon-reload
systemctl enable --now panel4ai-server.service

echo "Panel4AI server installed on ${bind_addr}."
echo "Health: curl http://${bind_addr}/health"
