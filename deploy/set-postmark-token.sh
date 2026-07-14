#!/usr/bin/env bash
set -euo pipefail

if [[ ${EUID} -ne 0 ]]; then
  echo "Run this helper with sudo." >&2
  exit 1
fi

read -rsp "Postmark Server Token: " token
echo
if [[ -z ${token} ]]; then
  echo "Token cannot be empty." >&2
  exit 1
fi

install -m 0640 -o root -g panel4ai /dev/null /etc/panel4ai/postmark-token
printf '%s' "${token}" > /etc/panel4ai/postmark-token
unset token
systemctl restart panel4ai-server.service
echo "Postmark token saved; Panel4AI restarted."
