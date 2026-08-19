# AI-kernel validation report

A comprehensive, reproducible validation of the current `aik` system: automated verification,
manual CLI testing against a real Ollama server, end-to-end scenarios, and a security review.
Performed in one pass; every claim below comes from a command actually run or code actually
read during that pass, not from memory or inference. Raw session logs referenced by filename
were captured under `/tmp/aik-test/results/` during testing and are reproducible by rerunning
the commands shown.

## Environment

| | |
|---|---|
| OS | Arch Linux, kernel `7.1.8-zen1-3-zen` (x86_64) |
| Rust | `rustc 1.97.0` / `cargo 1.97.0` |
| Ollama | `0.32.9`, running as a systemd service, local (`http://localhost:11434`) |
| Models used | `llama3.1:8b` (primary — reliable structured tool calling), `qwen2.5-coder:7b` (used once to demonstrate a model that answers tool-shaped prompts in prose instead of issuing a real tool call), `gemma4-e4b-64k:latest` (surfaced once by the no-model-specified autodetect path) |
| Hardware | 16 logical CPUs, 30 GiB RAM |
| Repository | branch `main`, base commit `bae6bc0f10b95e8fa57cf1279f7a7337c389109b`, clean at the start of this review |

## Repository state at the end of this review

```
 M README.md
 M crates/cli/src/lib.rs
 M crates/cli/src/session.rs
 M crates/cli/tests/session.rs
?? docs/
```

Three source changes, all in `aik-cli`: two bug fixes (below) plus their regression tests, and
documentation. No changes to `aik-core`, `aik-api`, or any of the security-relevant crates
(`aik-tools`, `aik-policy`, `aik-fs`, `aik-approval`, `aik-context`) — everything found there
during review was either already correct or a documented limitation, not a defect.

## Automated verification

Commands run exactly as specified, before and after the two fixes below; both runs are
recorded because the second is the one that reflects the repository's current state.

| Command | Before fixes | After fixes |
|---|---|---|
| `cargo fmt --all --check` | clean | clean |
| `cargo check --workspace --all-targets` | clean | clean |
| `cargo test --workspace` | **567 passed, 0 failed** | **570 passed, 0 failed** (3 new: one for each fix's regression test, one for the fix that needed two assertions) |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | clean | clean |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features` | clean | clean |

No existing test was modified or weakened to make it pass. The test count increased by three:
`a_one_shot_run_that_fails_reports_the_error_to_its_caller` and two unit tests for the new
`report()` error-chain helper (`report_includes_only_the_context_when_there_is_no_source`,
`report_walks_the_full_chain_of_causes`).

## Manual CLI validation

Every row below was exercised against the real, compiled `target/debug/aik` binary and a real
Ollama server — not just inferred from the automated suite, though the automated suite's own
passing tests are cited where they cover the same guarantee more thoroughly than a single
manual run could (e.g. TOCTOU/confinement edge cases with concurrent filesystem mutation, which
are impractical to reproduce reliably by hand and are instead unit-tested in `aik-fs`).

### Basic operation

| Test | Command | Expected | Actual | Result |
|---|---|---|---|---|
| `--help` | `aik --help` | Documents every flag | All 11 options + APPROVALS note present | PASS |
| `--version` | `aik --version` | Prints `aik 0.1.0` | Exact match | PASS |
| One-shot, no tool call | `aik --no-tools "say PONG"` | Direct reply, exit 0 | `PONG`, exit 0 | PASS |
| One-shot, tool call | `aik -p allow-all.json "read test.txt..."` | Tool invoked, result summarised | Correct read + summary | PASS |
| Interactive multi-turn | piped 2 questions + `/session` + `/tools` | Same session id both turns, correct identity, tool listing | Confirmed via banner + `/session` output | PASS |
| `/new` | piped: state a fact, `/new`, ask about it | New session cannot see old one | Model correctly said "I don't know" | PASS |
| EOF without `/quit` | piped input with no trailing `/quit` | Clean exit, no hang | Exited 0 after last line processed | PASS |
| Blank line / unknown `/command` | piped `\n\n/bogus\n...` | Skipped without reaching the model | Confirmed — help text shown for `/bogus`, no model call | PASS |
| Idle Ctrl-C | not separately reproduced live (signal timing in a piped harness) | Ends session | Verified by code path inspection (`tokio::select!` biased on `ctrl_c()`); not independently re-executed | NOT INDEPENDENTLY VERIFIED |

### Model / provider

| Test | Command | Expected | Actual | Result |
|---|---|---|---|---|
| Valid model | `-m llama3.1:8b` | Answers normally | Confirmed throughout | PASS |
| Invalid model | `-m totally-not-a-real-model "hello"` | Clean error, non-zero exit | `aik: Ollama returned HTTP 404 Not Found`, exit 1 | PASS |
| Provider unreachable | `-c {"components":{"model":{"ollama":{"endpoint":"http://localhost:19999"}}}}` | Clean error, non-zero exit | `aik: sending a completion request to Ollama: ... Connection refused`, exit 1 | PASS (after fix #1; see below — before the fix this exited **0**) |
| Model without tool support | `qwen2.5-coder:7b`, tool-shaped prompt | Answers in prose, no structured call | Confirmed: model printed JSON as text content, 0 tool calls reported | PASS (correct — matches `aik-ollama`'s documented behaviour of reporting the provider's `tools` capability faithfully) |
| Response without tool calls | `--no-tools "say PONG"` | Plain content, `Finished` | Confirmed | PASS |
| Response with tool calls | see above | `ToolCall` → `ToolResult` → `Finished` | Confirmed, correct sequencing | PASS |
| Multiple tool calls in one run | "list, then read test.txt" | Two sequential invocations, one turn boundary between list and read | Confirmed via `-v`: list (with per-entry `DiscoveredResource` checks), then read | PASS |
| Tool call followed by normal response | throughout | Assistant text after the tool result | Confirmed throughout | PASS |
| Malformed provider response | not reproduced against a live server (would require a fault-injecting proxy) | — | Covered instead by `aik-ollama`'s own `wiremock`-based test suite (deterministic, no live server needed), which passed | COVERED BY AUTOMATED SUITE |

### Context

| Test | Expected | Actual | Result |
|---|---|---|---|
| Multi-turn context growth | Window tokens increase as more turns/results accumulate | Observed 48 → 124 → 228 tokens across 3 turns of a real conversation (Scenario G) | PASS |
| Elision on an oversized tool result | Bulk content replaced by a marker naming the record; full value stays retrievable | A 13.5 KB file read produced `1 elided` in `-v` output and a truncated, marked payload in the actual model request | PASS |
| Tool results entering context | Subsequent turns see prior tool output | Confirmed — model correctly referenced earlier `filesystem.list`/`.read` results in later turns | PASS |
| Conversation continues after a tool failure | Model gets a structured error, replies normally, turn count unaffected | Confirmed (`NotFound` on a nonexistent file, model then answered normally; ran to completion, exit 0) | PASS |
| Conversation continues after an authorization denial | Same | Confirmed (`PermissionDenied` from an empty/absent policy, model then answered normally) | PASS |
| Session/context attribution stays correct | A session belongs to the agent principal, not the user | Verified live for `/new` isolation; principal ownership itself verified by the automated suite (`the_transcript_belongs_to_the_agent_principal_not_the_user`, passed) | PASS |

### Filesystem

All of the following used a workspace containing a plain file, a nested directory, a symlink
(`escape-link`) pointing at a file outside the configured root, and a `secrets.txt` fixture.

| Test | Argument sent | Result | Verdict |
|---|---|---|---|
| Normal read | `test.txt` | Content returned correctly | PASS |
| Nested path | `data/notes.txt` | Content returned correctly | PASS |
| Root listing | `""` (omitted) | All entries listed, correct kinds | PASS |
| Nonexistent path | `does-not-exist.txt` | `NotFound`, structured, model-visible | PASS |
| Absolute path | `/etc/passwd` | `InvalidArgument: path must be relative...`, refused before any read | PASS — confinement held |
| `../` traversal | `../outside/secret.txt` | `InvalidArgument: ... no ., .., or root prefixes`, refused syntactically | PASS |
| Symlink pointing outside root | `escape-link` (read) | `InvalidArgument: path resolves outside the tool's allowed root` | PASS |
| Symlink pointing outside root | `escape-link` (write) | `InvalidArgument: the path's final component is a symlink; this tool never writes through one` | PASS |
| Directory where a file was expected | `subdir` (read) | `InvalidArgument: not a regular file` | PASS |
| `..` in a list request | `..` | Refused syntactically, same as read/write | PASS |
| Write, normal | valid relative path + content | File created, exact byte-for-byte content confirmed on disk | PASS |
| Write, path traversal | `../outside/pwned.txt` | Refused before any filesystem access | PASS |
| Special files | not separately fabricated (FIFO/device) | Covered by `aik-fs`'s own test suite (`check_target` rejects non-regular-file targets; `O_NONBLOCK` on `create_within` prevents a FIFO from blocking) | COVERED BY AUTOMATED SUITE |

### Policy

| Test | Result | Verdict |
|---|---|---|
| No policy | Banner warns explicitly; every tool call denied with `no policy rule matched this request` | PASS |
| Explicit allow (`resource: "*"`) | Grants both capability- and resource-level questions | PASS |
| Explicit deny | Refuses with the policy-authored reason | PASS |
| Resource-specific rule *without* a capability-level companion rule | **Everything denied, including resources the specific rule looks like it allows** — because the tool-phase (capability, no resource) question never matches a resource-scoped rule | CONFIRMED BEHAVIOUR, documented as the "two-phase gotcha" in `docs/CLI.md` — not a bug, but a sharp edge a first-time policy author (including this reviewer, on the first attempt) will hit |
| Resource-specific rule *with* a capability-level companion rule, specific-deny-before-general-allow | `secrets.txt` denied, `test.txt` allowed, in the same run | PASS — matches the README's documented example exactly |
| Same rules, order reversed | General allow now shadows the specific deny; `secrets.txt` becomes readable | PASS — confirms first-match-wins with no specificity heuristic, exactly as documented |
| Invalid policy (empty action pattern) | Startup fails, exit 1, names the exact rule and field (`rules[0].action`) | PASS |
| Malformed policy JSON | Startup fails, exit 1, names the file and the JSON error | PASS |
| Policy attempting to reach outside the filesystem root | No effect — the tool never produces an out-of-root canonical path to match against, so the rule is simply never consulted for it | PASS — covered live (absolute path, traversal, symlink all still refused under an allow-`"*"` policy) and by the automated suite's `a_permissive_policy_cannot_reach_outside_the_configured_root` |
| Policy/config precedence | `--policy` file overrides the `policy` key from `--config` | Confirmed by the settings unit tests (`a_policy_file_overrides_the_one_in_the_configuration`, passed) | PASS |

### Tool exposure

| Test | Result | Verdict |
|---|---|---|
| Default tool set (no flags) | `filesystem.read` + `filesystem.list` only | PASS |
| `--write` | Adds `filesystem.write`; verified the tool actually functions (file created and content confirmed on disk) | PASS |
| `--no-tools` | No tools offered; model given no schemas at all (window collapsed to ~19 tokens with no tool defs), no tool calls possible | PASS |
| `--no-tools --write` together | Rejected at argument-parsing time before startup | PASS |
| Unregistered/unavailable tool requested by the model | Never reaches `ToolRegistry` — the agent loop checks the run's fixed tool set before invoking anything and reports "no tool named ... is available" itself | PASS — confirmed by code inspection of `crates/agent/src/run.rs::call_tool`, and observed live (model attempting `filesystem.write` with only read tools registered got no real tool-call machinery to use at all) |
| CLI cannot widen the tool set beyond what was registered | Structural — the frontend holds no `ToolRegistry`, only a `dyn Agent`; there is no code path from CLI input to a wider tool set | PASS — verified by code inspection of `crates/cli/src/session.rs` and `crates/cli/src/wiring.rs` |

### Approval

| Test | Result | Verdict |
|---|---|---|
| Interactive `require_approval`, approve (`y`) | Tool runs, file written, content confirmed on disk | PASS |
| Interactive `require_approval`, deny (`n`) | Tool refused, no file created | PASS |
| Blank answer | Refused (no default-allow on empty input) | PASS |
| Gibberish answer (`sure ok`) | Refused | PASS |
| EOF while a question is outstanding | Treated as an explicit refusal, session exits cleanly, no hang | PASS |
| One-shot with `require_approval` policy | Refused immediately — `no approval responder is attached, so nobody can answer` — no timeout wait | PASS |
| Approval cannot bypass filesystem confinement | A `../` traversal target under a `require_approval` write policy, answered `y`, is still refused — the path is rejected while building the resource claim, a step that runs *before* any approval question is even asked | PASS |
| Approval cannot bypass policy | Structural — approval is one branch of `Decision`, reached only after the policy engine returns `RequireApproval`; there is no path from an approved question to a call the policy denied | PASS (by construction; also exercised live throughout) |
| Approval events correctly attributed | `AuthorizationDecided`/`ToolInvoked` carry principal, kind and `on_behalf_of`; confirmed by the automated audit-attribution tests (`tool_calls_are_attributed_to_the_agent_acting_for_the_user`, `delegated_authority_is_visible_to_policy_and_in_the_audit_trail`, both passed) | PASS — see also the verbose-mode display gap noted under Limitations |
| Approval timeout (default 120s) | Not reproduced live (would require a multi-minute wait against the default, and the CLI exposes no flag to shorten it) | COVERED BY AUTOMATED SUITE (`aik-approval`'s own `tokio::test(start_paused = true)` timeout tests, deterministic, passed) |
| Shutdown while an approval is pending | Not reproduced via a raw process kill; reproduced via the closest live equivalent — EOF arriving mid-question, which resolves it as a refusal rather than hanging | PASS (live, close variant) — the exact "kernel shutdown closes the broker" path is additionally covered by `aik-approval`'s own `closing_refuses_the_waiting_and_the_future` test and `ApprovalComponent::stop` calling `broker.close()`, both confirmed by code reading |

### Security

Each boundary below states whether it was demonstrated, and how.

| Boundary | Demonstrated? | How |
|---|---|---|
| Model-controlled text cannot inject terminal escape sequences | **Yes, live** | A file containing literal `ESC[2K` and bare `\r` bytes was read by the model and echoed; no raw control byte reached the terminal at any point (tool output, assistant paraphrase) — rendered as `\u{001b}` text instead |
| Tool arguments/results cannot corrupt the approval prompt | **Yes, live + automated** | `PendingApproval` never carries the tool's arguments at all (only action, resource, and the policy-authored prompt text); confirmed by `crates/cli/tests/*` sanitisation tests (`a_hostile_resource_path_cannot_repaint_the_prompt`, passed) and by reading the actual approval-question renderer |
| Model output cannot modify principal/session/correlation identity | **Yes, by construction + automated** | The CLI hardcodes `AgentRequest.context = Value::Null` and derives the execution context's principal solely from CLI settings, never from anything the model produced; `AgentLoop` sets only its own identity and the caller's session id as execution-context attributes. Confirmed by code reading and by `a_model_claiming_authority_has_no_effect_on_the_decision` (passed) |
| `AgentRequest` context cannot alter trusted policy attributes | **Yes, by construction** | Same mechanism as above — `AgentRequest::context` is never merged into `ExecutionContext::attributes`, documented as a deliberate security decision in `crates/agent/src/agent.rs` |
| Cross-session isolation | **Yes, live + automated** | `/new` demonstrated live; session ownership enforcement (`InMemoryContextStore::authorize`) covered by its own unit tests, all passed |
| Cross-principal isolation | **Automated only** | The CLI runs one principal per process with no persistence, so cross-principal access across *processes* is not independently observable at the CLI layer; the underlying guarantee is proven by `the_transcript_belongs_to_the_agent_principal_not_the_user` and `InMemoryContextStore`'s own tests, all passed |
| Delegated identity preserved | **Yes, live + automated** | Every banner and `/session` output showed `assistant acting for user`; automated coverage as above |
| Audit events contain expected principal/delegation info | **Yes, by inspection + automated** | Confirmed the event schema carries it (`AuthorizationDecided`, `ToolInvoked`); confirmed the CLI's own verbose renderer does *not* display it (a display gap, not an enforcement gap) |
| No tool can bypass `ToolRegistry` authorization | **Yes, by construction** | `Tool` is never exposed outside `aik-tools`; an agent only ever holds a `dyn ToolRegistry`. Verified by code reading across `aik-api::tool`, `aik-tools::registry`, `aik-agent::run` |
| Permissive policy cannot escape filesystem confinement | **Yes, live + automated** | An allow-`"*"` policy still refused an absolute path, a `../` traversal and a symlink escape, live; `a_permissive_policy_cannot_reach_outside_the_configured_root` passed |
| No-policy mode fails closed | **Yes, live** | Every tool call denied with an absent policy, banner warns explicitly at startup |

## End-to-end scenarios (A–H)

All eight scenarios were run against `llama3.1:8b`, a temporary directory outside any real
project, and synthetic data. Full transcripts are in
`/tmp/aik-test/results/scenarios.log`. Summary:

| Scenario | Result |
|---|---|
| A — list files, describe them | PASS. Note: the model interpreted "the test directory" as a literal subdirectory named `test` on the first attempt (got `NotFound`, reasonable given the ambiguous prompt) and correctly listed the current directory once re-asked more plainly. Not a harness issue. |
| B — read and summarise `test.txt` | PASS. Tool call and content correct; the model's own summary slightly miscounted sentences — a model-quality artefact, not a tool/harness fault. |
| C — create `output.txt` with known text | PASS. Byte-for-byte content confirmed on disk. |
| D — read a file outside the root | PASS. Refused with `InvalidArgument`. Notable: the model then fabricated a plausible-looking but entirely invented `/etc/passwd`-style example in its prose reply, unprompted — no real data was ever read or disclosed, but it is a reminder that assistant text following a refusal is not tool output and should not be trusted as such. |
| E — write requiring approval, approve | PASS. File created, correct content. |
| F — write requiring approval, deny | PASS. No file created. |
| G — several filesystem operations over multiple turns | PASS. Window tokens grew 48 → 124 → 228 across the three turns as prior results accumulated in context, all three operations (list, read, write) completed correctly. |
| H — an operation the model is not allowed to perform | PASS. With no `--write`, the model (reasonably) checked whether the target existed via `filesystem.read` rather than attempting a write it had no schema for, got `NotFound`, and reported the file didn't exist — a clean demonstration of a disallowed capability being entirely unreachable rather than attempted-and-refused. |

## Bugs found and fixed

### 1. One-shot runs always exited 0, even on a hard model-provider failure

**Symptom.** `aik --policy allow.json "..."` against an unreachable Ollama endpoint, or an
invalid model name, printed an error line and still exited 0.

**Root cause.** `Session::turn` caught every model/stream error internally
(`Step::Update(Some(Err(error)))`), printed it, and returned `Ok(())` unconditionally. This is
correct for *interactive* mode (a failed turn should not kill the session — and the interactive
call site already has its own, until-now-unreachable, catch-and-continue handling for exactly
this). `Session::one_shot` calls the same `turn()`, but has no next prompt to continue to, so it
silently absorbed the failure too, leaving its caller (`run()`, then `main()`, then the process
exit code) with no way to distinguish "the model answered" from "nothing happened at all."

**Fix.** `turn()` now returns the error instead of printing and swallowing it
(`crates/cli/src/session.rs`). The interactive call site is unaffected — its pre-existing
`if let Err(error) = self.turn(...).await { println!("  error: {error}"); }` now actually
executes, producing the same visible output as before. `one_shot()` now propagates the error,
so `main()`'s existing error-reporting branch (exit code 1) fires correctly.

**Regression test.** `a_one_shot_run_that_fails_reports_the_error_to_its_caller` in
`crates/cli/tests/session.rs`.

### 2. Printed CLI errors discarded their root cause

**Symptom.** `aik: sending a completion request to Ollama` — no indication of *why*
(connection refused, DNS failure, TLS error, and a dozen other causes all print identically).

**Root cause.** `aik_core::Error`'s `Display` deliberately prints only its own context string
(confirmed intentional by `aik-core`'s own `wrapped_errors_keep_their_source` test, which
explicitly asserts `to_string()` excludes the source) — the underlying cause is meant to be
reachable through `std::error::Error::source()` instead. `aik`'s top-level error reporting
(`main()`) printed `{error}` directly, never walking that chain, so the deliberately-preserved
detail never reached the person running the command.

**Fix.** Added `report()` in `crates/cli/src/lib.rs`, which walks `.source()` and appends each
cause; both of `main()`'s error-printing sites use it now. `aik-core::Error`'s own `Display`
behaviour is untouched — this is purely how the CLI consumes an already-correct error chain.

**Regression tests.** `report_includes_only_the_context_when_there_is_no_source`,
`report_walks_the_full_chain_of_causes` in `crates/cli/src/lib.rs`.

Both fixes were verified against the real, compiled binary (not only their unit tests) before
and after: the unreachable-provider scenario now prints
`aik: sending a completion request to Ollama: error sending request for url (...): tcp connect
error: Connection refused (os error 111)` and exits 1, where it previously printed a truncated
message and exited 0.

## Limitations (not bugs)

See `docs/CLI.md`'s [Known limitations](CLI.md#other-known-limitations-not-bugs) for the full
list with detail. Summary: no persistence (in-memory context store and approval broker), no
summarisation/semantic memory, a heuristic (not model-accurate) token counter, verbose mode
omits principal/delegation attribution from its text rendering even though the underlying audit
events carry it correctly, and the filesystem TOCTOU window is narrowed but not fully closed —
all four are documented as deliberate in the code itself, not gaps found during this review.

One additional observation, not a limitation of the system but of models run against it: a
model will sometimes fabricate a plausible-looking answer after a tool call is correctly
refused (Scenario D). The refusal itself is sound and no data was disclosed; this is a reminder
for anyone building on top of this CLI that assistant prose is not tool output and must not be
treated as such by anything downstream.

## Token/context baseline

See `docs/CLI.md`'s [Token and context cost](CLI.md#token-and-context-cost-a-baseline) section
for the full measurements and methodology. Headline numbers, controlled (identical prompt,
identical conversation content, single turn, `llama3.1:8b`):

| Tools registered | Ollama-reported input tokens |
|---|---|
| none | 23 |
| read + list (2 schemas) | 398 |
| read + list + write (3 schemas) | 543 |

Tool schemas dominate the request size of a short conversation by more than an order of
magnitude, this cost is invisible to the CLI's own `[ctx]` window-token accounting (which only
reflects `ContextStore` records), and it — along with the system prompt — is resent in full on
every single turn with no caching, since neither the `ModelProvider` contract nor Ollama's
`/api/chat` endpoint carries any session concept. No optimisation was implemented; this is
strictly a baseline for future work, per the explicit scope of this review.

## Final test count

**570 tests passing, 0 failing**, across the whole workspace (unit, integration and doc tests
combined), after the two fixes above. 567 before them. `cargo fmt`, `cargo check`, `cargo
clippy -D warnings` and `cargo doc -D warnings` all clean, before and after.
