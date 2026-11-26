#!/usr/bin/env python3
"""Enable pane-local Codex identity before a live handoff. Never submit a prompt.

Called by the fork installer before replacing the server. Busy clients and drafts
are left untouched and reported as pending; rerunning is safe. The helper reports the client-rendered ID to the old server before swapping it;
the updated server captures subsequent identities passively.
"""
import argparse
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

ANSI = re.compile(r"\x1b\[[0-9;:]*m")
PLACEHOLDERS = {
    "ask codex to do anything", "implement {feature}", "explain this codebase",
    "improve documentation in @filename", "summarize recent commits",
    "write tests for @filename", "find and fix a bug in @filename",
    "use /skills to list available skills", "run /review on my current changes",
}
ID = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$", re.I)


def empty_composer(screen):
    """Require a dim native placeholder, not merely an idle agent status."""
    rows = [row for row in screen.splitlines()[-12:]
            if ANSI.sub("", row).strip().startswith("›")]
    if len(rows) != 1:
        return False
    raw = rows[0]
    value = ANSI.sub("", raw).strip()[1:].strip().casefold()
    return "\x1b[2m" in raw and value in PLACEHOLDERS


def footer_id(screen):
    rows = [row.strip() for row in screen.rstrip().splitlines()]
    if not rows or not rows[-1].startswith(("? for shortcuts", "← for agents · ? for shortcuts")):
        return None
    found = None
    for index in range(len(rows) - 2, max(-1, len(rows) - 10), -1):
        row = rows[index]
        if not row:
            return found if any(r.startswith("›") for r in rows[max(0, index - 8):index]) else None
        if found or row.startswith("›"):
            return None
        value = row.split(" · ", 1)[0].strip()
        if ID.fullmatch(value):
            found = value
    return None


def configured_items():
    path = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))) / "herdr-codex-status-line.json"
    metadata = json.loads(path.read_text())
    values = metadata.get("items")
    if metadata.get("version") != 11 or not isinstance(values, list) or not all(isinstance(v, str) for v in values):
        raise ValueError("missing Codex migration settings; install integration version 11")
    # Slash command arguments are native status field identifiers, never shell code.
    if not all(re.fullmatch(r"[a-z0-9-]+", value) for value in values):
        raise ValueError("unsupported status-line item; cannot migrate safely")
    return ["session-id"] + [value for value in values if value != "session-id"]


class Migration:
    def __init__(self, binary, session):
        self.env = dict(os.environ)
        self.command = [binary]
        if session:
            for key in ("HERDR_SOCKET_PATH", "HERDR_CLIENT_SOCKET_PATH", "HERDR_SESSION"):
                self.env.pop(key, None)
            self.command += ["--session", session]

    def cli(self, *args):
        return subprocess.check_output(self.command + list(args), env=self.env,
                                       text=True, stderr=subprocess.PIPE, timeout=10)

    def pane(self, pane):
        return json.loads(self.cli("pane", "get", pane))["result"]["pane"]

    def process(self, pane):
        return json.loads(self.cli("pane", "process-info", "--pane", pane))["result"]["process_info"]

    def screen(self, pane, fmt="text"):
        return self.cli("agent", "read", pane, "--source", "detection", "--format", fmt)

    def stable(self, pane, pid):
        info = self.process(pane)
        return (info["foreground_process_group_id"] == pid
                and any(p["pid"] == pid and p["name"] == "codex"
                        for p in info.get("foreground_processes", [])))

    def migrate(self, pane, items):
        pid = self.process(pane)["foreground_process_group_id"]
        sid = footer_id(self.screen(pane))
        if not sid:
            if (self.pane(pane).get("agent_status") != "idle"
                    or not self.stable(pane, pid)
                    or self.pane(pane).get("scroll", {}).get("offset_from_bottom") != 0
                    or not empty_composer(self.cli("pane", "read", pane, "--source", "visible", "--format", "ansi"))):
                raise RuntimeError("busy, draft present, or unsupported composer; left untouched")
            command = "/statusline " + " ".join(items)
            self.cli("pane", "send-text", pane, "/statusline")
            # Confirm our exact command is selected before sending Enter. If a
            # user types concurrently, leave the text unsubmitted and report it.
            deadline = time.monotonic() + 3
            while True:
                text = self.screen(pane)
                if "› /statusline" in text and "configure which items appear in the status line" in text.lower():
                    break
                if time.monotonic() >= deadline:
                    raise RuntimeError("could not confirm /statusline; text left unsubmitted")
                time.sleep(.1)
            self.cli("pane", "send-text", pane, " " + " ".join(items))
            # Codex briefly defers Enter following a burst of pasted text.
            time.sleep(.4)
            rows = [row.strip() for row in self.screen(pane).splitlines()]
            if not self.stable(pane, pid) or "› " + command not in rows:
                raise RuntimeError("foreground or composer changed; text left unsubmitted")
            self.cli("pane", "send-keys", pane, "Enter")
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            sid = footer_id(self.screen(pane))
            saved = self.pane(pane).get("agent_session") or {}
            if sid and self.stable(pane, pid):
                if saved.get("value") == sid:
                    return sid
                # The old server may not yet have passive footer capture. Save
                # the confirmed pane-local ID before its alternate screen is
                # discarded during handoff; never guess from files or cwd.
                self.cli("pane", "report-agent-session", pane, "--source", "herdr:codex",
                         "--agent", "codex", "--seq", str(time.time_ns()),
                         "--agent-session-id", sid, "--session-start-source", "resume")
            time.sleep(.2)
        raise RuntimeError("client has not rendered a captured session ID; resume is not yet verified")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--herdr", required=True)
    parser.add_argument("--session", default="")
    args = parser.parse_args()
    migration = Migration(args.herdr, args.session)
    # Configures future clients, preserving their existing status-line items.
    codex_home = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex")))
    if codex_home.is_dir():
        migration.cli("integration", "install", "codex")
    panes = json.loads(migration.cli("pane", "list"))["result"]["panes"]
    pending = 0
    for pane in panes:
        if pane.get("agent") != "codex":
            continue
        pane_id = pane["pane_id"]
        try:
            sid = migration.migrate(pane_id, configured_items())
            print(f"    Codex resume ready     {pane_id} {sid}", flush=True)
        except (RuntimeError, OSError, KeyError, ValueError, subprocess.SubprocessError) as error:
            pending += 1
            print(f"    Codex resume PENDING   {pane_id}: {error}", file=sys.stderr, flush=True)
    if pending:
        print("Codex resume setup is incomplete. Retry after these panes are idle with an empty composer; do not reboot expecting them to resume yet.", file=sys.stderr)
    return 1 if pending else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        sys.exit(f"Codex resume setup failed: {error}")
