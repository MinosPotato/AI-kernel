# Measuring `aik`: a token/context/latency baseline

This document is the companion to [`docs/CLI.md`](CLI.md)'s existing
["Token and context cost"](CLI.md#token-and-context-cost-a-baseline) section, and
supersedes it as the canonical baseline: everything below was re-measured against the
current repository, with a machine-readable recording mechanism added specifically to make
the numbers reproducible rather than transcribed from a terminal by hand.

**Scope.** This is a measurement pass, not an optimisation pass. Nothing here changes
authorization, policy evaluation, approval, tool execution, context assembly or model
behaviour. See [Phase 10 / strict scope control](#recommended-next-step) at the end for what
was deliberately left undone.

## What is measured

| Quantity | Where it comes from | Confidence |
|---|---|---|
| Provider-reported input tokens | `CompletionResponse::usage` / `CompletionChunk::Done::usage`, from Ollama's `prompt_eval_count` | **Provider-reported (exact, as reported)** |
| Provider-reported output tokens | Same, from `eval_count` | **Provider-reported (exact, as reported)** |
| Tool-definition tokens | `aik_api::measurement::RequestEstimate::tool_definition_tokens`, computed from the exact `ToolDefinition`s attached to the request | **Locally estimated** |
| System/instruction tokens | `RequestEstimate::system_tokens`, from pinned `Role::System` messages in the window actually sent | **Locally estimated** |
| Conversation tokens | `RequestEstimate::conversation_tokens`, everything else in the window | **Locally estimated** |
| Current-turn user input tokens | `RequestEstimate::user_input_tokens` — `Some` only on the turn fresh input was appended | **Locally estimated** |
| Tool-call / tool-result tokens | `RequestEstimate::tool_call_tokens` / `tool_result_tokens`, a breakdown of conversation tokens | **Locally estimated** |
| Context-store accounting (stored/included/elided/evicted) | `ContextUsage`, published on `ContextAssembled` | **Locally exact** — an exact count under the store's own `TokenCounter`, not the provider's tokenizer |
| Model-turn count, tool-call count | `RequestMeasured::turn`, `cumulative_tool_calls` | **Locally exact** |
| Model latency | `RequestMeasured::model_latency_ms`, wall-clock around `ModelProvider::complete` | **Locally exact** (wall-clock), not provider-reported — no provider in this codebase reports its own processing time |
| Tool latency (execution only) | `ToolInvoked::execution_duration_ms` | **Locally exact** (wall-clock); can include mid-execution `DiscoveredResource` authorization — see the field's own doc comment |
| Authorization latency | `AuthorizationDecided::duration_ms`, `ToolInvoked::authorization_duration_ms` | **Locally exact** (wall-clock); includes any approval wait, since that wait is part of resolving the decision |
| Approval latency | `AuthorizationDecided::approval_wait_ms` — `Some` only for a decision that actually asked an [`ApprovalSink`], `None` otherwise | **Locally exact** (wall-clock), isolated from the rest of `duration_ms` specifically because a policy check is sub-millisecond and an approval wait is a human being asked a question; the two have different distributions and summing them into one number was misleading |
| Cumulative run cost | `SessionStats` (`crates/cli/src/render.rs`), folded from the events above across every prompt in a session | **Locally exact for estimates and latency; exact-as-reported for provider tokens, when the provider reports them** |

**Why some things cannot be exact.** Ollama's `/api/chat` is the only provider in this
workspace, and it reports token counts computed by its own tokenizer — those numbers, when
present, are treated as ground truth and are never second-guessed. Everything this document
calls "estimated" is produced by `aik_api::context::TokenCounter`, which the kernel
deliberately keeps to a documented byte-length heuristic (`HeuristicTokenCounter`, 4 bytes
per token by default) rather than acquiring a real tokenizer — see
`crates/context/src/tokens.rs`. The two numbers are not interchangeable, and this document
never presents one as the other. See the [tool-count comparison](#tool-count-comparison)
below for how far apart they can be.

## Architecture: why this shape

The measurement layer is **one dedicated event, plus latency fields added to the two audit
events that already existed** — not a new subsystem, not a second way of measuring
anything.

* [`aik_api::measurement::RequestMeasured`] is published once per model turn, by
  `aik-agent`'s `Run::turn`, on the same `EventBus` the kernel already uses for
  `ContextAssembled`, `AuthorizationDecided` and `ToolInvoked`. It exists because neither of
  those events can see tool-definition cost or provider usage/latency — see the event's own
  module documentation for why that is structural, not an oversight.
* `AuthorizationDecided::duration_ms` and `ToolInvoked::{duration_ms,
  authorization_duration_ms, execution_duration_ms}` are additive fields on events that
  already existed, populated with `std::time::Instant` measurements taken around code that
  was already there — no control flow changed, only the fields on what was already emitted.
* The CLI's `-v`/`--verbose` output and a new `--record <FILE>` JSONL sink are both
  **subscribers** to these events, exactly like the pre-existing verbose renderer was. See
  `crates/cli/src/render.rs` and `crates/cli/src/recorder.rs`.

This was chosen over a scattered "measure it inline everywhere" approach because every
number above was already computable at a single point trusted code already passes through
(the agent loop's turn boundary, the tool registry's invoke boundary) — adding an event
there is strictly additive, and a test that asserts the model's request/response contents
are unaffected by whether anyone is listening
(`measurement_does_not_change_what_the_model_is_sent_or_what_runs`, `crates/cli/tests/measurement.rs`)
is the direct evidence that it stayed that way.

## Privacy: what is and is not recorded

`--record <FILE>` appends one JSON object per line. See `crates/cli/src/recorder.rs`'s
module documentation for the authoritative list; summarised:

**Recorded:** timestamps, session/correlation ids, turn numbers, event kind, token
estimates, provider-reported usage, latencies in milliseconds, model id, tool name,
authorization phase, authorization decision (as a tag, e.g. `"denied"`), and an error kind
where one applies.

**Never recorded:** prompts, assistant text, tool arguments, tool results, file contents,
resource identifiers (paths — present on the underlying audit events but deliberately
excluded here, since a path can encode a username or project layout), and policy-authored
deny/approval reasons (short human-authored text, excluded for the same reason paths are —
a reason can quote the resource it is about).

This is tested directly: `recorder::tests::recorded_*` assert the absence of `message`,
`content`, `resource` and `reason` keys, and
`a_recorder_writes_one_line_per_measured_turn_and_no_message_content`
(`crates/cli/tests/measurement.rs`) asserts that a known-secret substring placed in a real
prompt/response never appears anywhere in the recorded file.

A recorder that cannot open its destination fails at startup, named, like any other startup
error. A write that fails mid-run prints one message and disables itself for the rest of
the process rather than retrying forever or claiming success it did not have — see
`Recorder::write`.

## The verbose (`-v`) output

Per turn:

```
  [ctx]  stored 9236 — included 247 (3 records), elided 8989 (1 parts), evicted 0 (0 records)
  [req]  turn 2 — system 0, tools 301 (2 offered), conversation 247, total 548 (estimated)
  [req]  provider usage: 133 in / 114 out (exact, as reported)
  [req]  model latency: 1593ms
  [auth] Resource filesystem.write on ... → ApprovalGranted (118ms)
  [tool] filesystem.write → Succeeded (4ms exec, 118ms auth)
```

End of turn / end of session:

```
  [2 turns, 1 tool calls, 530 in / 319 out tokens, window 247 tokens]
  [session] 2 turns, 1 tool calls, 866 estimated tokens total, provider 530 in / 319 out (exact)
  [session] latency — model 4307ms, tools 0ms, authorization 0ms (approval 0ms)
```

Every number that is an estimate is labelled `(estimated)`; every number taken from the
provider is labelled `(exact, as reported)`. Nothing prints an estimate as if it were a
provider figure. See `render::measurement`, `render::assembled`, `render::session_totals`.

## Benchmark scenarios (A–H)

All scenarios were run against a real local Ollama server (`0.32.9`) and `llama3.1:8b`, in
`/tmp/aik-bench/ws` — a scratch directory outside the repository, containing only synthetic
files created for this pass — using `--policy` documents that also live outside the
repository. Every command below is reproducible as shown; only the temporary workspace path
needs to exist and be writable.

```bash
cargo build -p aik-cli
BIN=target/debug/aik
mkdir -p /tmp/aik-bench/ws && cd /tmp/aik-bench/ws
```

| Scenario | Command (abbreviated) | Result |
|---|---|---|
| A — no tools | `$BIN -m llama3.1:8b --no-tools -v -R a.jsonl "Say PONG and nothing else."` | 1 turn, 17 in / 3 out (provider, exact), 11 estimated conversation tokens, 0 tool-definition tokens, 2387ms model latency |
| B — one tool | built via a throwaway harness registering only `filesystem.read` (the CLI's own flags cannot register exactly one; see [note](#a-note-on-the-01-2-3-comparison)) | turn 1: 17→228 provider input tokens once one tool is offered; 122 estimated tool-definition tokens |
| C — multiple tools (read + list, the CLI default; read + list + write with `--write`) | `$BIN -m llama3.1:8b -p allow-all.json -v -R c.jsonl "..."` | 2 tools: 301 estimated / ~375 provider tool-definition tokens; 3 tools: 469 estimated / ~520 provider tool-definition tokens |
| D — repeated tool usage | 3 sequential `list`/`read`/`read` prompts in one session | included-window tokens grew **172 → 210 → 244** across turns in one prompt, and cumulative session provider usage grew to 2263 in / 152 out across 6 turns / 3 tool calls |
| E — large tool result | a synthetic 36,033-byte file, read and asked to be summarised | `[ctx] stored 9236 — included 247 (3 records), elided 8989 (1 parts), evicted 0` — elision alone reduced this turn's context cost by >97% |
| F — long conversation | 14 turns, each injecting ~690 estimated tokens of fixed user text (short enough to stay under the 1,024-token/part elision limit) | included tokens climbed **689 → 1457 → … → 7812** (10 turns) before eviction began; from turn 11 onward, `included` stabilised at **~7950–8000** while `evicted` grew **689 → 1457 → 2281 → 3122** tokens (1, 3, 5, 7 records) |
| G — approval | `--write` + a `require_approval` write policy, answered `y` | tool ran, file written; `AuthorizationDecided.approval_wait_ms` for the `ApprovalGranted` decision was ~0ms in this pass because the scripted `y` answer arrived immediately — see [limitations](#limitations) |
| H — failure paths | policy denial, `NotFound` read, unreachable provider (`http://localhost:19999`) | denial: reported to the model, run continues, audited as `Denied`; not-found read: reported to the model as `{"kind":"notfound",...}`, run continues; unreachable provider: one-shot run exits **1**, and **no `RequestMeasured` event is published for the failed turn** — a call that never returned has nothing to measure, and the recorder correctly shows a `context_assembled` line with no matching `request_measured` line for it |

### A note on the 0/1/2/3 comparison

The CLI's own `--write`/`--no-tools` flags only ever produce **0, 2, or 3** registered
tools (none; `filesystem.read` + `filesystem.list`; all three) — there is no flag to
register exactly one. To get the requested 0/1/2/3 comparison, a throwaway program was built
against the library crates directly (`aik-core`, `aik-api`, `aik-agent`, `aik-tools`,
`aik-fs`, `aik-context`, `aik-ollama` — all public APIs, no source changes), registering
exactly `N` filesystem tools and printing the `RequestMeasured` events for one turn. It was
not added to the repository or committed; the measurements it produced are the ones in the
table below. This is the only place in this pass where "reproduce the benchmark" requires
more than the `aik` binary — everything else uses `aik` exactly as shipped.

## The important comparison: tool-count overhead

Turn-1 provider-reported input tokens for an identical one-line prompt (`llama3.1:8b`):

| Tools registered | Provider input tokens (exact) | Estimated tool-definition tokens (local) |
|---|---|---|
| 0 | 17 | 0 |
| 1 (`filesystem.read`) | 228 | 122 |
| 2 (`filesystem.read` + `filesystem.list`) | 392 | 301 |
| 3 (`filesystem.read` + `filesystem.list` + `filesystem.write`) | 537 | 469 |

Marginal cost per tool schema, from the provider's own count: **+211, +164, +145 tokens**
for the first, second and third tool respectively — each one alone costing roughly ten to
twenty times the 11–17-token conversation body it rides alongside. The local estimate
under-counts this consistently (122 vs. 211 for the first tool, for example): the
byte-length heuristic is a reasonable *relative* signal — it correctly ranks 0 < 1 < 2 < 3
tools and grows roughly in step with the provider's own count — but it is not, and does not
claim to be, the provider's real count. This is exactly the distinction
[`RequestEstimate`]'s documentation makes explicit.

## Context cost: short / medium / long

Using the same [long-conversation run](#benchmark-scenarios-a–h) (scenario F), read as
three points along one growth curve rather than three separate runs:

| | Turns | Included tokens | Elided tokens | Evicted tokens |
|---|---|---|---|---|
| Short | 1 | 689 | 0 | 0 |
| Medium | 5 | 3,859 | 0 | 0 |
| Long | 14 | ~7,970 (stabilised) | 0 | 3,122 (7 records) |

This demonstrates the two independent mechanisms `aik-context` implements, cleanly
separated by workload shape:

* **Elision** dominates when a single record is oversized (scenario E: one 36 KB file
  read, 8,989 of 9,236 tokens elided, nothing evicted).
* **Eviction** dominates when many *individually modest* records accumulate past the
  window budget (scenario F: nothing ever exceeded the 1,024-token/part elision limit, so
  eviction — not elision — is what keeps the window at the configured 8,192-token ceiling).

## Token baseline (summary)

| | Value | Confidence |
|---|---|---|
| Fixed request overhead (0 tools, 1-line prompt) | 17 tokens in / 3 out (provider) | Provider-reported |
| Tool-definition overhead, 1st tool | +211 tokens (provider); 122 (local estimate) | Provider-reported / locally estimated |
| Tool-definition overhead, 2nd tool | +164 tokens (provider); +179 (local estimate) | Provider-reported / locally estimated |
| Tool-definition overhead, 3rd tool | +145 tokens (provider); +168 (local estimate) | Provider-reported / locally estimated |
| Context cost, short conversation | 689 estimated tokens (1 turn) | Locally exact (store accounting) |
| Context cost, long conversation, at steady state | ~7,970–8,000 estimated tokens (budget-bound) | Locally exact |
| Tool-result cost before elision | up to the full record size (36 KB / 9,236 tokens in scenario E) | Locally exact |
| Tool-result cost after elision | reduced to a small marker + surrounding structure (247 tokens in scenario E) | Locally exact |
| Output cost | model- and prompt-dependent; 3–319 output tokens observed across scenarios | Provider-reported |
| Cumulative run cost (scenario D, 3 prompts / 6 turns) | 2,263 in / 152 out provider tokens; 1,591 estimated tokens (session running total from `SessionStats`) | Provider-reported / locally estimated |
| Model latency | 186ms–2,714ms observed, `llama3.1:8b`, local Ollama | Locally exact (wall-clock) |
| Tool (execution) latency | 0ms observed for in-memory filesystem tools against a warm page cache | Locally exact (wall-clock) |
| Authorization latency | 0ms observed for the in-memory `RuleBasedPolicyEngine` | Locally exact (wall-clock) |
| Approval latency | ~0ms observed *in this scripted pass* because the answer was piped instantly | Locally exact, but not representative of a human's real response time — see [Limitations](#limitations) |

## Reproducing these measurements

```bash
cargo build -p aik-cli
mkdir -p /tmp/aik-bench/ws && cd /tmp/aik-bench/ws
echo "The project codename is Halibut. It was created in 2024." > notes.txt

cat > /tmp/aik-bench/allow-all.json << 'EOF'
{ "rules": [
    { "action": "filesystem.read", "effect": { "decision": "allow" } },
    { "action": "filesystem.list", "effect": { "decision": "allow" } },
    { "action": "filesystem.write", "effect": { "decision": "allow" } },
    { "action": "filesystem.read", "resource": "*", "effect": { "decision": "allow" } },
    { "action": "filesystem.list", "resource": "*", "effect": { "decision": "allow" } },
    { "action": "filesystem.write", "resource": "*", "effect": { "decision": "allow" } }
] }
EOF

BIN=/path/to/target/debug/aik

# Scenario A — no tools
$BIN -m llama3.1:8b --no-tools -v -R /tmp/aik-bench/a.jsonl "Say PONG and nothing else."

# Scenario C — multiple tools (2 by default, 3 with --write)
$BIN -m llama3.1:8b -p /tmp/aik-bench/allow-all.json -v -R /tmp/aik-bench/c.jsonl "what is in notes.txt?"
$BIN -m llama3.1:8b -p /tmp/aik-bench/allow-all.json --write -v -R /tmp/aik-bench/c3.jsonl "what is in notes.txt?"

# Scenario E — large tool result / elision
python3 -c "print(('The quick brown fox jumps over the lazy dog. ' * 800))" > big.txt
printf 'read big.txt and tell me how many words are in it\n/quit\n' | \
  $BIN -m llama3.1:8b -p /tmp/aik-bench/allow-all.json -v -R /tmp/aik-bench/e.jsonl

# Scenario F — long conversation / eviction
python3 -c "
para = ('Context filler sentence to pad the conversation. ' * 55)
for i in range(1, 15):
    print(f'{para} This is turn {i}: what number is this turn?')
print('/quit')
" | $BIN -m llama3.1:8b --no-tools -v -R /tmp/aik-bench/f.jsonl
```

Inspect `*.jsonl` for the structured form of everything `-v` printed.

## Limitations

* **The approval-latency numbers in this pass are not representative of a real human.**
  Every approval scenario here answered `y` via a piped script, so
  `AuthorizationDecided.duration_ms` for those decisions measures scheduling/IO overhead,
  not thinking time. The mechanism is correct — it is simply measuring a script, not a
  person. A genuinely human-timed run is the natural follow-up, requires nothing further
  from this implementation, and just needs someone at a real terminal.
* **Model variability was not statistically characterised.** Latency and even
  tool-calling behaviour (see scenario E, where the model occasionally narrates fictitious
  tool calls in prose rather than issuing real ones) vary between runs and models; the
  numbers above are single observations, not means over repeated trials, per the time
  available for this pass. `docs/CLI.md`'s own baseline notes the same limitation.
* **`execution_duration_ms` can include mid-execution authorization time.** A tool such as
  `filesystem.list` asks a `DiscoveredResource` question per entry *while it runs*, which
  is structurally part of execution rather than the up-front authorization phase — see
  `ToolInvoked::execution_duration_ms`'s own doc comment.
* **The 0/1/2/3 tool-count comparison could not be produced through the shipped CLI
  alone**, since its flags only reach 0, 2 or 3 tools — see the
  [note above](#a-note-on-the-01-2-3-comparison).
* **The heuristic token counter is not a real tokenizer**, by design (see
  `crates/context/src/tokens.rs`) — every number in this document labelled "estimated"
  should be read as a consistent, monotonic, but not billing-accurate figure.
* **An observation, not a bug found by this pass:** `ToolInvoked.outcome` is
  `InvocationOutcome::Denied` for *any* error `InProcessToolRegistry::authorize` returns —
  including a `Tool::planned_resources` failure that has nothing to do with a policy
  decision, such as reading a file that does not exist (`resolve_within` cannot canonicalise
  a path that is not there, so the read tool's own resource-claim construction fails before
  any policy question is even asked). This was true before this pass — the `if let
  Err(error) = self.authorize(...)` branch that produces it predates this work — and
  `duration_ms`/`authorization_duration_ms` are correctly populated for it regardless of
  what the outcome label means; it is noted here because it was visible in scenario H's
  audit trail and is worth knowing when reading `ToolInvoked` events, not because it affects
  anything this pass implemented.

## Recommended next step

Based only on the measurements collected in this pass: **tool-definition resend is the
single highest-value target.** It is fixed cost — 122–469 estimated tokens (211–537 tokens
by the provider's own count) — added to *every single turn* of *every conversation*,
whether or not the model ever calls a tool, and it currently has no caching or
deduplication of any kind (`ModelProvider::complete` takes a fresh `CompletionRequest`
every call, and Ollama's `/api/chat` is stateless). By contrast, tool-*result* growth
(scenario D/F) is already bounded by the existing elision/eviction mechanism, and the
fixed 0-tool conversation overhead (17 tokens) is negligible next to either. No optimisation
for this was implemented as part of this pass, per its explicit scope — this is a
measurement-driven recommendation for what to investigate next, not an instruction to
implement it here.
