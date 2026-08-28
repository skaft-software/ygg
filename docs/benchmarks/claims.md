# Claims matrix

This matrix is intentionally conservative.  “Reported” means supplied by the
operator; it is not the same as independently reproduced evidence.

| Claim | Current evidence | Confidence | Missing experiment | Reproduction command |
| --- | --- | --- | --- | --- |
| Ygg tops Terminal-Bench 2.1 | Operator-reported 391/445 raw (`87.87%`) on the v0.6.2 control; control fingerprint and gaps are in [baseline-v0.6.2.md](baseline-v0.6.2.md). | **PRELIMINARY** | Attach Harbor/dataset revision, raw trials, and independently reproduce the canonical run plus an integrity adjudication. | `git checkout v0.6.2`; run the pinned Harbor command after filling the missing control fields. |
| Ygg has the best same-model harness performance | No controlled same-endpoint shootout yet. | **UNSUPPORTED** | Run Ygg, Pi, OpenCode, Aider, and Goose on identical local endpoint, hardware, tasks, limits, and timeout. | `python3 scripts/bench-systems.py ...`; separate harness drivers are still required. |
| Ygg is fastest | A local v0.6.2 release `--version` smoke measured 4.896 ms median over 5 launches on one Mac; this is not end-to-end speed. | **PRELIMINARY** | Repeated cold/idle/resume/UI/provider-boundary measurements on multiple machines and competitors. | `python3 scripts/bench-systems.py --binary ./target/release/ygg --repetitions 9`. |
| Ygg uses the least memory | No comparable agent-only RSS/PSS dataset. | **UNSUPPORTED** | Measure idle, normal, large-session, and 1/2/4-session RSS/PSS with inference server excluded. | `python3 scripts/bench-systems.py --idle-command ... --concurrency 1,2,4`. |
| Ygg is most token-efficient | Existing notes and new telemetry explain disjoint cache accounting, but no A/B result exists. | **PRELIMINARY** | Same model/task/harness comparison with request, cache, output, reasoning, and success-normalized totals. | `ygg --telemetry ./run.jsonl ...`; analyze with `--telemetry`. |
| Ygg is best for local inference | Custom OpenAI-compatible endpoints, offline startup, model discovery, and explicit context metadata are implemented and tested. | **PRELIMINARY** | Independent onboarding test across vLLM, llama.cpp, Ollama, SGLang, and LM Studio, including saturated/error states. | `ygg --login custom`; configure `~/.ygg/credentials/custom.json`; `ygg --offline --model ...`. |
| Ygg improves local-model capability | No same-model task success comparison is available. | **UNSUPPORTED** | Blind, randomized harness shootout using identical local weights and server. | Controlled shootout workflow in [README.md](README.md#same-model-harness-shootout). |
| Ygg is reliable enough for daily use | The frozen v0.6.2 control recorded 1,461 package tests (229 + 408 + 824); one primary author reports daily use. | **PRELIMINARY** | Multi-user beta protocol with installation, crash, resume, cancellation, and abandonment data. | Follow [beta-protocol.md](beta-protocol.md). |
| Ygg is easier/simpler than alternatives | Small core and optional extensions are design facts; no independent user study. | **UNSUPPORTED** | Timed first-ten-minute onboarding study with comparable tasks and failure capture. | Follow [beta-protocol.md](beta-protocol.md). |

The reported control is not silently updated by this matrix.  A future result
must add a dated row or a versioned report with its own binary, model, provider,
benchmark, and environment identity.
