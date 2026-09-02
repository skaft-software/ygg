# Distribution channels

Ygg release channels are fed by the same immutable native release assets. The
Homebrew formula is generated from the signed `YGG_RELEASE_METADATA.json`
record produced by the protected binary-release workflow. The generator does
not read the repository's package manifest for a version and does not query a
mutable `latest` or release API. Before a formula is rendered, the workflow
verifies the metadata's Sigstore bundle, its canonical tag/workflow/source
identity, the `YGG_SHA256SUMS` digest, and the two macOS archive digests.

## Homebrew

For a release whose formula has completed the protected tap handoff, the
supported Homebrew channel is macOS only (Apple silicon and Intel). It
installs the two native executables and declares `ripgrep` as a dependency:

```sh
brew install skaft-software/tap/ygg
ygg --version
ygg doctor
```

`brew upgrade ygg` follows the formula update after the protected tap
publication job has opened and merged it. Linux users should use the signed
binary installer or the npm launcher instead; the Ygg Homebrew formula does
not provide a Linux runtime.

The formula uses the archive's versioned top-level directory and installs only
`ygg` and `ygg-host`. It does not run an npm lifecycle hook, invoke Cargo, or
build from source. A formula generated from local release assets can be checked
offline with:

```sh
scripts/test-homebrew-formula.sh
```

That check proves deterministic metadata parsing, archive checksum handoff,
formula syntax, expected architecture URLs, and failure on a changed digest.
It is not hosted macOS acceptance and does not prove that a tap mutation or a
public release has completed.

## Release handoff and tap publication

A release candidate must first have a stable `vX.Y.Z` tag, a signed native
checksum manifest, and a signed immutable metadata document. The Homebrew
workflow downloads that exact metadata and its signature from the canonical
release, verifies the Sigstore identity against the release workflow commit,
and renders `Formula/ygg.rb` in a clean tap checkout. Formula rendering and tap
publication require an explicit dispatch from the matching
`ygg-binaries-vX.Y.Z` tooling tag; binary release completion alone never mutates
the tap. The workflow then checks the formula diff and opens a protected tap
pull request; it must never replace a formula directly on the default branch.

The tap repository and its GitHub App/installation permission are deployment
configuration, not source-controlled credentials. Configure them in the
protected release environment. A missing token, non-canonical tap repository,
metadata mismatch, failed formula check, or unavailable hosted macOS acceptance
must stop the handoff without changing the tap.

If a formula update was opened from a wrong or incomplete release, close the
pull request without merging it and regenerate from the same immutable release
metadata. Do not edit SHA-256 values by hand. If a bad formula was merged,
revert the tap commit and open a replacement from a newly reviewed metadata
record; do not point the formula at a mutable release alias.

## Other channels

- The version-pinned shell installer is the native fallback for macOS and
  GNU/Linux x86-64.
- The npm channel provides a no-lifecycle launcher and optional native packages
  for the same three supported targets. See
  [`docs/release/npm-trusted-publishing.md`](release/npm-trusted-publishing.md).
- Cargo installation remains available for users who intentionally build from
  the canonical tag.
