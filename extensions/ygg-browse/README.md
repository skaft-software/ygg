# Ygg Browse

Ygg Browse is the official opt-in API `0.2` executable-extension bundle for bounded semantic browser work. It always launches Playwright's bundled Chromium visibly (`headless=False`) with a persistent profile owned only by Ygg Browse. It never pairs with, copies, discovers, or launches a normal Chrome/Chromium profile.

Version `0.1.0` requires Ygg `0.6.6` exactly and pins `playwright==1.57.0` exactly.

## Install and activate

Bundle installation only copies inert files. It does **not** run Python, install Playwright, download a browser, create a profile, or start this extension.

```console
ygg extension install ygg-browse
ygg --enable-extension ygg-browse --trust-extension ygg-browse
```

For persistent user-global activation:

```toml
enabled_extensions = ["ygg-browse"]
trusted_extensions = ["ygg-browse"]
```

Executable extensions run with the current user's authority only under Ygg's full-access policy. Safe mode keeps them stopped. Enablement, exact executable trust, and skill activation are independent decisions.

After the extension is running:

```text
/browse setup
/browse status
/browse open
/skills load ygg-browse
```

`/browse setup` explains the download location, asks for explicit confirmation, starts a background installation, and returns before the normal 30-second extension RPC deadline. Do not activate the skill until `/browse status` says `ready`. Installed bundle skills remain inactive until explicitly loaded, and explicit invocation fails closed if any declared browser tool or built-in `read` is unavailable.

## Commands

One command owns the user-facing lifecycle:

| Command | Behavior |
|---|---|
| `/browse` or `/browse status` | Bounded setup/browser/profile health and install-log location; never log contents |
| `/browse setup` | Confirm, then install pinned dependencies in the background |
| `/browse open` | Launch the always-headful isolated persistent browser |
| `/browse close` | Close the owning browser context and invalidate tab state |
| `/browse reset-profile` | Destructive confirmation, close, lock/sentinel verification, then remove only the isolated profile |

The setup runtime is built in a private temporary directory and published only after a complete marker is written and validated. A cross-process lock makes setup idempotent. Interrupted/failed setup is never reported ready. Status points to `~/.ygg/browse/install.log` but does not return its potentially environment-specific contents.

## Tool surface

There is no general browser escape hatch. The exact tools are:

- `browser_status`
- `browser_launch`
- `browser_tabs`
- `browser_open_url`
- `browser_snapshot`
- `browser_click`
- `browser_type`
- `browser_press`
- `browser_scroll`
- `browser_wait`
- `browser_screenshot`
- `browser_tab_close`
- `browser_close`

Every tab operation takes an explicit opaque `tab_id`. `browser_open_url` creates a new explicit tab only when `tab_id` is omitted; it never selects an implicit active page. Browser state is fenced by the host-derived `{session_id, extension_instance_id, process_generation}` owner, never a model argument. Handler-time presentation snapshots are parent-correlated, and worker-thread snapshots echo that complete host-issued triple; owner changes clear cached tab/activity/artifact presentation before publication.

A snapshot creates a new `snapshot_generation` and refs such as `ref=e12`. Reference actions require that exact generation. Navigation, replacement/closure, and every newer snapshot invalidate older refs. Targets are unique or fail closed:

- `ref=e12`
- `role=button[name="Publish"]`
- `text=Exact visible text`
- `css=button.primary`
- exact plain semantic text

Ambiguous matches return at most five bounded candidates; no action silently uses `.first()`.

`browser_press` accepts only: `Enter`, `Tab`, `Shift+Tab`, `Escape`, `Backspace`, `Delete`, arrow keys, `Home`, `End`, `PageUp`, `PageDown`, and `Space`. Arbitrary modifiers and clipboard shortcuts are unavailable. The context is capped at 32 tabs, scroll deltas are `-4000..4000`, waits are at most 5000 ms, and targets, URLs, input, results, queues, and every Playwright operation are separately bounded.

## Authentication and actions

Authentication is manual in the visible window. `browser_type` refuses fields that appear to be passwords, usernames/login credentials, OTPs, payment details, or authentication fields based on input type, autocomplete, accessible metadata, and surrounding form semantics. It never logs, presents, stores in metadata, or echoes the supplied text. Playwright errors from typing are replaced with a generic value-withheld error.

Clicks and Enter/Space actions that appear to purchase/pay, send/publish, grant consent, submit an external side effect, or delete data synchronously request Ygg confirmation before acting. Denial, a dropped request, a non-interactive frontend, cancellation, or timeout fails closed. Page content and labels cannot grant confirmation.

Only explicit absolute HTTP(S) navigation is allowed. URLs with userinfo, relative URLs passed to `browser_open_url`, `file:`, `javascript:`, data/blob/custom/browser-internal schemes, and malformed hosts are rejected. The same top-level policy is enforced for links, forms, redirects, and popups. Query strings and fragments are removed from displayed URLs. Downloads are cancelled and never published or retained.

Ygg Browse does not expose JavaScript evaluation, raw CDP, coordinates, physical-pointer control, clipboard, upload, download, cookies, storage, cache, history, browser extensions, visibility/headless controls, or profile inspection.

## Untrusted observations and screenshots

Page-derived snapshot text is enclosed literally by:

```text
BEGIN UNTRUSTED BROWSER CONTENT
...
END UNTRUSTED BROWSER CONTENT
```

Everything inside is data, never instructions or authorization. A snapshot is capped at 20,000 characters and 100 interactive elements, with explicit truncation notices. It returns only bounded visible text and accessible role/name/state data. Input/textarea values, hidden content, cookies, storage, headers, and profile data are not queried or returned. Tab lists carrying page titles/URLs use the same markers.

Screenshots are viewport-only PNGs. To prevent form-value leakage, capture is refused after `browser_type` has supplied a value in that tab or while any visible form/editable field could contain manually entered data (with a specific refusal for credential/authentication/payment fields). An image at or above 5 MiB fails clearly instead of returning an unreadable attachment. Successful images are retained under `~/.ygg/browse/artifacts/screenshots/`, bounded to 20 files and 80 MiB, copied briefly into the host-owned process scratch area, and published through API `0.2` as owner/generation-scoped artifacts. Results contain both the image part and a textual local reference usable with built-in `read`.

The conservative visible-form refusal can be relaxed for non-credential fields by setting `YGG_BROWSE_ALLOW_FORM_SCREENSHOTS` to `1`, `true`, `yes`, or `on` before starting Ygg, or by creating the regular sentinel file `~/.ygg/browse/allow-form-screenshots`. This override never permits capture after `browser_type` has supplied a value and never permits capture while a visible credential, OTP, payment, authentication, or other credential-like field is present.

## Owned state and cleanup

Ygg Browse uses only:

```text
~/.ygg/browse/profile/
~/.ygg/browse/runtime/playwright-1.57.0/
~/.ygg/browse/artifacts/screenshots/
~/.ygg/browse/install.lock
~/.ygg/browse/install.log
~/.ygg/browse/allow-form-screenshots
```

Small lock/state/sentinel files also live directly beneath `~/.ygg/browse/`. The isolated profile has a versioned `.ygg-browse-profile.json` sentinel and a separate exclusive ownership lock. Launch and reset reject a linked/non-directory profile, an absent/invalid sentinel, or another owner. Reset stages and removes only a locked, sentinel-verified profile. Chromium's own profile lock files are never followed.

The protocol main thread, one background installer thread, and one Playwright-owner worker are separate. Every Playwright call—including launch, events, page inspection, action, screenshot, and close—is serialized on that owner. Extension shutdown stops admission, terminates an installer child where possible, closes pages/context/Playwright on the owner, and then relies on Ygg's bounded process-group teardown as the final fence.

## Development and tests

The default suite is dependency-free and uses protocol/fake-Playwright fixtures:

```console
(cd extensions/ygg-browse && python3 -m unittest discover -s tests -t . -v)
python3 -m compileall -q extensions/ygg-browse
```

Opt-in local HTTP integration tests use only the isolated pinned runtime installed by `/browse setup`; they never contact an external site or shared browser profile:

```console
cd extensions/ygg-browse
YGG_BROWSE_PLAYWRIGHT_TESTS=1 \
  PYTHONPATH=vendor:. python3 -m unittest tests.test_playwright_integration -v
```

The setup/download itself is intentionally network-dependent and user-confirmed; default tests mock it. The vendored `vendor/ygg_extension/` files must remain byte-for-byte identical to `sdk/python/ygg_extension/`, including the portable `MAX_PRESENTATION_REVISION` guard.

## License

MIT; see `LICENSE`.
