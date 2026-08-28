# Ygg presentation contract

Ygg's default presentation is one coherent terminal instrument, not a fixed
brand hue. Its stable identity is the tree/dot-matrix mark, black terminal
field, typography, spacing, interaction grammar, and trust semantics. The
selected model supplies an adaptive atmosphere layered over that structure.

## Stable versus adaptive visual tokens

Stable product tokens include the terminal surface, text hierarchy, layout,
spacing, tree silhouette, semantic success/warning/error colours, and
permission treatment. Adaptive model tokens include the tree gradient, wordmark
accent, prompt/focus accent, active divider, and selected-model highlight.
Changing models changes ambience, never behavior or authority.

Known model families use palettes matched to or inspired by the recognizable
Artificial Analysis model-colour system. This is a visual mapping, not an
Artificial Analysis partnership or endorsement. Unknown and local models use a
deterministic fallback so their colour does not change between sessions.
Contrast is normalized for the active terminal background, and status is never
communicated by hue alone.

## Information layers

The UI separates three layers:

1. **Durable conversation** — user turns, assistant conclusions, meaningful
   summaries, useful tool results, and errors that need action.
2. **Live activity** — the current request, running tools, retries, compaction,
   progress, waiting, and active workers. These update in place and settle into
   one final state.
3. **Diagnostics** — raw or detailed telemetry, complete worker prompts, retry
   metadata, internal IDs, and retained full output. Diagnostics are available
   on demand and do not become default transcript rows.

Structured telemetry is evidence for measurement; it is not a one-row-per-event
rendering instruction. Presentation code coalesces activity by stable request,
tool, and worker identity.

## Interaction tone

The default should be calm, dense under pressure, and precise about state. The
startup tree glimmer is a signature interaction: subtle, non-blocking,
finite, and safe to degrade on limited or reduced-motion terminals. Progressive
disclosure keeps raw detail one action away without imposing a dashboard.

A useful internal rule is: **calm by default, detail on demand, raw truth one
keystroke away**. Any future theme or extension must preserve the default
hierarchy before adding options.
