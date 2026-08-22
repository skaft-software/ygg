# Changelog

## 0.2.0

- Rework into a zero-tool SSH portal: the extension no longer registers any
  model-callable tools. On `/ssh connect <target>` it verifies non-interactive
  authentication with one BatchMode probe and contributes a small prompt-context
  block telling the model it is operating through an SSH tunnel, which alias to
  use, and that remote output is untrusted data.
- Remove `ssh_status`, `ssh_exec`, `ssh_read`, `ssh_write`, and `ssh_list`;
  the owner-fence machinery; per-exec confirmation plumbing; authority levels;
  output/file limits; presentation snapshots; and digest-pinned project
  configuration. Remote work happens through the agent's normal shell tool with
  the remote account's own OpenSSH-enforced permissions.
- Shrink configuration to `{version, targets}` with targets of
  `{alias, label?, cwd?, enabled?}`; keep the strict file-trust validation
  (regular non-symlink owned file, safe permissions, duplicate-key rejection,
  bounded sizes).
- Add the `ssh-remote-work` companion skill carrying remote-work technique
  (quoting, discovery-before-read, multiplexing guidance) loaded on demand.
- Replace the process-management test fleet with offline config, session,
  wire-protocol, and release smoke tests around the probe and context flow.

## 0.1.0

- Add the API `0.2` owner-fenced system OpenSSH runtime for explicit configured aliases.
- Add bounded status, exec, remote read/write, action-time mutation approval, cancellation, health, ambiguity, explicit recovery, and process-tree cleanup.
- Add generic presentation snapshots, `/ssh` headless controls, fake-SSH fixtures, self-contained vendored SDK, package tests, and release smoke coverage.
