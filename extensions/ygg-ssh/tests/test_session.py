from __future__ import annotations

import unittest

from ygg_ssh.session import ProbeResult, SshSessions, _session_key

from .helpers import CONTEXT, SessionsHarness, fixture_config


class SessionCommandTests(unittest.TestCase):
    def test_connect_success_selects_target_and_reports_portal(self):
        harness = SessionsHarness()
        text = harness.connect()
        self.assertIn("SSH portal active", text)
        self.assertIn("fixture-alias", text)
        self.assertEqual(harness.probe_calls, ["fixture-alias"])
        self.assertIsNotNone(harness.sessions.context_contribution())

    def test_probe_failure_does_not_select_and_explains_recovery(self):
        harness = SessionsHarness(probe_exit=255)
        text = harness.connect()
        self.assertIn("failed", text)
        self.assertIn("BatchMode", text)
        self.assertEqual(harness.probe_calls, ["fixture-alias"])
        self.assertIsNone(harness.sessions.context_contribution())

    def test_unknown_and_disabled_targets_are_rejected_without_probing(self):
        harness = SessionsHarness()
        unknown = harness.sessions.execute_command(["connect", "nope"], dict(CONTEXT))["text"]
        self.assertIn("unknown configured target", unknown)
        disabled = SessionsHarness(enabled=False)
        text = disabled.connect()
        self.assertIn("disabled", text)
        self.assertEqual(disabled.probe_calls, [])
        self.assertIsNone(disabled.sessions.context_contribution())

    def test_disconnect_clears_selection_and_context(self):
        harness = SessionsHarness()
        harness.connect()
        result = harness.sessions.execute_command(["disconnect", "fixture"], dict(CONTEXT))
        self.assertIn("disconnected", result["text"])
        self.assertIsNone(harness.sessions.context_contribution())
        again = harness.sessions.execute_command(["disconnect", "fixture"], dict(CONTEXT))["text"]
        self.assertIn("not the selected target", again)

    def test_status_list_show_and_usage(self):
        harness = SessionsHarness()
        status = harness.sessions.execute_command([], dict(CONTEXT))["text"]
        self.assertIn("SSH configured targets: 1", status)
        self.assertIn("fixture-alias", status)
        show = harness.sessions.execute_command(["show", "fixture"], dict(CONTEXT))["text"]
        self.assertIn("fixture-alias", show)
        self.assertIn("state: inactive", show)
        harness.connect()
        show = harness.sessions.execute_command(["show", "fixture"], dict(CONTEXT))["text"]
        self.assertIn("state: active", show)
        usage = harness.sessions.execute_command(["bogus"], dict(CONTEXT))["text"]
        self.assertIn("Usage:", usage)
        unknown = harness.sessions.execute_command(["show", "nope"], dict(CONTEXT))["text"]
        self.assertEqual(unknown, "Unknown configured SSH target.")

    def test_status_contribution_reflects_selection(self):
        harness = SessionsHarness()
        idle = harness.sessions.status_contribution()
        self.assertIn("idle", idle["text"])
        harness.connect()
        active = harness.sessions.status_contribution()
        self.assertIn("active", active["text"])
        self.assertIn("fixture", active["text"])

    def test_context_block_describes_tunnel_and_untrusted_data(self):
        harness = SessionsHarness()
        self.assertIsNone(harness.sessions.context_contribution())
        harness.connect()
        contribution = harness.sessions.context_contribution()
        self.assertIsNotNone(contribution)
        assert contribution is not None
        self.assertEqual(contribution["label"], "ygg-ssh")
        self.assertEqual(contribution["placement"], "prompt_suffix")
        content = " ".join(contribution["content"].split())
        self.assertIn("SSH tunnel", content)
        self.assertIn("ssh <alias>", content)
        self.assertIn("untrusted", content)
        self.assertIn("fixture-alias", content)
        self.assertIn("/srv/fixture", content)

    def test_settle_session_drops_matching_selection(self):
        harness = SessionsHarness()
        harness.connect()
        harness.sessions.settle_session("other-session")
        self.assertIsNotNone(harness.sessions.context_contribution())
        harness.sessions.settle_session("fixture-session")
        self.assertIsNone(harness.sessions.context_contribution())

    def test_session_key_prefers_host_session_id(self):
        self.assertEqual(_session_key(CONTEXT), "fixture-session")
        self.assertEqual(_session_key({}), "")
        self.assertEqual(
            _session_key({"resource_owner": {"session_id": "owner-session"}}),
            "owner-session",
        )


class DefaultProbeTests(unittest.TestCase):
    def test_default_probe_uses_batch_mode_and_reports_exit(self):
        config = fixture_config()
        sessions = SshSessions(config)
        result = sessions._default_probe(config.targets[0])
        # The fixture alias does not exist; OpenSSH exits 255 quickly. Either
        # way the probe must complete without hanging and report a status.
        self.assertIsInstance(result, ProbeResult)
        self.assertFalse(result.ok)

    def test_constructor_bounds_connect_timeout(self):
        with self.assertRaises(ValueError):
            SshSessions(fixture_config(), connect_timeout_ms=1)


if __name__ == "__main__":
    unittest.main()
