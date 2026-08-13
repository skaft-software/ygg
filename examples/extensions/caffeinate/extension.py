#!/usr/bin/env python3
"""Keep macOS awake while Ygg is processing a prompt."""

import os
from pathlib import Path
import subprocess
import sys

from ygg_extension import Extension


CAFFEINATE = Path("/usr/bin/caffeinate")
ext = Extension()
inhibitor = None
last_error = None


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
            [str(CAFFEINATE), "-i", "-w", str(os.getpid())],
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


def hook_result(notification=None):
    notifications = [] if notification is None else [notification]
    return {
        "disposition": {"action": "continue"},
        "context": [],
        "notifications": notifications,
    }


@ext.hook("before_prompt")
def before_prompt(payload):
    if start_inhibitor():
        return hook_result()
    return hook_result(
        {
            "level": "warning",
            "title": "Caffeinate unavailable",
            "message": last_error or "the sleep inhibitor could not be started",
        }
    )


@ext.hook("after_response")
def after_response(payload):
    stop_inhibitor()
    return hook_result()


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
    return {"text": "Caffeinate is idle; it runs while Ygg processes a prompt."}


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
    stop_inhibitor()


if __name__ == "__main__":
    try:
        ext.run()
    finally:
        stop_inhibitor()
