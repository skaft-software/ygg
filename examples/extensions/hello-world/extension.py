#!/usr/bin/env python3
"""Minimal dependency-free Ygg executable extension using the Python SDK."""

from ygg_extension import Extension


ext = Extension()


@ext.tool(
    name="hello_world",
    description="Greet someone from an executable extension",
    parameters={
        "type": "object",
        "properties": {"name": {"type": "string"}},
        "required": ["name"],
        "additionalProperties": False,
    },
)
def hello(args):
    name = args.get("name", "tinkerer")
    ext.notify(f"hello_world greeted {name}", level="success")
    return {"content": f"Hello, {name}!"}


@ext.command(
    name="hello",
    description="Show a greeting notification",
    usage="/hello [name]",
)
def hello_command(arguments):
    name = arguments[0] if arguments else "tinkerer"
    return {"text": f"Hello, {name}!"}


@ext.hook("before_prompt")
def before_prompt(payload):
    return {"disposition": {"action": "continue"}, "context": [], "notifications": []}


@ext.hook("after_response")
def after_response(payload):
    return {"disposition": {"action": "continue"}, "context": [], "notifications": []}


@ext.context
def collect_context(params):
    return [
        {
            "label": "hello-world",
            "content": "The hello-world extension is active.",
            "placement": "system_suffix",
        }
    ]


@ext.status("status")
def collect_status(params):
    return {
        "surface": params.get("surface", "status"),
        "text": "hello",
        "style_role": "extension.hello_world.status",
        "priority": 0,
    }


@ext.renderer("hello_world")
def render_tool(params):
    output = params.get("output") or "waiting"
    return {
        "segments": [
            {
                "text": f"hello_world · {output}",
                "style_role": "extension.hello_world.tool",
            }
        ]
    }


if __name__ == "__main__":
    ext.run()
