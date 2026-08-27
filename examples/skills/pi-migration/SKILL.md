---
name: Pi Migration Cleanup
description: Use Ygg's zero-token Pi inventory and organize only the remaining compatibility decisions.
version: 0.1.0
required-tools:
  - read
  - bash
tags:
  - pi
  - migration
  - compatibility
---
# Pi Migration Cleanup

Use this procedure only after explicit activation. Keep the model portion small;
the host scanner, not the model, owns discovery and classification.

1. Run `ygg migrate pi --dry-run --summary` first. Request `--json` only when a
   package needs targeted inspection.
2. Never read Pi credential/model stores, install dependencies, execute Pi
   package code, or send setup contents to a network service.
3. Treat `direct` resources as already portable, `bridge` resources as
   candidates for `ygg pi install PATH`, and `native_port`/`manual`/`blocked`
   resources as explicit residual work.
4. Only link a source after the user has reviewed it. `ygg pi install PATH`
   creates an inert wrapper; it does not enable or trust the process.
5. Report the exact source path, compatibility classification, unsupported
   surfaces, and the enable/trust command. Do not claim that an extension is
   compatible until `/extensions status` shows a healthy process.
