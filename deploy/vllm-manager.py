#!/usr/bin/env python3
"""Host-side single-GPU vLLM supervisor for hlid.

vLLM serves one base model per process. This service keeps a stable OpenAI
-compatible endpoint while serializing requests, lazily starting the requested
model, switching processes between models, and stopping vLLM after inactivity.
It intentionally uses only Python's standard library so it can run outside the
vLLM Docker container and invoke docker without mounting the Docker socket into
hlid.
"""
from __future__ import annotations

import http.client
import json
import os
import shlex
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

CONFIG = Path(
    os.environ.get(
        "VLLM_MANAGER_CONFIG",
        str(Path.home() / ".config" / "ygg" / "vllm-manager.json"),
    )
)


def load_config() -> dict[str, Any]:
    with CONFIG.open() as f:
        config = json.load(f)
    config.setdefault("bind_addr", "127.0.0.1:8001")
    config.setdefault("vllm_url", "http://127.0.0.1:8000")
    config.setdefault("docker_container", "ygg-vllm")
    config.setdefault("idle_timeout_secs", 900)
    config.setdefault("startup_timeout_secs", 900)
    models = config.get("models", [])
    if not models:
        raise ValueError("manager config has no models")
    config["models_by_id"] = {model["id"]: model for model in models}
    return config


class VllmManager:
    def __init__(self, config: dict[str, Any]):
        self.config = config
        self.models = config["models_by_id"]
        self.vllm_url = urlsplit(config["vllm_url"])
        self.lock = threading.Lock()
        self.last_activity = time.monotonic()
        self.active_model: str | None = None

    def _vllm_get(self, path: str) -> tuple[int, bytes]:
        connection = http.client.HTTPConnection(self.vllm_url.hostname, self.vllm_url.port or 80, timeout=3)
        try:
            connection.request("GET", path)
            response = connection.getresponse()
            return response.status, response.read()
        finally:
            connection.close()

    def ready_model(self) -> str | None:
        try:
            status, body = self._vllm_get("/v1/models")
            if status != 200:
                return None
            models = json.loads(body).get("data", [])
            ids = [model.get("id") for model in models]
            for model in self.models.values():
                if model["served_model_name"] in ids:
                    return model["id"]
        except (OSError, ValueError, TypeError):
            pass
        return None

    def stop(self) -> None:
        subprocess.run(
            ["docker", "exec", self.config["docker_container"], "bash", "-lc",
             "pkill -TERM -f '[v]llm serve' || true"],
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if self.ready_model() is None:
                self.active_model = None
                return
            time.sleep(0.25)
        raise RuntimeError("vLLM did not stop within 20 seconds")

    def start(self, model: dict[str, Any]) -> None:
        args = model.get("launch_args", "")
        if not args:
            args = (
                "--dtype float16 --tensor-parallel-size 1 --gpu-memory-util 0.90 "
                "--max-num-batched-tokens 2048 --block-size 64 --enforce-eager "
                "--trust-remote-code --reasoning-parser qwen3 "
                "--enable-auto-tool-choice --tool-call-parser qwen3_coder"
            )
        log_name = model["id"].replace("/", "-")
        command = (
            "source /opt/intel/oneapi/setvars.sh --force >/dev/null 2>&1; "
            "export ZE_AFFINITY_MASK=0 VLLM_ALLOW_LONG_MAX_MODEL_LEN=1 "
            "VLLM_WORKER_MULTIPROC_METHOD=spawn VLLM_OFFLOAD_WEIGHTS_BEFORE_QUANT=1 "
            "VLLM_QUANTIZE_Q40_LIB=/opt/venv/lib/python3.12/site-packages/"
            "vllm_int4_for_multi_arc.so; "
            f"exec vllm serve {shlex.quote(model['path'])} "
            f"--served-model-name {shlex.quote(model['served_model_name'])} "
            f"--host 0.0.0.0 --port 8000 --max-model-len {int(model['context_window'])} "
            f"{args} 2>&1 | tee /llm/logs/manager-{shlex.quote(log_name)}.log"
        )
        subprocess.run(
            ["docker", "exec", "-d", self.config["docker_container"], "bash", "-lc", command],
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
        deadline = time.monotonic() + self.config["startup_timeout_secs"]
        while time.monotonic() < deadline:
            if self.ready_model() == model["id"]:
                self.active_model = model["id"]
                return
            time.sleep(1)
        raise RuntimeError(f"vLLM did not load {model['id']} before the startup timeout")

    def ensure_model(self, model_id: str) -> None:
        model = self.models.get(model_id)
        if model is None:
            raise ValueError(f"unknown model {model_id!r}")
        current = self.ready_model()
        if current == model_id:
            self.active_model = model_id
            return
        if current is not None:
            self.stop()
        self.start(model)

    def reap(self) -> None:
        while True:
            time.sleep(30)
            if not self.lock.acquire(blocking=False):
                continue
            try:
                if (self.ready_model() is not None and
                        time.monotonic() - self.last_activity >= self.config["idle_timeout_secs"]):
                    print("idle timeout reached; stopping vLLM", flush=True)
                    try:
                        self.stop()
                    except Exception as exc:  # keep the supervisor alive
                        print(f"idle stop failed: {exc}", flush=True)
            finally:
                self.lock.release()

    def models_response(self) -> dict[str, Any]:
        data = []
        for model in self.models.values():
            modalities = ["text", "image"] if model.get("vision", False) else ["text"]
            data.append({
                "id": model["id"],
                "object": "model",
                "owned_by": "local",
                "root": model["path"],
                "max_model_len": model["context_window"],
                "permission": [],
                "architecture": {"input_modalities": modalities},
                # ygg's existing discovery uses this to recover context size.
                "status": {"args": ["--max-model-len", str(model["context_window"])]},
            })
        return {"object": "list", "data": data}


class Handler(BaseHTTPRequestHandler):
    manager: VllmManager

    def log_message(self, fmt: str, *args: Any) -> None:
        print(f"{self.address_string()} - {fmt % args}", flush=True)

    def send_json(self, status: int, body: dict[str, Any]) -> None:
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self) -> None:
        if self.path == "/v1/models":
            self.send_json(200, self.manager.models_response())
        elif self.path == "/health":
            # The manager stays healthy while vLLM is intentionally asleep.
            self.send_json(200, {"status": "ok", "active_model": self.manager.ready_model()})
        else:
            self.send_json(404, {"error": {"message": "not found"}})

    def do_POST(self) -> None:
        if self.path not in ("/v1/chat/completions", "/v1/responses", "/v1/completions", "/v1/messages"):
            self.send_json(404, {"error": {"message": "not found"}})
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if length > 10 * 1024 * 1024:
                raise ValueError("request body exceeds 10 MiB")
            body = self.rfile.read(length)
            request = json.loads(body)
            model_id = request.get("model", "")
            with self.manager.lock:
                self.manager.ensure_model(model_id)
                self.manager.last_activity = time.monotonic()
                self.proxy(body)
        except ValueError as exc:
            self.send_json(400, {"error": {"message": str(exc), "type": "invalid_request_error"}})
        except Exception as exc:
            self.send_json(503, {"error": {"message": str(exc), "type": "server_error"}})

    def proxy(self, body: bytes) -> None:
        connection = http.client.HTTPConnection(
            self.manager.vllm_url.hostname, self.manager.vllm_url.port or 80,
            timeout=self.manager.config.get("request_timeout_secs", 900),
        )
        try:
            forwarded = {"Content-Type": self.headers.get("Content-Type", "application/json")}
            connection.request("POST", self.path, body=body, headers=forwarded)
            response = connection.getresponse()
            self.send_response(response.status)
            for name in ("Content-Type", "Cache-Control", "X-Request-Id"):
                value = response.getheader(name)
                if value:
                    self.send_header(name, value)
            self.end_headers()
            while True:
                chunk = response.read(64 * 1024)
                if not chunk:
                    break
                self.wfile.write(chunk)
                self.wfile.flush()
        finally:
            connection.close()


def main() -> None:
    config = load_config()
    host, port = config["bind_addr"].rsplit(":", 1)
    manager = VllmManager(config)
    Handler.manager = manager
    threading.Thread(target=manager.reap, daemon=True, name="idle-reaper").start()
    server = ThreadingHTTPServer((host, int(port)), Handler)
    print(f"vLLM manager listening on {host}:{port}; {len(manager.models)} models; idle={config['idle_timeout_secs']}s", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
