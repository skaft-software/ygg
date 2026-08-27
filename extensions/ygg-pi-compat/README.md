# `ygg-pi-compat`

This directory contains the Node compatibility host used by `ygg pi install`.
It is intentionally a subprocess: Pi extensions remain Pi code, while Ygg
continues to own the model loop, JSON-RPC transport, trust gates, persistence,
and process cleanup.

The generated wrapper supports the initial compatibility subset:

- Pi tools and textual/image results;
- a generated package-specific `/<name> COMMAND ...` command route;
- notifications, confirmations, and text input;
- basic lifecycle events and local Pi event-bus behavior; and
- bounded progress and cancellation.

It does not silently emulate Pi's TUI, provider, session, compaction, or
arbitrary mutation APIs. Those calls produce compatibility errors or are
reported during bridge startup.

The bridge uses the user's installed `@earendil-works/pi-coding-agent` runtime
and does not install npm dependencies. Set `YGG_PI_PACKAGE` to the package root
when it cannot be found beside `pi` on `PATH`.
