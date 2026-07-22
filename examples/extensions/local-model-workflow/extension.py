#!/usr/bin/env python3
"""Inspectable local-model workflow contributions for Ygg."""

import json
from pathlib import Path
import sys


API_VERSION = "0.1"
notification_sent = False


def send(message):
    print(json.dumps(message, separators=(",", ":")), flush=True)


def result(request_id, value):
    send({"jsonrpc": "2.0", "id": request_id, "result": value})


def rpc_error(request_id, code, message):
    send(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": code, "message": message},
        }
    )


def host_state(params):
    context = params.get("context") or {}
    host = context.get("host") or {}
    workspace = context.get("workspace")
    return host, workspace


def model_label(host):
    model = host.get("model") or "local model"
    return model.rsplit("/", 1)[-1]


def active_skill_names(host):
    names = []
    for skill in host.get("active_skills") or []:
        name = skill.get("name") or skill.get("id")
        if name:
            names.append(str(name))
    return names


def workflow_context(host, workspace):
    model = model_label(host)
    skills = active_skill_names(host)
    workspace_name = Path(workspace).name if workspace else "workspace"
    skill_text = ", ".join(skills) if skills else "none"
    return (
        f"Local-model workflow is active for {model} in {workspace_name}. "
        "Keep plans and tool output compact, inspect before editing, and ask for "
        f"clarification when ambiguity would cause broad changes. Active skills: {skill_text}."
    )


def initialize(request_id):
    result(
        request_id,
        {"api_version": API_VERSION, "tools": [], "commands": []},
    )


def before_prompt(request_id, params):
    global notification_sent
    if params.get("hook") != "before_prompt":
        rpc_error(request_id, -32602, "this extension only implements before_prompt")
        return
    host, workspace = host_state(params)
    if not notification_sent:
        send(
            {
                "jsonrpc": "2.0",
                "method": "notification",
                "params": {
                    "level": "info",
                    "title": "Local workflow active",
                    "message": f"Prompt shaping is enabled for {model_label(host)}.",
                },
            }
        )
        notification_sent = True
    result(
        request_id,
        {
            "disposition": {"action": "continue"},
            "context": [
                {
                    "label": "local-model-workflow",
                    "content": workflow_context(host, workspace),
                    "placement": "system_suffix",
                }
            ],
            "notifications": [],
        },
    )


def collect_context(request_id, params):
    host, workspace = host_state(params)
    result(
        request_id,
        [
            {
                "label": "local-model-workflow",
                "content": workflow_context(host, workspace),
                "placement": "system_suffix",
            }
        ],
    )


def collect_status(request_id, params):
    if params.get("surface") != "status":
        rpc_error(request_id, -32602, "only the status surface is declared")
        return
    host, _ = host_state(params)
    model = model_label(host)
    skill_count = len(active_skill_names(host))
    suffix = "skill" if skill_count == 1 else "skills"
    result(
        request_id,
        {
            "surface": "status",
            "text": f"local · {model} · {skill_count} {suffix}",
            "style_role": "extension.local_model_workflow.status",
            "priority": 20,
        },
    )


def handle(request):
    request_id = request.get("id")
    method = request.get("method")
    params = request.get("params") or {}
    if method == "initialize":
        initialize(request_id)
    elif method == "hook/run":
        before_prompt(request_id, params)
    elif method == "context/collect":
        collect_context(request_id, params)
    elif method == "status/collect":
        collect_status(request_id, params)
    elif method == "shutdown":
        result(request_id, {})
        return False
    else:
        rpc_error(request_id, -32601, f"unknown method: {method}")
    return True


for line in sys.stdin:
    try:
        request = json.loads(line)
        if not handle(request):
            break
    except Exception as error:  # Protocol diagnostics must never use stdout.
        print(
            f"local-model-workflow extension error: {error}",
            file=sys.stderr,
            flush=True,
        )
