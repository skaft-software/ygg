# Configured-provider acceptance

Ygg Serve uses the coding agent's provider stack rather than a web-specific
client. Release acceptance therefore has two separate gates:

1. a deterministic, credential-free conformance test on every pull request; and
2. a manually approved credentialed check against representative live providers
   before a stable release.

The two gates must remain separate. Pull-request CI must not inherit developer or
repository provider credentials, and credentialed results must not upload raw
provider traffic, prompts, or logs as artifacts.

## Supported provider matrix

| Route | Providers | Credential source | Deterministic coverage | Stable-release live representative |
| --- | --- | --- | --- | --- |
| OpenAI Responses | OpenAI API models | `OPENAI_API_KEY` | `ygg-ai` protocol/client tests | OpenAI |
| OpenAI Responses with subscription auth | Codex models | `ygg --login codex` or first-use import into Ygg's owner-only store | protocol, refresh, migration, and redaction tests | Codex when this route changes |
| Anthropic Messages | Anthropic, MiniMax | `ANTHROPIC_API_KEY`, `MINIMAX_API_KEY` | `ygg-ai` protocol/client tests | Anthropic |
| OpenAI Chat | DeepSeek, OpenRouter, Groq, Cerebras, xAI, Together AI, Fireworks AI, NVIDIA, Hugging Face, Moonshot AI, Xiaomi, OpenCode Zen | provider variable listed below | `ygg-ai` protocol/client tests plus full Serve process acceptance through a local endpoint | OpenRouter or another affected preset |
| Custom OpenAI-compatible Chat | User-defined local or remote endpoint | `none`, `bearer_env`, or the `api_key_env` shorthand in `~/.ygg/credentials/custom.json` | full Serve process acceptance through a disposable loopback fixture | One user-configured endpoint when custom routing changes |

The built-in OpenAI Chat credential variables are:

| Provider | Environment variable |
| --- | --- |
| DeepSeek | `DEEPSEEK_API_KEY` |
| OpenRouter | `OPENROUTER_API_KEY` |
| Groq | `GROQ_API_KEY` |
| Cerebras | `CEREBRAS_API_KEY` |
| xAI | `XAI_API_KEY` |
| Together AI | `TOGETHER_API_KEY` |
| Fireworks AI | `FIREWORKS_API_KEY` |
| NVIDIA | `NVIDIA_API_KEY` |
| Hugging Face | `HF_TOKEN` |
| Moonshot AI | `MOONSHOT_API_KEY` |
| Xiaomi | `XIAOMI_API_KEY` |
| OpenCode Zen | `OPENCODE_API_KEY` |

A custom provider's environment-variable name is user-selected in its
`bearer_env` auth entry or `api_key_env` shorthand. API keys must not be placed
directly in Git-tracked configuration. A provider being listed here means Ygg
supports its wire route; individual model capabilities still depend on the provider's model
metadata.

## Deterministic CI gate

`apps/web/tests/live-host.spec.ts` launches the real Serve-capable `ygg` binary
with a temporary owner-only `HOME`, workspace, credential registry, and session
directory. Its provider is an in-process loopback OpenAI-compatible server. The
child environment is an allowlist containing a fake fixture token, so ambient
`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, and other live credentials cannot enter
the test process.

The test proves:

- bearer authentication and provider-qualified model selection;
- streamed text;
- a streamed `read` tool call and tool-result replay on the next request;
- retries after fixture `429` throttling and `408` timeout responses;
- explicit `/compact`, durable checkpointing, process restart, and resume;
- cancellation of an in-flight stream; and
- bounded provider/phase failure diagnostics that omit the provider body,
  fixture token, prompt canaries, request IDs, and provider error codes.

Run it after building the feature-enabled binary:

```sh
cargo build -p ygg-coding-agent --bin ygg --features serve --locked
cd apps/web
npm ci
npm run test:e2e:live
```

This is the only configured-provider test allowed in ordinary CI. It must stay
loopback-only and credential-free.

## Credentialed stable-release gate

The protected `Stable provider acceptance` workflow is the only release evidence
accepted by the release workflows. Configure its
`stable-release-provider-acceptance` environment with required reviewers and
these spend-limited secrets:

- `LIVE_OPENAI_API_KEY`
- `LIVE_ANTHROPIC_API_KEY`
- `LIVE_OPENAI_CHAT_BASE_URL` and `LIVE_OPENAI_CHAT_API_KEY`
- `LIVE_AUDIO_BASE_URL` and `LIVE_AUDIO_API_KEY`

Dispatch `.github/workflows/provider-acceptance.yml` with the workflow ref and
`source_sha` both set to the exact 40-character candidate commit. Supply the
reviewed model and provider IDs as workflow inputs. Do not dispatch it for pull
requests or forks.

The workflow builds `ygg-host` from that checkout and invokes
`scripts/provider-acceptance.py` in isolated owner-only workspaces. It checks:

1. OpenAI Responses, Anthropic Messages, and OpenAI-compatible Chat each stream
   text, call the read-only `read` tool, and return a disposable file canary.
2. The exact production `provider:model` selected for native audio consumes an
   integrity-pinned spoken-code WAV attachment and correctly transcribes the
   code, which is not present in the model prompt.
3. Every route obeys the bounded host protocol lifecycle, sequence, scope, and
   terminal-event contract.

Provider stderr is discarded, the child receives an allowlisted environment,
and raw protocol/provider traffic is never uploaded. Workflow logs and the job
summary contain only route labels, provider/model IDs, candidate SHA, and
sanitized pass/fail status. Retry, cancellation, compaction, persistence, and
invalid-credential redaction remain deterministic CI responsibilities; the live
gate must not intentionally create billable throttling or network disruption.

Both stable release workflows query GitHub's immutable Actions history and fail
closed unless the exact source commit has a successful `workflow_dispatch` run
with a recorded approval for `stable-release-provider-acceptance`. A local run,
a run for another SHA, or an unapproved successful run is not release evidence.

## Release record

A stable tag is blocked while any required row is `NOT RUN` or `FAIL`.

| Candidate | Gate | Provider/model | UTC date | Result | Reviewer |
| --- | --- | --- | --- | --- | --- |
| Uncommitted `v0.4.0` worktree | Deterministic Serve configured-provider matrix | `custom/e2e/e2e-model` | 2026-08-10 | PASS | Local release review |
| `v0.4.0` release SHA | OpenAI Responses live representative | To be recorded | — | NOT RUN — no credential available | — |
| `v0.4.0` release SHA | Anthropic Messages live representative | To be recorded | — | NOT RUN — no credential available | — |
| `v0.4.0` release SHA | OpenAI Chat live representative | To be recorded | — | NOT RUN — no credential available | — |
| `v0.4.0` release SHA | Native audio production route | To be recorded | — | NOT RUN — no accepted route or credential available | — |

Replace the pending rows with the protected run's sanitized metadata before
creating the stable GitHub release. A local dirty-worktree pass is useful
regression evidence but is not release-SHA evidence.
