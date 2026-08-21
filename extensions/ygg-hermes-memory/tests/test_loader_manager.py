from __future__ import annotations

from contextlib import contextmanager
from dataclasses import replace
import json
import os
from pathlib import Path
import py_compile
import shutil
import sys
import threading
import time
import unittest
from unittest import mock

from .helpers import (
    FakeExtension,
    load_fixture_config,
    mock_descriptor,
    offline_descriptor,
    owner_context,
    temporary_directory,
    wait_until,
    write_config,
)
from ygg_hermes_memory.discovery import discover_providers
from ygg_hermes_memory import loader as loader_module
from ygg_hermes_memory.loader import (
    ProviderLoadError,
    _ProviderCollector,
    _extract_provider,
    load_selected_provider,
)
from ygg_hermes_memory.manager import MemoryBridge, ProviderGenerationFenced


@contextmanager
def fixture_mode(value: str):
    previous = os.environ.get("YGG_MEMORY_FIXTURE_MODE")
    os.environ["YGG_MEMORY_FIXTURE_MODE"] = value
    try:
        yield
    finally:
        if previous is None:
            os.environ.pop("YGG_MEMORY_FIXTURE_MODE", None)
        else:
            os.environ["YGG_MEMORY_FIXTURE_MODE"] = previous


def activate_mock(directory: Path, *, limits=None):
    config = load_fixture_config(
        directory,
        providers=[mock_descriptor()],
        limits=limits or {},
    )
    extension = FakeExtension()
    bridge = MemoryBridge(extension, config)
    bridge.start({"host": {"session_id": "session-a"}})
    context = owner_context("session-a")
    candidate = bridge._discovery.by_id("directory:mock")
    bridge.execute_command(["trust", candidate.id, candidate.fingerprint], context)
    response = bridge.execute_command(["select", candidate.id], context)
    return bridge, extension, context, candidate, response


class LoaderAndManagerTests(unittest.TestCase):
    def test_directory_loader_requires_exact_trust_and_ignores_secondary_registration(self):
        with temporary_directory() as directory:
            config = load_fixture_config(directory, providers=[mock_descriptor()])
            candidate = discover_providers(config).by_id("directory:mock")
            with self.assertRaisesRegex(ProviderLoadError, "provider_not_trusted"):
                load_selected_provider(candidate, config, expected_fingerprint="0" * 64)
            loaded = load_selected_provider(
                candidate, config, expected_fingerprint=candidate.fingerprint
            )
        self.assertEqual(loaded.provider.name, "mock-memory")
        self.assertIn("prefetch", loaded.optional_hooks)
        self.assertIn("register_skill", loaded.ignored_registrations)
        self.assertIn("on_delegation", loaded.unsupported_hooks)

    def test_directory_loader_imports_the_verified_snapshot_not_changed_source(self):
        with temporary_directory() as directory:
            source = Path(mock_descriptor()["path"])
            copied = directory / "provider"
            shutil.copytree(source, copied, ignore=shutil.ignore_patterns("__pycache__"))
            descriptor = dict(mock_descriptor(), path=str(copied), id="snapshot-race")
            config = load_fixture_config(directory, providers=[descriptor])
            candidate = discover_providers(config).by_id("directory:snapshot-race")
            original_snapshot = loader_module.directory_snapshot
            sentinel = directory / "changed-source-executed"

            def snapshot_then_replace(*args, **kwargs):
                result = original_snapshot(*args, **kwargs)
                (copied / "__init__.py").write_text(
                    f"from pathlib import Path\nPath({str(sentinel)!r}).write_text('executed')\n",
                    encoding="utf-8",
                )
                return result

            with mock.patch.object(
                loader_module, "directory_snapshot", side_effect=snapshot_then_replace
            ):
                loaded = load_selected_provider(
                    candidate, config, expected_fingerprint=candidate.fingerprint
                )
            self.assertEqual(loaded.provider.name, "mock-memory")
            self.assertFalse(sentinel.exists())

    def test_unchecked_bytecode_cannot_bypass_the_trusted_source_fingerprint(self):
        with temporary_directory() as directory:
            source = Path(mock_descriptor()["path"])
            copied = directory / "provider"
            shutil.copytree(source, copied, ignore=shutil.ignore_patterns("__pycache__"))
            init_file = copied / "__init__.py"
            original = init_file.read_text(encoding="utf-8")
            sentinel = directory / "unchecked-bytecode-executed"
            init_file.write_text(
                f"from pathlib import Path\nPath({str(sentinel)!r}).write_text('executed')\n",
                encoding="utf-8",
            )
            cache = copied / "__pycache__" / "__init__.pyc"
            cache.parent.mkdir()
            py_compile.compile(
                str(init_file),
                cfile=str(cache),
                doraise=True,
                invalidation_mode=py_compile.PycInvalidationMode.UNCHECKED_HASH,
            )
            init_file.write_text(original, encoding="utf-8")
            descriptor = dict(mock_descriptor(), path=str(copied), id="unchecked-bytecode")
            config = load_fixture_config(directory, providers=[descriptor])
            candidate = discover_providers(config).by_id("directory:unchecked-bytecode")

            loaded = load_selected_provider(
                candidate, config, expected_fingerprint=candidate.fingerprint
            )
            self.assertEqual(loaded.provider.name, "mock-memory")
            self.assertFalse(sentinel.exists())

    def test_entry_point_loader_is_selected_only_and_exposes_tool(self):
        sys.modules.pop("offline_entrypoint", None)
        with temporary_directory() as directory:
            config = load_fixture_config(directory, providers=[])
            candidate = discover_providers(config).by_id("entrypoint:entrypoint-memory")
            self.assertNotIn("offline_entrypoint", sys.modules)
            loaded = load_selected_provider(
                candidate, config, expected_fingerprint=candidate.fingerprint
            )
        self.assertIn("offline_entrypoint", sys.modules)
        self.assertEqual(loaded.provider.name, "entrypoint-memory")
        self.assertEqual(loaded.provider.get_tool_schemas()[0]["name"], "entrypoint_recall")
        with self.assertRaisesRegex(ProviderLoadError, "instance_entry_point_unsupported"):
            _extract_provider(
                loaded.provider,
                loaded.memory_provider_class,
                _ProviderCollector(),
            )

    def test_contract_version_provider_import_and_initialize_failures_are_bounded(self):
        class WrongVersion:
            @staticmethod
            def version(name):
                return "0.20.0"

        with temporary_directory() as directory:
            config = load_fixture_config(directory, providers=[mock_descriptor()])
            candidate = discover_providers(config).by_id("directory:mock")
            with self.assertRaisesRegex(ProviderLoadError, "version_mismatch"):
                load_selected_provider(
                    candidate,
                    config,
                    expected_fingerprint=candidate.fingerprint,
                    metadata_module=WrongVersion,
                )

            broken = directory / "broken-provider"
            broken.mkdir()
            (broken / "plugin.yaml").write_text("name: broken-memory\nversion: 1.0.0\n")
            (broken / "__init__.py").write_text("this is not valid python !!!\n")
            descriptor = dict(mock_descriptor(), id="broken", path=str(broken), label="Broken")
            broken_config = load_fixture_config(directory, providers=[descriptor])
            broken_candidate = discover_providers(broken_config).by_id("directory:broken")
            with self.assertRaisesRegex(ProviderLoadError, "import_failed"):
                load_selected_provider(
                    broken_candidate,
                    broken_config,
                    expected_fingerprint=broken_candidate.fingerprint,
                )

        with fixture_mode("fail-initialize"):
            with temporary_directory() as directory:
                bridge, extension, context, _, response = activate_mock(directory)
                self.assertIn("unavailable", response["text"])
                owner = bridge.owner_for_context(context)
                self.assertEqual(owner.state, "unavailable")
                self.assertNotIn("provider-secret", json.dumps(bridge.presentation_snapshot(owner)))
                self.assertEqual(extension.tools, {})
                bridge.shutdown()

    def test_trust_does_not_import_and_selection_publishes_dynamic_tools(self):
        with temporary_directory() as directory:
            sentinel = directory / "sentinel"
            # Clear the synthetic package so this test observes the import edge.
            for name in list(sys.modules):
                if name.startswith("_ygg_hermes_provider_"):
                    sys.modules.pop(name, None)
            previous = os.environ.get("YGG_MEMORY_IMPORT_SENTINEL")
            os.environ["YGG_MEMORY_IMPORT_SENTINEL"] = str(sentinel)
            try:
                config = load_fixture_config(directory, providers=[mock_descriptor()])
                extension = FakeExtension()
                bridge = MemoryBridge(extension, config)
                bridge.start({"host": {"session_id": "session-a"}})
                context = owner_context()
                candidate = bridge._discovery.by_id("directory:mock")
                trust = bridge.execute_command(["trust", candidate.id, candidate.fingerprint], context)
                self.assertFalse(sentinel.exists())
                self.assertIn("not been imported", trust["text"])
                bridge.execute_command(["select", candidate.id], context)
                self.assertTrue(sentinel.exists())
                self.assertEqual(set(extension.tools), {"recall_mock", "remember_mock"})
                self.assertTrue(any(item[0] == "register" for item in extension.mutations))
            finally:
                bridge.shutdown()
                if previous is None:
                    os.environ.pop("YGG_MEMORY_IMPORT_SENTINEL", None)
                else:
                    os.environ["YGG_MEMORY_IMPORT_SENTINEL"] = previous

    def test_selection_is_one_provider_instance_per_owner(self):
        with temporary_directory() as directory:
            bridge, extension, first_context, candidate, _ = activate_mock(directory)
            second_context = owner_context("session-b")
            bridge.execute_command(["select", candidate.id], second_context)
            first = bridge.owner_for_context(first_context)
            second = bridge.owner_for_context(second_context)
            self.assertIsNot(first, second)
            self.assertIsNot(first.provider.provider, second.provider.provider)
            bridge.before_prompt({"prompt": "first private prompt"}, first_context)
            bridge.collect_context({"prompt": "first private prompt"}, first_context)
            bridge.before_prompt({"prompt": "second isolated prompt"}, second_context)
            bridge.collect_context({"prompt": "second isolated prompt"}, second_context)
            first_queries = [x["query"] for x in first.provider.provider.events if x["event"] == "prefetch"]
            second_queries = [x["query"] for x in second.provider.provider.events if x["event"] == "prefetch"]
            self.assertEqual(first_queries, ["first private prompt"])
            self.assertEqual(second_queries, ["second isolated prompt"])
            bridge.shutdown()

    def test_context_is_fenced_redacted_bounded_and_frozen_per_prompt_epoch(self):
        with fixture_mode("injected-memory"):
            with temporary_directory() as directory:
                bridge, _, context, _, _ = activate_mock(
                    directory, limits={"maxContextBytes": 2048}
                )
                bridge.before_prompt({"prompt": "remember project"}, context)
                first = bridge.collect_context({"prompt": "remember project"}, context)
                # A prompt-composition retry replays neither provider prefetch
                # nor on_turn_start for the still-open prompt epoch.
                bridge.before_prompt({"prompt": "remember project"}, context)
                second = bridge.collect_context({"prompt": "remember project"}, context)
                text = "\n".join(item["content"] for item in first)
                self.assertEqual(first, second)
                self.assertIn("YGG_UNTRUSTED_MEMORY_BEGIN", text)
                self.assertIn("[provider marker removed]", text)
                self.assertNotIn("sk-abcdefghijklmnop", text)
                self.assertNotIn("static-secret", text)
                self.assertLessEqual(sum(len(item["content"].encode("utf-8")) for item in first), 2048)
                owner = bridge.owner_for_context(context)
                prefetches = [event for event in owner.provider.provider.events if event["event"] == "prefetch"]
                starts = [event for event in owner.provider.provider.events if event["event"] == "on_turn_start"]
                self.assertEqual(len(prefetches), 1)
                self.assertEqual(len(starts), 1)
                self.assertEqual(owner.last_prefetch["cache"], "hit")
                presentation = bridge.presentation_snapshot(owner)
                serialized = json.dumps(presentation)
                self.assertNotIn("IGNORE ALL", serialized)
                self.assertNotIn("Useful remembered fact", serialized)
                bridge.shutdown()

    def test_oversized_and_failed_prefetch_degrade_while_slow_prefetch_fences(self):
        with fixture_mode("oversized-memory"):
            with temporary_directory() as directory:
                bridge, _, context, _, _ = activate_mock(
                    directory, limits={"maxContextBytes": 1024}
                )
                bridge.before_prompt({"prompt": "large"}, context)
                result = bridge.collect_context({"prompt": "large"}, context)
                self.assertLessEqual(sum(len(x["content"].encode()) for x in result), 1024)
                self.assertTrue(bridge.owner_for_context(context).last_prefetch["truncated"])
                bridge.shutdown()
        with fixture_mode("slow-prefetch"):
            with temporary_directory() as directory:
                bridge, _, context, _, _ = activate_mock(
                    directory,
                    limits={"prefetchTimeoutMs": 30, "maxContextBytes": 1024},
                )
                bridge.before_prompt({"prompt": "slow"}, context)
                aborted = []
                bridge._abort_generation = aborted.append
                started = time.monotonic()
                with self.assertRaises(ProviderGenerationFenced):
                    bridge.collect_context({"prompt": "slow"}, context)
                self.assertLess(time.monotonic() - started, 0.4)
                self.assertEqual(aborted, ["unfinished_provider_prefetch_timeout"])
                self.assertNotEqual(bridge.owner_for_context(context).state, "stopped")
                bridge.shutdown()
        with fixture_mode("fail-prefetch"):
            with temporary_directory() as directory:
                bridge, _, context, _, _ = activate_mock(directory)
                bridge.before_prompt({"prompt": "failure"}, context)
                result = bridge.collect_context({"prompt": "failure"}, context)
                owner = bridge.owner_for_context(context)
                self.assertTrue(result)  # static activation context survives
                self.assertEqual(owner.last_prefetch["outcome"], "degraded")
                self.assertEqual(owner.activities[-1].state, "degraded")
                self.assertNotIn("should-never-reach-ui", json.dumps(bridge.presentation_snapshot(owner)))
                bridge.shutdown()

    def test_unavailable_and_malformed_schema_providers_fail_soft(self):
        with fixture_mode("unavailable"):
            with temporary_directory() as directory:
                bridge, extension, context, _, response = activate_mock(directory)
                owner = bridge.owner_for_context(context)
                self.assertIn("unavailable", response["text"])
                self.assertEqual(owner.state, "unavailable")
                self.assertIn("[redacted]", owner.setup_hint)
                self.assertNotIn("do-not-show", owner.setup_hint)
                self.assertNotIn("/home/alice", owner.setup_hint)
                self.assertEqual(extension.tools, {})
                bridge.shutdown()
        with fixture_mode("malformed-schema"):
            with temporary_directory() as directory:
                bridge, extension, context, _, _ = activate_mock(directory)
                owner = bridge.owner_for_context(context)
                self.assertEqual(owner.state, "degraded")
                self.assertEqual(owner.last_error_code, "malformed_tool_schema")
                self.assertEqual(extension.tools, {})
                bridge.shutdown()

    def test_host_filtered_dynamic_catalog_is_authoritative(self):
        class FilteringExtension(FakeExtension):
            def register_tools(self, definitions):
                accepted = [item for item in definitions if item["name"] == "recall_mock"]
                return super().register_tools(accepted)

        with temporary_directory() as directory:
            config = load_fixture_config(directory, providers=[mock_descriptor()])
            extension = FilteringExtension()
            bridge = MemoryBridge(extension, config)
            bridge.start({"host": {"session_id": "session-a"}})
            context = owner_context()
            candidate = bridge._discovery.by_id("directory:mock")
            bridge.execute_command(["trust", candidate.id, candidate.fingerprint], context)
            bridge.execute_command(["select", candidate.id], context)
            owner = bridge.owner_for_context(context)
            self.assertEqual(owner.published_tool_names, ("recall_mock",))
            self.assertEqual(set(extension.tools), {"recall_mock"})
            self.assertEqual(bridge._owner_view(owner)["toolCount"], 1)
            bridge.shutdown()

    def test_tool_reads_writes_malformed_results_and_durability_provenance(self):
        with temporary_directory() as directory:
            bridge, extension, context, _, _ = activate_mock(directory)
            write_handler = extension.tools["remember_mock"]["handler"]
            result = write_handler({"content": "remember this"}, context)
            self.assertFalse(result.get("is_error", False))
            self.assertEqual(result["metadata"]["durability"], "committed")
            owner = bridge.owner_for_context(context)
            activity = owner.activities[-1]
            self.assertEqual(activity.kind, "memory_write")
            self.assertEqual(activity.state, "succeeded")
            self.assertIn("committed", activity.provenance)
            read = extension.tools["recall_mock"]["handler"]({"query": "x"}, context)
            self.assertIn("YGG_UNTRUSTED_MEMORY_BEGIN", read["content"][0]["text"])
            bridge.shutdown()
        with fixture_mode("malformed-result"):
            with temporary_directory() as directory:
                bridge, extension, context, _, _ = activate_mock(directory)
                result = extension.tools["recall_mock"]["handler"]({"query": "x"}, context)
                self.assertTrue(result["is_error"])
                self.assertIn("malformed", result["content"][0]["text"])
                self.assertEqual(bridge.owner_for_context(context).activities[-1].state, "failed")
                bridge.shutdown()

    def test_switch_and_reload_shutdown_prior_instance_and_replace_catalog(self):
        with temporary_directory() as directory:
            config = load_fixture_config(
                directory, providers=[mock_descriptor(), offline_descriptor()]
            )
            extension = FakeExtension()
            bridge = MemoryBridge(extension, config)
            bridge.start({"host": {"session_id": "session-a"}})
            context = owner_context()
            mock = bridge._discovery.by_id("directory:mock")
            offline = bridge._discovery.by_id("directory:offline")
            for candidate in (mock, offline):
                bridge.execute_command(["trust", candidate.id, candidate.fingerprint], context)
            bridge.execute_command(["select", mock.id], context)
            owner = bridge.owner_for_context(context)
            old_mock = owner.provider.provider
            bridge.execute_command(["select", offline.id], context)
            self.assertTrue(old_mock.closed)
            self.assertEqual(set(extension.tools), {"recall_offline", "remember_offline"})
            first_offline = owner.provider.provider
            bridge.execute_command(["reload"], context)
            self.assertTrue(first_offline._closed)
            self.assertIsNot(first_offline, owner.provider.provider)
            self.assertTrue(any("switched" in item.summary.lower() for item in owner.activities))
            self.assertTrue(any("reloaded" in item.summary.lower() for item in owner.activities))
            bridge.shutdown()

    def test_stale_generation_retires_provider_and_tools_without_crossing_fence(self):
        with temporary_directory() as directory:
            bridge, extension, context, _, _ = activate_mock(directory)
            old_owner = bridge.owner_for_context(context)
            old_provider = old_owner.provider.provider
            new_context = owner_context("session-a", generation=2)
            new_owner = bridge.owner_for_context(new_context)
            self.assertIsNot(old_owner, new_owner)
            self.assertIsNone(new_owner.provider)
            self.assertEqual(old_owner.state, "stopped")
            self.assertEqual(extension.tools, {})
            self.assertTrue(wait_until(lambda: old_provider.closed))
            # Focusing the unselected new generation never reuses old handles.
            self.assertEqual(bridge.collect_context({"prompt": "x"}, new_context), [])
            bridge.shutdown()

    def test_retirement_control_thread_start_failure_fences_generation(self):
        with temporary_directory() as directory:
            bridge, _, context, _, _ = activate_mock(directory)
            old_provider = bridge.owner_for_context(context).provider.provider
            aborted = []
            bridge._abort_generation = aborted.append
            with mock.patch(
                "ygg_hermes_memory.manager.threading.Thread.start",
                side_effect=RuntimeError("thread unavailable"),
            ):
                with self.assertRaises(ProviderGenerationFenced):
                    bridge.owner_for_context(owner_context("session-a", generation=2))
            self.assertTrue(bridge._generation_fenced.is_set())
            self.assertFalse(old_provider.closed)
            self.assertEqual(len(aborted), 1)
            self.assertTrue(aborted[0].endswith("_thread_start_failed"))
            bridge.shutdown()

    def test_uncooperative_stale_provider_retirement_fences_generation(self):
        with fixture_mode("slow-shutdown"):
            with temporary_directory() as directory:
                bridge, _, context, _, _ = activate_mock(
                    directory, limits={"shutdownTimeoutMs": 30}
                )
                aborted = []
                bridge._abort_generation = aborted.append
                bridge.owner_for_context(owner_context("session-a", generation=2))
                self.assertTrue(wait_until(bridge._generation_fenced.is_set))
                self.assertEqual(
                    aborted,
                    ["unfinished_provider_retire_provider_timeout"],
                )
                bridge.shutdown()

    def test_required_provider_call_thread_start_failure_fences_generation(self):
        with temporary_directory() as directory:
            bridge, _, context, _, _ = activate_mock(directory)
            owner = bridge.owner_for_context(context)
            provider = owner.provider.provider
            aborted = []
            bridge._abort_generation = aborted.append
            with mock.patch(
                "ygg_hermes_memory.manager.threading.Thread.start",
                side_effect=RuntimeError("thread unavailable"),
            ):
                with self.assertRaises(ProviderGenerationFenced):
                    bridge.execute_command(["off"], context)
            self.assertFalse(provider.closed)
            self.assertEqual(
                aborted,
                ["unfinished_provider_shutdown_provider_thread_runtimeerror"],
            )
            bridge.shutdown()

    def test_uncooperative_cancellation_fences_the_provider_generation(self):
        class Token:
            cancelled = False

        with temporary_directory() as directory:
            bridge, _, context, _, _ = activate_mock(directory)
            owner = bridge.owner_for_context(context)
            aborted = []
            bridge._abort_generation = aborted.append
            token = Token()
            timer = threading.Timer(0.03, lambda: setattr(token, "cancelled", True))
            timer.start()
            started = time.monotonic()
            with self.assertRaises(ProviderGenerationFenced):
                bridge._call_bounded(
                    owner, "test_cancel", lambda: time.sleep(1), 1000, cancellation=token
                )
            self.assertLess(time.monotonic() - started, 0.3)
            self.assertEqual(aborted, ["unfinished_provider_test_cancel_cancelled"])
            self.assertTrue(bridge._generation_fenced.is_set())
            bridge.shutdown()

    def test_uncooperative_tool_timeout_fences_without_false_write_settlement(self):
        with fixture_mode("slow-tool"):
            with temporary_directory() as directory:
                bridge, extension, context, _, _ = activate_mock(
                    directory, limits={"toolTimeoutMs": 30}
                )
                aborted = []
                bridge._abort_generation = aborted.append
                owner = bridge.owner_for_context(context)
                with self.assertRaises(ProviderGenerationFenced):
                    extension.tools["remember_mock"]["handler"](
                        {"content": "ambiguous write"}, context
                    )
                self.assertEqual(
                    aborted,
                    ["unfinished_provider_tool_remember_mock_timeout"],
                )
                self.assertEqual(owner.activities[-1].state, "running")
                self.assertIsNone(owner.activities[-1].completed_at_ms)
                bridge.shutdown()

    def test_slow_trusted_default_activation_never_blocks_admitting_prompt(self):
        with fixture_mode("slow-availability"):
            with temporary_directory() as directory:
                base = load_fixture_config(
                    directory,
                    providers=[mock_descriptor()],
                    limits={"availabilityTimeoutMs": 1000, "shutdownTimeoutMs": 30},
                )
                candidate = discover_providers(base).by_id("directory:mock")
                config = replace(
                    base,
                    trusted_providers={candidate.id: candidate.fingerprint},
                    default_provider=candidate.id,
                )
                bridge = MemoryBridge(FakeExtension(), config)
                aborted = []
                bridge._abort_generation = aborted.append
                bridge.start({"host": {"session_id": "display"}})
                context = dict(owner_context("durable"))
                context["host"] = {"session_id": "display", "active_skills": []}
                started = time.monotonic()
                bridge.before_prompt({"prompt": "direct coding continues"}, context)
                self.assertLess(time.monotonic() - started, 0.15)
                owner = bridge.owner_for_context(context)
                self.assertEqual(owner.state, "loading")
                self.assertEqual(owner.user_text, "direct coding continues")
                self.assertEqual(bridge.collect_context({"prompt": "direct coding continues"}, context), [])

                def availability_call_started():
                    with bridge._call_lock:
                        return any(
                            kind == "is_available"
                            for _, kind in bridge._active_call_threads.values()
                        )

                self.assertTrue(wait_until(availability_call_started))
                with self.assertRaises(ProviderGenerationFenced):
                    bridge.shutdown()
                self.assertEqual(len(aborted), 1)
                self.assertIn(
                    aborted[0],
                    {
                        "unfinished_provider_is_available_shutdown",
                        "unfinished_provider_shutdown_unfinished",
                    },
                )

    def test_realistic_offline_provider_runs_end_to_end_without_network(self):
        with temporary_directory() as directory:
            config = load_fixture_config(directory, providers=[offline_descriptor()])
            extension = FakeExtension()
            bridge = MemoryBridge(extension, config)
            bridge.start({"host": {"session_id": "offline-session"}})
            context = owner_context("offline-session")
            candidate = bridge._discovery.by_id("directory:offline")
            bridge.execute_command(["trust", candidate.id, candidate.fingerprint], context)
            bridge.execute_command(["select", candidate.id], context)
            write = extension.tools["remember_offline"]["handler"](
                {"content": "Rust ownership notes"}, context
            )
            self.assertEqual(write["metadata"]["durability"], "committed")
            bridge.before_prompt({"prompt": "ownership"}, context)
            contributions = bridge.collect_context({"prompt": "ownership"}, context)
            self.assertIn("Rust ownership notes", "\n".join(x["content"] for x in contributions))
            store = directory / "hermes-home" / "offline-recall-fixture.json"
            self.assertTrue(store.is_file())
            # The bridge never owns or copies this provider-created store.
            self.assertFalse(any("offline-recall-fixture" in str(path) for path in extension.presentations))
            bridge.shutdown()
    def test_explicit_provider_environment_is_loaded_only_after_trusted_selection_and_cleared(self):
        with temporary_directory() as directory:
            env_file = directory / "provider.env"
            env_file.write_text("FIXTURE_PROVIDER_TOKEN=top-secret-value\n", encoding="utf-8")
            os.chmod(env_file, 0o600)
            path = write_config(directory, providers=[mock_descriptor()])
            value = json.loads(path.read_text(encoding="utf-8"))
            value["environment"]["providerEnvFile"] = str(env_file)
            path.write_text(json.dumps(value), encoding="utf-8")
            os.chmod(path, 0o600)
            from ygg_hermes_memory.config import load_config

            config = load_config(path)
            extension = FakeExtension()
            bridge = MemoryBridge(extension, config)
            bridge.start({"host": {"session_id": "session-a"}})
            context = owner_context()
            candidate = bridge._discovery.by_id("directory:mock")
            self.assertNotIn("FIXTURE_PROVIDER_TOKEN", os.environ)
            bridge.execute_command(["trust", candidate.id, candidate.fingerprint], context)
            self.assertNotIn("FIXTURE_PROVIDER_TOKEN", os.environ)
            bridge.execute_command(["select", candidate.id], context)
            self.assertEqual(os.environ.get("FIXTURE_PROVIDER_TOKEN"), "top-secret-value")
            self.assertNotIn("top-secret-value", json.dumps(bridge.presentation_snapshot()))
            bridge.shutdown()
            self.assertNotIn("FIXTURE_PROVIDER_TOKEN", os.environ)


if __name__ == "__main__":
    unittest.main()
