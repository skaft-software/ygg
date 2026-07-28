#!/usr/bin/env python3
"""Inspectable local-model workflow contributions for Ygg."""

from pathlib import Path

from ygg_extension import Extension


ext = Extension()
notification_sent = False


def host_state(context):
    context = context or {}
    return context.get("host") or {}, context.get("workspace")


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


@ext.hook("before_prompt")
def before_prompt(payload, context):
    global notification_sent
    if not notification_sent:
        host, _ = host_state(context)
        ext.notify(
            f"Prompt shaping is enabled for {model_label(host)}.",
            level="info",
            title="Local workflow active",
        )
        notification_sent = True
    host, workspace = host_state(context)
    return {
        "disposition": {"action": "continue"},
        "context": [
            {
                "label": "local-model-workflow",
                "content": workflow_context(host, workspace),
                "placement": "system_suffix",
            }
        ],
        "notifications": [],
    }


@ext.context
def collect_context(params):
    host, workspace = host_state(params.get("context"))
    return [
        {
            "label": "local-model-workflow",
            "content": workflow_context(host, workspace),
            "placement": "system_suffix",
        }
    ]


@ext.status("status")
def collect_status(params):
    host, _ = host_state(params.get("context"))
    model = model_label(host)
    skill_count = len(active_skill_names(host))
    suffix = "skill" if skill_count == 1 else "skills"
    return {
        "surface": params.get("surface", "status"),
        "text": f"local · {model} · {skill_count} {suffix}",
        "style_role": "extension.local_model_workflow.status",
        "priority": 20,
    }


if __name__ == "__main__":
    ext.run()
