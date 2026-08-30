# Ygg presentation contract

Ygg's default presentation is one coherent terminal instrument, not a fixed
provider hue. Its stable identity is the tree/dot-matrix mark, typography,
spacing, interaction grammar, semantic colours, and trust treatment. Model
identity is retained as provenance without turning every working surface into
provider branding.

## Stable versus adaptive visual tokens

Stable product tokens include the terminal surface, text hierarchy, layout,
spacing, tree silhouette, Ygg interaction accent, and semantic
success/warning/error colours. Adaptive model tokens identify model provenance:
the startup atmosphere, each persisted prompt card, and the composer for the
model that will receive the next prompt. Changing models changes provenance and
ambience, never behavior or authority.

Every submitted prompt captures its model-lab colour. That stored colour paints
the prompt marker and card background for the lifetime of the transcript, so a
later model switch cannot recolour old prompts. The composer immediately adopts
the selected next model's colour, including while another model's run is still
settling. Picker and completion focus use Ygg's UI accent; queued-steering
chrome follows the selected model's adaptive accent because it previews input
destined for that model. Contrast is normalized for the detected terminal
background, and status is never communicated by hue alone.

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

## Surface and geometry contract

Transcript surfaces, the composer, footer, and pickers resolve one shared
horizontal grid. In the default theme, full-width rules, cards, and event or
prompt markers begin at terminal column 0; primary text begins at column 2; and
nested detail begins at column 4. Narrow pickers collapse to compact rows,
regular terminals stack labels over metadata, and genuinely wide terminals may
use columns. The composer keeps stable full-width top and bottom rules and no
side borders, so copied draft text cannot include frame characters. Its height
grows proportionally but remains bounded by terminal height.

There is exactly one breathing row between transcript content and the composer.
The composer does not animate or recolour merely because work starts or draft
text changes; transcript activity owns liveness. The footer is one quiet line:
model/reasoning identity on the left and bounded context/session cost on the
right, dropping secondary fields before truncating primary identity.

Context composition is a semantic timeline. Segments run left-to-right in the
order the model receives them, from system/provider framing and tool schemas
through chronological messages and pending adjustments to output reserve and
remaining capacity. Every displayed category has its own colour; categories
must not be duplicated merely to create a separate accounting slice.

Queued steering is a pending-state hint, not a second transcript. It occupies at
most two rows: one count and one clipped preview of the oldest queued message,
with a compact count for additional messages.

## Approval contract

An approval panel is an enforcement surface, not decorative chrome.

- The prompt and bounded consequence detail are retained separately from the two
  action labels, so identical descriptions render once without being discarded.
- Consequence detail is terminal-sanitized, wraps inside the shared inset, and is
  capped at three rows with an explicit omission marker.
- At constrained heights, the selected action row takes priority over detail.
  Enter cannot confirm a confirmation action unless that selected action is
  present in the rendered panel frame.
- Confirmation panels do not expose a filter or item count; arbitrary typing
  cannot mutate their choices.

## Outcome contract

Terminal outcomes must remain distinguishable after animation stops:

- normal completion uses the success glyph and `completed`;
- completion with warnings uses the warning glyph and the explicit
  `completed with warnings` label;
- interruption remains a warning-class terminal state; and
- failure uses the error glyph plus `failed` and elapsed time.

A collapsed failure always retains a useful reason immediately below its
headline. The reason is credential-redacted at the inference boundary,
terminal-sanitized again for presentation, bounded to 4 KiB at a UTF-8 boundary,
and included in semantic copy. Raw envelopes and headers remain diagnostic
evidence rather than transcript copy.

Tool failures follow the same rule: collapsed rows retain a bounded actionable
summary while complete captured output remains available through disclosure when
it exists.

## Interaction tone

The default should be calm, dense under pressure, and precise about state. The
startup tree glimmer is a signature interaction: subtle, non-blocking, finite,
and safe to degrade on limited or reduced-motion terminals. Progressive
disclosure keeps raw detail one action away without imposing a dashboard.

A useful internal rule is: **calm by default, detail on demand, raw truth one
keystroke away**. Any future theme or extension must preserve the default
hierarchy before adding options.
