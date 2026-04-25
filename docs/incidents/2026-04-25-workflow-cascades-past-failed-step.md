# Workflow run cascades past a failed step and hangs

- **Date observed:** 2026-04-25
- **Reporter:** stage-4 dogfooding session
- **Affected crate(s):** ork-core (compiler + engine), ork-llm (openai_compatible)
- **Severity:** high

## What happened

Running `make -C demo demo-stage-4` against the live Minimax catalog, the
`research_repos` step failed mid-stream with
`LLM provider error: stream read failed: error decoding response body`
(reqwest body-decode error, i.e. the upstream SSE stream was closed
mid-flight). The engine then walked the `Always` edge synthesised from
`synthesize.depends_on: [research_repos]` and started the next step with
an unsubstituted `{{research_repos.output}}` prompt against the same
flaky LLM. The polling loop sat in `running` for 120s+ and tripped the
180s timeout instead of failing fast.

## Expected vs actual

- **Expected:** When `research_repos` fails with no `condition.on_fail`
  declared, the run terminates promptly with `Failed` status, surfacing
  the upstream LLM error to the demo script.
- **Actual:** The engine fans forward through every `depends_on` edge
  regardless of pass/fail because the compiler emits
  `EdgeCondition::Always` for them. Downstream steps run with bogus
  prompts, and the demo polling timeout fires before the engine returns.

Compounding the friction:
- The engine's per-step `info!`/`error!` events do not carry `run_id`,
  so the demo's filtered `ork-api.log` tail in the timeout dump only
  surfaces the two service-layer create/start lines — none of the
  per-step lifecycle events for the run.
- A single transient `error decoding response body` is enough to kill
  the whole run; no retry is attempted by the OpenAI-compatible
  provider.

## Reproducer

```
make -C demo demo-stage-4
# (with MINIMAX_API_KEY set, against the real upstream)
```

Terminal excerpt (`terminals/22.txt`, lines 959–1028) shows two
`step_results` entries followed by four heartbeats and the 180s
timeout. `demo/logs/ork-api.log` lines 87–90 show the cascade in the
engine logs (interleaved with a previous run, hence no `run_id` to
discriminate).

## Hypothesis

`crates/ork-core/src/workflow/compiler.rs` line 99 unconditionally emits
`EdgeCondition::Always` for `depends_on`. `crates/ork-core/src/workflow/engine.rs`
line 269–277 walks the edge regardless. Both behaviours together turn a
failed step into a slow-motion cascade.

## Resolution

- **Test:** `crates/ork-core/tests/engine_failed_step_does_not_cascade.rs`
- **Test:** `crates/ork-llm/tests/openai_compatible_stream_retry.rs`
- **Fix:** compiler emits `OnPass` for `depends_on` (linear-chain
  failures terminate the run); engine threads `run_id` into step-level
  log fields; OpenAI-compatible provider retries once on a transient
  failure regardless of whether it happened during the initial
  `send().await` (TCP/TLS reset, 5xx) or mid-stream while polling the
  response body — but only before any SSE event has been yielded to
  the downstream consumer, and only if the prior attempt was not a
  4xx (auth / validation / quota). Initial-send and mid-stream
  retries share a single `STREAM_MAX_ATTEMPTS = 2` budget so the
  worst case is two HTTP requests per `chat_stream` call.
- **Verification (live demo, 2026-04-25):** `make -C demo demo-stage-4`
  now terminates promptly when the upstream is flapping —
  `research_repos:failed(58s)` is the only failure recorded and the
  run flips to `failed` instead of cascading to `synthesize`/
  `write_plan`/`review`. `demo/logs/ork-api.log` shows `run_id` on
  every step lifecycle event.
- **Closed:** 2026-04-25
