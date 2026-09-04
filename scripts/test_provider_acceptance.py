#!/usr/bin/env python3
"""Synthetic regressions for provider-acceptance policy evidence."""

from __future__ import annotations

import copy
import importlib.util
import pathlib
import sys
import unittest

SCRIPT = pathlib.Path(__file__).with_name("provider-acceptance.py")
SPEC = importlib.util.spec_from_file_location("provider_acceptance", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("provider-acceptance test module could not be loaded")
provider_acceptance = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = provider_acceptance
SPEC.loader.exec_module(provider_acceptance)

HELLO_REQUEST_ID = "hello"
REQUEST_ID = "request"
RUN_ID = "run"


def effective_policy() -> dict[str, object]:
    def value(setting: object) -> dict[str, object]:
        return {"value": setting, "source": "host_request"}

    return {
        "effect_policy": value("controlled"),
        "workspace_confinement": value(True),
        "allow_edit": value(False),
        "allow_write": value(False),
        "allow_process": value(False),
        "allow_shell": value(False),
        "shell_path": value({"selection": "system_bash"}),
        "bash_timeout_ms": value(120_000),
        "max_output_bytes": value(51_200),
        "allow_remote_read": value(False),
    }


def exchange_events(
    run_events: list[dict[str, object]], *, audio: bool = False
) -> list[dict[str, object]]:
    features = {
        "streaming": True,
        "inline_models": True,
        "typed_media_input": True,
    }
    if audio:
        features["typed_audio_input"] = True
    hello = {
        "protocol_version": 1,
        "request_id": HELLO_REQUEST_ID,
        "seq": 1,
        "type": "hello",
        "data": {
            "max_frame_bytes": provider_acceptance.MAX_FRAME_BYTES,
            "commands": ["run"],
            "features": features,
        },
    }
    return [hello] + [
        {
            "protocol_version": 1,
            "request_id": REQUEST_ID,
            "run_id": RUN_ID,
            "seq": index,
            "type": event["type"],
            "data": event["data"],
        }
        for index, event in enumerate(run_events, start=1)
    ]


def allowed_lifecycle() -> list[dict[str, object]]:
    policy = effective_policy()
    return [
        {
            "type": "accepted",
            "data": {
                "effective_tool_policy": policy,
                "registered_tools": ["read"],
            },
        },
        {"type": "started", "data": {"model": "fixture"}},
        {
            "type": "tool_start",
            "data": {
                "toolCallId": "call-allowed",
                "toolName": "read",
                "input": {"path": "acceptance-canary.txt"},
            },
        },
        {
            "type": "tool_policy",
            "data": {
                "toolCallId": "call-allowed",
                "toolName": "read",
                "decision": {
                    "effect": "workspace_read",
                    "allowed": True,
                    "authorization": "policy",
                    "policy": copy.deepcopy(policy),
                },
            },
        },
        {
            "type": "tool_finish",
            "data": {"toolCallId": "call-allowed", "ok": True},
        },
        {"type": "settled", "data": {}},
        {"type": "final_result", "data": {"status": "completed"}},
    ]


def denied_lifecycle(
    *, effect: str | None = "workspace_read", denial_code: str = "workspace_confinement"
) -> list[dict[str, object]]:
    events = allowed_lifecycle()
    policy = copy.deepcopy(effective_policy())
    decision: dict[str, object] = {
        "allowed": False,
        "denial_code": denial_code,
        "policy": policy,
    }
    if effect is not None:
        decision["effect"] = effect
    events[2:2] = [
        {
            "type": "tool_start",
            "data": {"toolCallId": "call-denied", "toolName": "read"},
        },
        {
            "type": "tool_policy",
            "data": {
                "toolCallId": "call-denied",
                "toolName": "read",
                "decision": decision,
            },
        },
        {
            "type": "tool_finish",
            "data": {"toolCallId": "call-denied", "ok": False},
        },
    ]
    return events


def event_data(
    events: list[dict[str, object]], event_type: str, call_id: str | None = None
) -> dict[str, object]:
    for event in events:
        if event["type"] != event_type:
            continue
        data = event["data"]
        if not isinstance(data, dict):
            raise AssertionError("fixture event data must be an object")
        if call_id is None or data.get("toolCallId") == call_id:
            return data
    raise AssertionError(f"fixture {event_type} event was not found")


class ToolPolicyValidationTests(unittest.TestCase):
    def validate(self, run_events: list[dict[str, object]]) -> list[dict[str, object]]:
        return provider_acceptance.validate_exchange(
            exchange_events(run_events),
            HELLO_REQUEST_ID,
            REQUEST_ID,
            RUN_ID,
            require_audio=False,
        )

    def assert_rejected(self, events: list[dict[str, object]]) -> None:
        with self.assertRaises(provider_acceptance.AcceptanceError):
            self.validate(events)

    def test_accepts_allowed_tool_policy_lifecycle(self) -> None:
        events = allowed_lifecycle()
        returned = self.validate(events)
        self.assertEqual(
            [event["type"] for event in returned], [event["type"] for event in events]
        )

    def test_audio_route_exempts_inapplicable_tool_policy_evidence(self) -> None:
        events = [
            {"type": "accepted", "data": {}},
            {"type": "started", "data": {}},
            {"type": "settled", "data": {}},
            {"type": "final_result", "data": {"status": "completed"}},
        ]
        provider_acceptance.validate_exchange(
            exchange_events(events, audio=True),
            HELLO_REQUEST_ID,
            REQUEST_ID,
            RUN_ID,
            require_audio=True,
        )

    def test_audio_route_rejects_unexpected_tool_lifecycle(self) -> None:
        events = [
            {"type": "accepted", "data": {}},
            {"type": "started", "data": {}},
            {
                "type": "tool_start",
                "data": {"toolCallId": "call-unexpected", "toolName": "read"},
            },
            {"type": "settled", "data": {}},
            {"type": "final_result", "data": {"status": "completed"}},
        ]
        with self.assertRaises(provider_acceptance.AcceptanceError):
            provider_acceptance.validate_exchange(
                exchange_events(events, audio=True),
                HELLO_REQUEST_ID,
                REQUEST_ID,
                RUN_ID,
                require_audio=True,
            )

    def test_accepts_denied_registered_read_alongside_successful_read(self) -> None:
        self.validate(denied_lifecycle())

    def test_accepts_unclassified_registered_read_denial(self) -> None:
        self.validate(
            denied_lifecycle(effect=None, denial_code="invalid_tool_arguments")
        )

    def test_accepts_denial_after_predecision_progress(self) -> None:
        events = denied_lifecycle()
        policy_index = next(
            index
            for index, event in enumerate(events)
            if event["type"] == "tool_policy"
            and event_data([event], "tool_policy").get("toolCallId") == "call-denied"
        )
        events.insert(
            policy_index,
            {"type": "tool_progress", "data": {"toolCallId": "call-denied"}},
        )
        self.validate(events)

    def test_accepts_response_style_tool_call_id(self) -> None:
        events = allowed_lifecycle()
        for event in events:
            data = event["data"]
            if "toolCallId" in data:
                data["toolCallId"] = "call_raw|item_exact"
        self.validate(events)

    def test_accepts_unregistered_call_rejected_before_lookup(self) -> None:
        events = allowed_lifecycle()
        events[2:2] = [
            {
                "type": "tool_start",
                "data": {"toolCallId": "call-unknown", "toolName": "unknown_tool"},
            },
            {
                "type": "tool_finish",
                "data": {"toolCallId": "call-unknown", "ok": False},
            },
        ]
        self.validate(events)

    def test_rejects_oversized_tool_call_id(self) -> None:
        events = allowed_lifecycle()
        for event in events:
            data = event["data"]
            if "toolCallId" in data:
                data["toolCallId"] = "a" * (
                    provider_acceptance.MAX_TOOL_CALL_ID_BYTES + 1
                )
        self.assert_rejected(events)

    def test_rejects_non_ascii_tool_call_id(self) -> None:
        events = allowed_lifecycle()
        for event in events:
            data = event["data"]
            if "toolCallId" in data:
                data["toolCallId"] = "\ud800"
        self.assert_rejected(events)

    def test_rejects_missing_accepted_policy_evidence(self) -> None:
        events = allowed_lifecycle()
        del event_data(events, "accepted")["effective_tool_policy"]
        self.assert_rejected(events)

    def test_rejects_missing_registered_tools_evidence(self) -> None:
        events = allowed_lifecycle()
        del event_data(events, "accepted")["registered_tools"]
        self.assert_rejected(events)

    def test_rejects_missing_registered_tool_policy_decision(self) -> None:
        events = [event for event in allowed_lifecycle() if event["type"] != "tool_policy"]
        self.assert_rejected(events)

    def test_rejects_run_without_a_successful_read_decision(self) -> None:
        events = allowed_lifecycle()
        decision = event_data(events, "tool_policy")["decision"]
        assert isinstance(decision, dict)
        decision.clear()
        decision.update(
            {
                "effect": "workspace_read",
                "allowed": False,
                "denial_code": "workspace_confinement",
                "policy": copy.deepcopy(effective_policy()),
            }
        )
        event_data(events, "tool_finish")["ok"] = False
        self.assert_rejected(events)

    def test_rejects_extra_registered_tool(self) -> None:
        events = allowed_lifecycle()
        event_data(events, "accepted")["registered_tools"] = ["read", "write"]
        self.assert_rejected(events)

    def test_rejects_duplicate_registered_tool(self) -> None:
        events = allowed_lifecycle()
        event_data(events, "accepted")["registered_tools"] = ["read", "read"]
        self.assert_rejected(events)

    def test_rejects_host_request_policy_mode_mismatch(self) -> None:
        events = allowed_lifecycle()
        accepted_policy = event_data(events, "accepted")["effective_tool_policy"]
        assert isinstance(accepted_policy, dict)
        accepted_policy["effect_policy"]["value"] = "unsafe_host"
        policy = event_data(events, "tool_policy")["decision"]
        assert isinstance(policy, dict)
        policy["policy"] = copy.deepcopy(accepted_policy)
        self.assert_rejected(events)

    def test_rejects_host_request_policy_provenance_mismatch(self) -> None:
        events = allowed_lifecycle()
        accepted_policy = event_data(events, "accepted")["effective_tool_policy"]
        assert isinstance(accepted_policy, dict)
        accepted_policy["allow_write"]["source"] = "config"
        policy = event_data(events, "tool_policy")["decision"]
        assert isinstance(policy, dict)
        policy["policy"] = copy.deepcopy(accepted_policy)
        self.assert_rejected(events)

    def test_rejects_host_request_capability_mismatch(self) -> None:
        events = allowed_lifecycle()
        accepted_policy = event_data(events, "accepted")["effective_tool_policy"]
        assert isinstance(accepted_policy, dict)
        accepted_policy["allow_write"]["value"] = True
        policy = event_data(events, "tool_policy")["decision"]
        assert isinstance(policy, dict)
        policy["policy"] = copy.deepcopy(accepted_policy)
        self.assert_rejected(events)

    def test_rejects_host_request_timeout_mismatch(self) -> None:
        events = allowed_lifecycle()
        accepted_policy = event_data(events, "accepted")["effective_tool_policy"]
        assert isinstance(accepted_policy, dict)
        accepted_policy["bash_timeout_ms"]["value"] = 120_001
        policy = event_data(events, "tool_policy")["decision"]
        assert isinstance(policy, dict)
        policy["policy"] = copy.deepcopy(accepted_policy)
        self.assert_rejected(events)

    def test_rejects_host_request_output_limit_mismatch(self) -> None:
        events = allowed_lifecycle()
        accepted_policy = event_data(events, "accepted")["effective_tool_policy"]
        assert isinstance(accepted_policy, dict)
        accepted_policy["max_output_bytes"]["value"] = 51_201
        policy = event_data(events, "tool_policy")["decision"]
        assert isinstance(policy, dict)
        policy["policy"] = copy.deepcopy(accepted_policy)
        self.assert_rejected(events)

    def test_rejects_host_request_shell_selection_mismatch(self) -> None:
        events = allowed_lifecycle()
        accepted_policy = event_data(events, "accepted")["effective_tool_policy"]
        assert isinstance(accepted_policy, dict)
        accepted_policy["shell_path"]["value"]["selection"] = "configured"
        policy = event_data(events, "tool_policy")["decision"]
        assert isinstance(policy, dict)
        policy["policy"] = copy.deepcopy(accepted_policy)
        self.assert_rejected(events)

    def test_rejects_policy_for_unregistered_tool(self) -> None:
        events = allowed_lifecycle()
        policy = copy.deepcopy(effective_policy())
        events[2:2] = [
            {
                "type": "tool_start",
                "data": {"toolCallId": "call-unknown", "toolName": "unknown_tool"},
            },
            {
                "type": "tool_policy",
                "data": {
                    "toolCallId": "call-unknown",
                    "toolName": "unknown_tool",
                    "decision": {
                        "effect": "workspace_read",
                        "allowed": True,
                        "authorization": "policy",
                        "policy": policy,
                    },
                },
            },
        ]
        self.assert_rejected(events)

    def test_rejects_unregistered_tool_that_succeeds(self) -> None:
        events = allowed_lifecycle()
        events[2:2] = [
            {
                "type": "tool_start",
                "data": {"toolCallId": "call-unknown", "toolName": "unknown_tool"},
            },
            {
                "type": "tool_finish",
                "data": {"toolCallId": "call-unknown", "ok": True},
            },
        ]
        self.assert_rejected(events)

    def test_rejects_policy_before_tool_started(self) -> None:
        events = allowed_lifecycle()
        policy_index = next(
            index for index, event in enumerate(events) if event["type"] == "tool_policy"
        )
        policy = events.pop(policy_index)
        events.insert(2, policy)
        self.assert_rejected(events)

    def test_rejects_policy_after_matching_tool_finished(self) -> None:
        events = allowed_lifecycle()
        policy_index = next(
            index for index, event in enumerate(events) if event["type"] == "tool_policy"
        )
        policy = events.pop(policy_index)
        finish_index = next(
            index for index, event in enumerate(events) if event["type"] == "tool_finish"
        )
        events.insert(finish_index + 1, policy)
        self.assert_rejected(events)

    def test_rejects_duplicate_tool_policy_decision(self) -> None:
        events = allowed_lifecycle()
        policy_index = next(
            index for index, event in enumerate(events) if event["type"] == "tool_policy"
        )
        events.insert(policy_index + 1, copy.deepcopy(events[policy_index]))
        self.assert_rejected(events)

    def test_rejects_policy_snapshot_that_differs_from_acceptance(self) -> None:
        events = allowed_lifecycle()
        decision = event_data(events, "tool_policy")["decision"]
        assert isinstance(decision, dict)
        snapshot = decision["policy"]
        assert isinstance(snapshot, dict)
        snapshot["allow_write"]["value"] = True
        self.assert_rejected(events)

    def test_rejects_allowed_read_with_wrong_effect(self) -> None:
        events = allowed_lifecycle()
        decision = event_data(events, "tool_policy")["decision"]
        assert isinstance(decision, dict)
        decision["effect"] = "host_read"
        self.assert_rejected(events)

    def test_rejects_allowed_read_with_wrong_authorization(self) -> None:
        events = allowed_lifecycle()
        decision = event_data(events, "tool_policy")["decision"]
        assert isinstance(decision, dict)
        decision["authorization"] = "human_grant"
        self.assert_rejected(events)

    def test_rejects_mismatched_denial_code_and_effect(self) -> None:
        events = denied_lifecycle(
            effect="workspace_read", denial_code="effect_network_denied"
        )
        self.assert_rejected(events)

    def test_rejects_read_denial_not_permitted_by_controlled_policy(self) -> None:
        events = denied_lifecycle(
            effect="workspace_read", denial_code="approval_denied"
        )
        self.assert_rejected(events)

    def test_rejects_denied_policy_that_finishes_successfully(self) -> None:
        events = denied_lifecycle()
        event_data(events, "tool_finish", "call-denied")["ok"] = True
        self.assert_rejected(events)

    def test_rejects_leaky_policy_reason_without_echoing_it(self) -> None:
        secret = "unit-test-secret-policy-reason"
        events = allowed_lifecycle()
        decision = event_data(events, "tool_policy")["decision"]
        assert isinstance(decision, dict)
        decision["reason"] = secret
        with self.assertRaises(provider_acceptance.AcceptanceError) as failure:
            self.validate(events)
        self.assertNotIn(secret, str(failure.exception))

    def test_rejects_malformed_policy_provenance(self) -> None:
        events = allowed_lifecycle()
        accepted_policy = event_data(events, "accepted")["effective_tool_policy"]
        assert isinstance(accepted_policy, dict)
        accepted_policy["allow_write"]["source"] = "credential-file"
        policy = event_data(events, "tool_policy")["decision"]
        assert isinstance(policy, dict)
        policy["policy"] = copy.deepcopy(accepted_policy)
        self.assert_rejected(events)


if __name__ == "__main__":
    unittest.main()
