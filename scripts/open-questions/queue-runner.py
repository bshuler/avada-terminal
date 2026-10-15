#!/usr/bin/env python3
"""queue-runner.py — carries filed instructions into the Open Questions relay pane once it is free.

Bert's ruling (2026-10-15): when the relay pane cannot take a message now (it is on the tab he
is looking at, or it is holding a live menu), the message is filed into a queue, he is told,
and it flows by itself once the pane is free. The queue is meant to become a visible Avada
module with create/read/update/delete (spec MUST 6 "Shape"); until then this script is the
mechanism and the queue directory is the view.

Layout, under ~/Library/Application Support/avada/open-questions/:
  queue/<YYYY-MM-DD-HHMM>-<slug>.md   waiting, oldest first (delete the file to cancel)
  queue/sent/<same name>              delivered: the pointer line was accepted by the pane
  queue/runner.log                    what happened, one line per event
  queue/runner.pid                    one runner at a time

Free means all of: pane-safe.sh says SAFE (hidden tab, or Bert away), the pane has settled
(`avada ctl read --wait`), and its screen ends in an idle prompt with no live menu. The pointer
line tells the relay to carry the file down by the Down procedure in its AGENTS.md. Never two
copies: a line that is not seen on screen after the submit is logged `held`, not re-sent.

Stdlib only. Run: python3 -I queue-runner.py [--once]   (the live copy sits beside AGENTS.md)
"""
import os
import re
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

HOME = Path.home()
RELAY_DIR = HOME / "Library" / "Application Support" / "avada" / "open-questions"
QUEUE = RELAY_DIR / "queue"
SENT = QUEUE / "sent"
LOG = QUEUE / "runner.log"
PIDFILE = QUEUE / "runner.pid"
RELAY_LOG = RELAY_DIR / "relay-log.md"
PANE_SAFE = HOME / "Library" / "Application Support" / "avada" / "orchestrator" / "pane-safe.sh"
AVADA = os.environ.get("HP_CTL", "/Applications/Avada.app/Contents/MacOS/avada")
RELAY_LABEL = "Open Questions"           # the pane whose label contains this, on a running tab
OLD_LABEL = "(old)"
RETRY_SECS = 60
LIVE_MENU = re.compile(r"Esc to cancel|Enter to select")   # the footer every Claude Code menu shows
PROMPT = re.compile(r"^\s*❯\s?(.*?)\s*$", re.M)                # the input line; group 1 = text after it


def log(msg):
    line = f"{datetime.now().strftime('%Y-%m-%d %H:%M:%S')} {msg}"
    with LOG.open("a") as f:
        f.write(line + "\n")
    print(line, flush=True)


def ctl(*args, timeout=120):
    r = subprocess.run([AVADA, "ctl", *args], capture_output=True, text=True, timeout=timeout)
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def relay_pane():
    """The relay pane id from `ctl tabs`, found by label, never hard-coded."""
    _, out = ctl("tabs")
    for line in out.splitlines():
        if RELAY_LABEL in line and OLD_LABEL not in line and "(running)" in line:
            return line.split()[0]
    return None


def verdict(pane):
    try:
        r = subprocess.run([str(PANE_SAFE), pane], capture_output=True, text=True, timeout=10)
        return (r.stdout or "").strip() or "UNKNOWN"
    except Exception as exc:  # pane-safe missing or app down: not safe, say why
        return f"UNKNOWN({exc.__class__.__name__})"


def screen(pane):
    _, out = ctl("read", pane, "--screen")
    return "\n".join(l for l in out.splitlines() if l.strip())


def tail(pane, n=12):
    return "\n".join(screen(pane).splitlines()[-n:])


def prompt_text(t):
    """Text on the last input line, or None when no prompt is on screen."""
    ms = PROMPT.findall(t)
    return ms[-1] if ms else None


def pane_free(pane):
    """Free = pane-safe SAFE, settled, idle prompt, no live menu, no real unsent input.
    An idle Claude Code prompt usually carries a dim ghost suggestion after the ❯, so text
    there is probed the way the relay brief does: send one x; if the line then reads just x
    the text was a ghost, otherwise it is Bert's or the relay's unsent input and we hold."""
    v = verdict(pane)
    if not v.startswith("SAFE"):
        return False, v
    ctl("read", pane, "--wait", timeout=600)
    t = tail(pane)
    if LIVE_MENU.search(t):
        return False, "live menu on screen"
    if "esc to interrupt" in t:
        return False, "pane busy"
    text = prompt_text(t)
    if text is None:
        return False, "no prompt on screen"
    if text:
        ctl("send", pane, "x")
        time.sleep(1)
        after = prompt_text(tail(pane))
        ctl("keys", pane, "backspace")
        if after != "x":
            return False, "input line holds text not ours"
    return True, v


def pointer_line(sent_path):
    rel = sent_path.relative_to(RELAY_DIR)
    return (f"From Bert, filed while you were busy: carry {rel} to the Manager by the Down "
            f"procedure in AGENTS.md, then tell Bert in one line.")


def deliver(path, pane):
    SENT.mkdir(exist_ok=True)
    dest = SENT / path.name
    path.rename(dest)
    line = pointer_line(dest)
    rc, out = ctl("submit", pane, line)
    if rc != 0:
        dest.rename(path)
        log(f"held {path.name}: submit failed rc={rc} {out.strip()[:200]}")
        return False
    time.sleep(3)
    if line[:40] not in screen(pane):
        log(f"held {dest.name}: pointer not seen on screen after submit; not re-sending")
        with RELAY_LOG.open("a") as f:
            f.write(f"{datetime.now().strftime('%Y-%m-%d %H:%M')} EDT | queue | pointer for {dest.name} submitted but not seen on screen | queue/sent/{dest.name} | held\n")
        return False
    with RELAY_LOG.open("a") as f:
        f.write(f"{datetime.now().strftime('%Y-%m-%d %H:%M')} EDT | queue | runner handed {dest.name} to the relay pane | queue/sent/{dest.name} | delivered\n")
    log(f"delivered {dest.name} to {pane}")
    return True


def pending():
    return sorted(p for p in QUEUE.glob("*.md") if p.is_file())


def main(once=False):
    QUEUE.mkdir(parents=True, exist_ok=True)
    if PIDFILE.exists():
        try:
            os.kill(int(PIDFILE.read_text().strip()), 0)
            print("runner already running", file=sys.stderr)
            return 1
        except (ValueError, ProcessLookupError, PermissionError):
            pass
    PIDFILE.write_text(str(os.getpid()))
    try:
        log(f"runner start pid={os.getpid()} once={once}")
        while True:
            items = pending()
            if not items:
                if once:
                    return 0
                time.sleep(RETRY_SECS)
                continue
            pane = relay_pane()
            if pane is None:
                log("no running relay pane found; waiting")
                time.sleep(RETRY_SECS)
                continue
            free, why = pane_free(pane)
            if not free:
                log(f"waiting: {len(items)} queued, pane {pane} not free ({why})")
                time.sleep(RETRY_SECS)
                continue
            if not deliver(items[0], pane) and once:
                return 2
            time.sleep(5)
    finally:
        PIDFILE.unlink(missing_ok=True)


if __name__ == "__main__":
    sys.exit(main(once="--once" in sys.argv[1:]))
