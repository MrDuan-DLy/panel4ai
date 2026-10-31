#!/usr/bin/env python3
"""Push Antigravity quota from this Mac to panel4ai-server.

The Google login stays on this machine: it carries the broad cloud-platform scope, so it
does not belong on a public VPS. Only the official `agy` CLI reads the quota, and the server
receives percentages and reset times. `agy -p /usage` runs no model turn.

Reads the ingest token from the Keychain item `panel4ai-ingest` and the server address from
PANEL4AI_SERVER_URL. Only standard library, because launchd runs it with /usr/bin/python3.
"""
import datetime as dt
import json
import os
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request

PROVIDER = 'antigravity'
KEYCHAIN_SERVICE = 'panel4ai-ingest'
AGY_TIMEOUT_SECS = 90  # a cold start has been seen to take over 60 s
MAX_OUTPUT_BYTES = 1024 * 1024
WINDOW_SECONDS = {'5h': 18_000, 'weekly': 604_800}
WINDOW_BASE = {'5h': 'hourly5', 'weekly': 'weekly'}
# Bucket ids Google uses today, named the way the dashboards already group windows.
BUCKET_SCOPE = {
    'gemini-5h': 'gemini',
    'gemini-weekly': 'gemini',
    '3p-5h': 'claudegpt',
    '3p-weekly': 'claudegpt',
    'gemini-argon-wtu-weekly': 'argon',
}


class AgyError(Exception):
    def __init__(self, kind, message):
        super().__init__(message)
        self.kind = kind
        self.message = message


def scope_of(bucket_id):
    if bucket_id in BUCKET_SCOPE:
        return BUCKET_SCOPE[bucket_id]
    # A new bucket should still appear, under a name derived from its id.
    return ''.join(c for c in bucket_id.lower().replace('weekly', '').replace('5h', '') if c.isalnum())


def parse_reset(value):
    # launchd's Python 3.9 accepts only 3 or 6 fractional digits; whole seconds are enough here.
    whole = re.sub(r'\.\d+', '', value).replace('Z', '+00:00')
    return int(dt.datetime.fromisoformat(whole).timestamp())


def parse_report(text):
    """Turn `agy -p /usage --output-format json` output into ingest windows."""
    try:
        report = json.loads(text)
    except ValueError:
        raise AgyError('invalid', 'agy returned output that is not JSON') from None
    command = report.get('command') or {}
    if report.get('status') != 'SUCCESS' or command.get('name') != 'usage':
        # The report text can name the account, so only its category leaves this machine.
        lowered = json.dumps(report).lower()
        if any(word in lowered for word in ('sign in', 'signin', 'login', 'log in', 'auth')):
            raise AgyError('unauthorized', 'agy is not signed in; run agy on the Mac')
        raise AgyError('transport', f"agy usage report status: {report.get('status')}")
    windows = []
    try:
        for group in (command.get('data') or {}).get('groups') or []:
            for bucket in group.get('buckets') or []:
                window = bucket.get('window')
                bucket_id = bucket.get('id') or ''
                scope = scope_of(bucket_id)
                if window not in WINDOW_SECONDS or not scope:
                    continue
                # Proto JSON omits zero, so an absent fraction on a live bucket means exhausted.
                remaining = bucket.get('remaining_fraction', 0.0)
                reset = bucket.get('reset_time')
                windows.append({
                    'windowType': f'{WINDOW_BASE[window]}_{scope}',
                    'usedPercent': round(max(0.0, min(1.0, 1.0 - float(remaining))) * 100, 2),
                    'resetAt': parse_reset(reset) if reset else 0,
                    'windowSeconds': WINDOW_SECONDS[window],
                })
    except (AttributeError, TypeError, ValueError):
        raise AgyError('invalid', 'agy usage report has an unexpected shape') from None
    if not windows:
        raise AgyError('invalid', 'agy usage report has no known quota buckets')
    return windows


def run_agy(agy_path):
    """Run the report with browser launches disabled, and leave no process behind."""
    work = tempfile.mkdtemp(prefix='panel4ai-agy-')
    try:
        # A failed silent sign-in would otherwise open a browser from this background job.
        blocker = os.path.join(work, 'open')
        with open(blocker, 'w') as handle:
            handle.write('#!/bin/sh\nexit 1\n')
        os.chmod(blocker, stat.S_IRWXU)
        os.link(blocker, os.path.join(work, 'xdg-open'))
        env = dict(os.environ)
        env.update({
            'AGY_CLI_DISABLE_AUTO_UPDATE': 'true',  # only this exact value is honoured
            'BROWSER': blocker,
            'PATH': f"{work}:{env.get('PATH', '/usr/bin:/bin')}",
        })
        process = subprocess.Popen(
            [agy_path, '-p', '/usage', '--output-format', 'json'],
            cwd=work, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, start_new_session=True,
        )
        try:
            output, _ = process.communicate(timeout=AGY_TIMEOUT_SECS)
        except subprocess.TimeoutExpired:
            raise AgyError('transport', f'agy did not finish in {AGY_TIMEOUT_SECS} s') from None
        finally:
            # agy can start MCP servers in its session; they must not outlive the report.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
            process.wait()
        if len(output) > MAX_OUTPUT_BYTES:
            raise AgyError('invalid', 'agy output is unexpectedly large')
        return output.decode('utf-8', 'replace')
    finally:
        shutil.rmtree(work, ignore_errors=True)


def ingest_token():
    return subprocess.run(
        ['/usr/bin/security', 'find-generic-password', '-s', KEYCHAIN_SERVICE, '-w'],
        check=True, capture_output=True, text=True, timeout=20,
    ).stdout.strip()


def post(server, token, payload):
    request = urllib.request.Request(
        f"{server.rstrip('/')}/api/v1/ingest/{PROVIDER}",
        data=json.dumps(payload).encode(),
        headers={'Authorization': f'Bearer {token}', 'Content-Type': 'application/json'},
        method='POST',
    )
    # The server is on the tailnet; a system proxy must not carry the bearer token.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(request, timeout=30) as response:
        return response.status


def find_agy():
    candidates = [os.environ.get('ANTIGRAVITY_CLI_PATH'), os.path.expanduser('~/.local/bin/agy')]
    for candidate in candidates:
        if candidate and os.access(candidate, os.X_OK):
            return candidate
    return shutil.which('agy')


def collect():
    agy = find_agy()
    if not agy:
        return {'error': {'kind': 'unauthorized', 'message': 'agy CLI is not installed on the Mac'}}
    try:
        return {'windows': parse_report(run_agy(agy))}
    except AgyError as error:
        return {'error': {'kind': error.kind, 'message': error.message}}


def main():
    server = os.environ.get('PANEL4AI_SERVER_URL')
    if not server:
        print('PANEL4AI_SERVER_URL is not set', file=sys.stderr)
        return 2
    payload = collect()
    try:
        post(server, ingest_token(), payload)
    except (urllib.error.URLError, OSError, subprocess.SubprocessError) as error:
        # No tailnet yet after waking. The next launchd run tries again.
        print(f'push failed: {type(error).__name__}: {getattr(error, "code", "")}', file=sys.stderr)
        return 1
    if 'error' in payload:
        print(f"reported {payload['error']['kind']}: {payload['error']['message']}", file=sys.stderr)
    return 0


if __name__ == '__main__':
    sys.exit(main())
