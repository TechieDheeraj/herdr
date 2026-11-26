#!/bin/sh
# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=codex
# HERDR_INTEGRATION_VERSION=11

set -eu

action="${1:-}"
hook_input_file="$(mktemp "${TMPDIR:-/tmp}/herdr-codex-hook.XXXXXX")" || exit 0
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

case "$action" in
  session) ;;
  *) exit 0 ;;
esac

[ "${HERDR_ENV:-}" = "1" ] || exit 0
[ -n "${HERDR_SOCKET_PATH:-}" ] || exit 0
[ -n "${HERDR_PANE_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

HERDR_ACTION="$action" HERDR_HOOK_INPUT_FILE="$hook_input_file" python3 - <<'PY'
import json
import os
import random
import re
import socket
import subprocess
import time

source = "herdr:codex"
action = os.environ.get("HERDR_ACTION", "")
pane_id = os.environ.get("HERDR_PANE_ID")
socket_path = os.environ.get("HERDR_SOCKET_PATH")
hook_input_file = os.environ.get("HERDR_HOOK_INPUT_FILE")

if not pane_id or not socket_path:
    raise SystemExit(0)

# A shared app-server can inherit the first client's HERDR_PANE_ID. Only a
# hook descended from this pane's foreground process may report to that pane.
# Daemon clients report their own identity through their configured footer.
try:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
        probe.settimeout(0.5)
        probe.connect(socket_path)
        probe.sendall((json.dumps({
            "id": "codex-owner", "method": "pane.process_info",
            "params": {"pane_id": pane_id},
        }) + "\n").encode())
        with probe.makefile() as reply:
            info = json.loads(reply.readline(65536))["result"]["process_info"]
    foreground = {p["pid"] for p in info.get("foreground_processes", [])}
    process_rows = subprocess.check_output(
        ["/bin/ps", "-eo", "pid=,ppid=,args="], text=True, timeout=1
    )
    parents, commands = {}, {}
    for row in process_rows.splitlines():
        fields = row.split(None, 2)
        if len(fields) == 3:
            pid, ppid = map(int, fields[:2])
            parents[pid], commands[pid] = ppid, fields[2]
    parent = os.getpid()
    for _ in range(64):
        # A detached daemon retains its launching CLI's parent relationship.
        # Hooks themselves also start new OS sessions, so SID equality cannot
        # distinguish embedded hooks from shared app-server hooks.
        if re.search(r"\bapp-server(?:-daemon)?\b", commands.get(parent, "")):
            raise SystemExit(0)
        if parent in foreground:
            break
        parent = parents.get(parent, 0)
        if parent <= 1:
            raise SystemExit(0)
    else:
        raise SystemExit(0)
except Exception:
    raise SystemExit(0)

hook_input = {}
if hook_input_file:
    try:
        with open(hook_input_file, encoding="utf-8") as handle:
            content = handle.read()
        if content.strip():
            hook_input = json.loads(content)
    except Exception:
        hook_input = {}

hook_event_name = str(hook_input.get("hook_event_name") or "")
if hook_event_name and hook_event_name != "SessionStart":
    raise SystemExit(0)

request_id = f"{source}:{int(time.time() * 1000)}:{random.randrange(1_000_000):06d}"
report_seq = time.time_ns()
session_id = hook_input.get("session_id")
agent_session_id = session_id if isinstance(session_id, str) and session_id else None
transcript_path = hook_input.get("transcript_path")
if not isinstance(transcript_path, str) or not transcript_path.strip():
    raise SystemExit(0)
inherited_session_id = os.environ.get("CODEX_THREAD_ID")
if inherited_session_id and inherited_session_id != agent_session_id:
    raise SystemExit(0)
session_start_source = hook_input.get("source") if hook_event_name == "SessionStart" else None
if not isinstance(session_start_source, str) or not session_start_source:
    session_start_source = None
if agent_session_id:
    params = {
        "pane_id": pane_id,
        "source": source,
        "agent": "codex",
        "seq": report_seq,
        "agent_session_id": agent_session_id,
    }
    if session_start_source:
        params["session_start_source"] = session_start_source
    request = {
        "id": request_id,
        "method": "pane.report_agent_session",
        "params": params,
    }
else:
    raise SystemExit(0)

try:
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(0.5)
    client.connect(socket_path)
    client.sendall((json.dumps(request) + "\n").encode())
    try:
        client.recv(4096)
    except Exception:
        pass
    client.close()
except Exception:
    pass
PY
