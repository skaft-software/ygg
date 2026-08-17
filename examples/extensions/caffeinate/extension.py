#!/usr/bin/env python3
"""Keep macOS awake while Ygg is processing a prompt."""

import os
from pathlib import Path
import subprocess
import sys
import threading

from ygg_extension import Extension


CAFFEINATE = Path("/usr/bin/caffeinate")
# Terminal lifecycle notifications normally release the inhibitor. Bound the
# subprocess fallback too in case the host or protocol stream disappears before
# a terminal notification can arrive.
MAX_INHIBIT_SECONDS = 30 * 60
ext = Extension(
    api_version="0.2",
    supported_features=(
        "request_cancellation",
        "content_parts",
        "lifecycle_events",
    ),
)
inhibitor = None
last_error = None
active_turns = set()
state_lock = threading.RLock()


def inhibitor_active():
    global inhibitor, last_error
    if inhibitor is None:
        return False
    returncode = inhibitor.poll()
    if returncode is None:
        return True
    inhibitor = None
    if returncode != 0:
        last_error = f"caffeinate exited with status {returncode}"
    return False


def start_inhibitor():
    global inhibitor, last_error
    if inhibitor_active():
        return True
    inhibitor = None
    last_error = None

    if sys.platform != "darwin":
        last_error = "unsupported platform (macOS only)"
        return False
    if not CAFFEINATE.is_file():
        last_error = f"{CAFFEINATE} was not found"
        return False

    try:
        process = subprocess.Popen(
            [str(CAFFEINATE), "-i", "-t", str(MAX_INHIBIT_SECONDS)],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except OSError as error:
        last_error = str(error)
        return False

    inhibitor = process
    return True


def stop_inhibitor():
    global inhibitor
    process, inhibitor = inhibitor, None
    if process is None:
        return
    try:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=1)
    except subprocess.TimeoutExpired:
        try:
            process.kill()
            process.wait(timeout=1)
        except (OSError, subprocess.TimeoutExpired) as error:
            ext.log.warning("failed to kill caffeinate", error=str(error))
    except OSError as error:
        ext.log.warning("failed to stop caffeinate", error=str(error))


def turn_key(event):
    return (event.get("session_id"), event.get("turn_id"))


@ext.on_lifecycle("turn/started")
def turn_started(event):
    with state_lock:
        active_turns.add(turn_key(event))
        if not start_inhibitor():
            ext.log.warning(
                "caffeinate unavailable",
                error=last_error or "the sleep inhibitor could not be started",
            )


@ext.on_lifecycle("turn/settled")
def turn_settled(event):
    with state_lock:
        active_turns.discard(turn_key(event))
        if not active_turns:
            stop_inhibitor()


@ext.on_lifecycle("session/settled")
def session_settled(event):
    with state_lock:
        session_id = event.get("session_id")
        active_turns.difference_update(
            [owner for owner in active_turns if owner[0] == session_id]
        )
        if not active_turns:
            stop_inhibitor()


@ext.command(
    name="caffeinate",
    description="Show the macOS sleep inhibitor state",
    usage="/caffeinate",
)
def caffeinate_command(arguments):
    if arguments:
        return {"text": "Usage: /caffeinate"}
    if inhibitor_active():
        return {"text": f"Caffeinate is active (pid {inhibitor.pid})."}
    if last_error:
        return {"text": f"Caffeinate is unavailable: {last_error}."}
    return {"text": "Caffeinate is idle; it runs while this extension observes active turns."}


@ext.status("status")
def collect_status(params):
    if not inhibitor_active():
        return None
    return {
        "surface": params.get("surface", "status"),
        "text": "awake",
        "style_role": "extension.caffeinate.active",
        "priority": 10,
    }


@ext.on_shutdown
def shutdown(params):
    with state_lock:
        active_turns.clear()
        stop_inhibitor()


if __name__ == "__main__":
    try:
        ext.run()
    finally:
        stop_inhibitor()
