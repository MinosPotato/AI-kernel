# AI-kernel

The core of a long-term AI operating layer — the small, permanent foundation that agents,
model providers, tools, memory, permissions, scheduling, desktop integration and every
frontend are built on.

At its center is the kernel (`aik-core`, `aik-api`) — not an LLM wrapper: it knows nothing
about models, agents, tools, storage, a UI or the operating system, only the mechanisms such
things need in order to coexist. Everything that does know about those things — a real
model provider, real tools, an agent loop, a terminal frontend — lives downstream of the
kernel, each in its own crate, listed below.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design and the reasoning behind it.

## Layout

| Crate | What it is |
|---|---|
| [`aik-core`](crates/core) | The kernel: lifecycle, events, registry, tasks, config, plugins |
| [`aik-api`](crates/api) | Contracts for future subsystems. Traits and types only, no implementations |
| [`aik`](crates/aik) | Facade re-exporting both |
| [`aik-ollama`](crates/ollama) | A `ModelProvider` backed by a local or remote Ollama server |
| [`aik-tools`](crates/tools) | The authorization-gated `ToolRegistry` implementation |
| [`aik-policy`](crates/policy) | A deterministic, configuration-driven `PolicyEngine` |
| [`aik-fs`](crates/fs) | Filesystem `Tool`s — read and write — each confined to a configured root |
| [`aik-exec`](crates/exec) | Running allowlisted programs behind an OS-level sandbox |
| [`aik-approval`](crates/approval) | A human-in-the-loop `ApprovalSink`, answered by a frontend |
| [`aik-context`](crates/context) | An agent's transcript, and the budgeted model window derived from it |
| [`aik-store`](crates/store) | The one embedded database the durable subsystems share |
| [`aik-memory`](crates/memory) | Records an agent keeps between conversations, and the tools onto them |
| [`aik-scheduler`](crates/scheduler) | Time- and event-triggered jobs, persistent across restarts |
| [`aik-audit`](crates/audit) | The durable, append-only trail of authorization decisions and tool calls |
| [`aik-agent`](crates/agent) | The agent loop: model turns, bounded context, authorization-gated tool calls |
| [`aik-runtime`](crates/runtime) | System assembly: one description of a deployment, wired into a kernel |
| [`aik-ipc`](crates/ipc) | The authenticated local protocol between the host process and its clients |
| [`aik-daemon`](crates/daemon) | The host process (`aikd`): one owner of the database, running the schedule |
| [`aik-cli`](crates/cli) | The terminal frontend: a conversation, streamed updates, interactive approvals |

`aik-core` does not depend on `aik-api`, and neither depends on any of the subsystem
crates. A kernel can be built and run with none of the subsystem contracts present, no
model provider, no tool registry, and no policy engine at all. Only `aik-runtime` depends on
all of them, because assembling them is the whole of what it does; `aik-cli` and `aik-daemon`
depend on it rather than on the subsystems, so the terminal and the host process assemble the
same system by construction rather than by agreement.

## Two processes

redb hands the database to exactly one process, and a schedule needs something that is always
there to run it. So there is a host process, and everything else talks to it:

```bash
aikd --policy policy.json                 # holds the database, runs the schedule
aik --socket "$XDG_RUNTIME_DIR/aik/aikd.sock"          # a conversation, through the host
aik audit --socket "$XDG_RUNTIME_DIR/aik/aikd.sock"    # the trail, through the host
```

The socket is mode `0600` in a directory mode `0700`, the peer's account is checked by the
kernel before a byte of the protocol is read, and a per-instance token is written beside the
socket. No request carries a principal, a tool name or a policy: the host derives the one
identity in play from the connection it authenticated. There is no network listener, and
adding one needs a transport identity that is not a uid — see [`aik-ipc`](crates/ipc).

With no host running, `aik` assembles its own kernel exactly as before.

## The mechanisms

| Mechanism | Type | Purpose |
|---|---|---|
| Configuration | `Config` | Layered, immutable, format-agnostic settings |
| Wiring | `Registry` | Capability → implementation, resolved at runtime |
| Lifecycle | `Component`, `Kernel` | Dependency-ordered startup, rollback and shutdown |
| Communication | `EventBus` | Typed pub/sub, plus a JSON firehose for out-of-process bridges |
| Concurrency | `Tasks` | Hierarchical cancellation scopes and tracked background work |
| Extensibility | `Plugin` | Bundles of components, with an ABI version for future dynamic loading |

## Try it

A miniature system — a service provider, a consumer that resolves it by capability, and a
bridge that observes everything as JSON without knowing a single event type:

```bash
cargo run -p aik --example minimal
```

```
start order: [ComponentId("demo.bridge"), ComponentId("demo.sensor"), ComponentId("demo.analyser")]
[analyser] first direct read: 0
[bridge]   kernel.state_changed {"state":"running"}
[bridge]   demo.reading {"value":1}
[analyser] reading 1 from `demo.sensor`
...
[sensor]   loop stopped cleanly
```

## Writing a component

```rust,ignore
use aik::prelude::*;
use std::sync::Arc;

trait Notifier: Send + Sync {
    fn notify(&self, message: &str);
}

struct DesktopNotifier;

#[async_trait]
impl Component for DesktopNotifier {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new("platform.notifier")
            .requires("platform.hyprland")
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        ctx.provide_default::<dyn Notifier>(Arc::new(DesktopNotifier))
    }

    async fn start(&self, ctx: &ComponentContext) -> Result<()> {
        ctx.tasks().spawn_cancellable("watch", |token| async move {
            token.cancelled().await;
        });
        Ok(())
    }
}
```

Consumers never name the implementation:

```rust,ignore
let notifier = ctx.service::<dyn Notifier>()?;
```

That indirection is the point: replacing a desktop backend, a model provider or a memory
store is a change to which component is registered, never a change to the kernel.

## Talking to a real model

[`aik-ollama`](crates/ollama) is the first real `ModelProvider`: a kernel component that
talks to [Ollama](https://ollama.com) over HTTP, with streaming, cancellation, timeouts and
tool calling. Nothing about HTTP or Ollama's wire format leaves that crate — consumers
depend on `dyn ModelProvider`, resolved through the registry, exactly like the `Notifier`
example above.

```bash
cargo run -p aik-ollama --example chat
cargo run -p aik-ollama --example chat -- mistral "what is a kernel?"
```

Requires a running `ollama serve` with a model pulled; if it is not reachable, the example
prints a clear explanation and exits cleanly instead of failing loudly. The crate's own test
suite (`cargo test -p aik-ollama`) needs no such server — it runs against a mocked HTTP
server, deterministically.

### Letting a model ask for a tool

A provider that cannot carry tool calls makes the agent loop a chat box: the loop offers
tools every turn, and every turn comes back as prose. So `CompletionRequest::tools` carries
a `ToolDefinition` — a name, a description and an input schema — and the provider translates
that, the `tool_calls` that come back, and the results that go in reply.

```bash
cargo run -p aik-ollama --example tools
cargo run -p aik-ollama --example tools -- qwen3:8b
```

A `ToolDefinition` is deliberately *not* a `ToolSpec`. A spec also says which permissions a
tool requires, and that is the tool registry's business, not the model's: telling a model
which capability names exist is telling it what is worth asking for, and it would travel to
whatever server the provider talks to. Passing a smaller type makes that structural rather
than a rule each provider has to remember.

Nothing about offering a tool authorizes it. A call arriving from a model is a request, and
it goes through `ToolRegistry::invoke` — policy, resource checks, approval, audit — exactly
like a call from anywhere else. Ollama reports a `tools` capability per model; one without
it will answer in prose, which is a model's choice rather than a provider failure.

## Authorizing and touching real files

[`aik-tools`](crates/tools) is the reference `ToolRegistry`: an agent is only ever given a
handle to it, never to a `Tool` directly, so it is the one place authorization can be
enforced and audited. [`aik-policy`](crates/policy) is a rule-based `PolicyEngine` for it —
a first-match-wins list of principal/action/resource rules, read from the kernel's existing
`Config` mechanism rather than a bespoke file format. [`aik-fs`](crates/fs) is where that
meets the host system: a read tool and a write tool, each confined to a configured root,
independently of whatever policy allows.

```rust,ignore
let policy = RuleBasedPolicyEngine::from_config(&config, "policy")?;

let kernel = Kernel::builder()
    .component(
        ToolsComponent::new()
            .with_tool(FsReadTool::new("/home/user/project")?)
            .with_tool(FsWriteTool::new("/home/user/project")?)
            .with_policy(Arc::new(policy)),
    )
    .build()?;
```

A policy document is JSON, evaluated top to bottom, first match wins:

```json
{ "rules": [
    { "action": "filesystem.read", "resource": "/home/user/project/secrets*",
      "effect": { "decision": "deny", "reason": "contains credentials" } },
    { "action": "filesystem.read", "resource": "/home/user/project/*",
      "effect": { "decision": "allow" } },
    { "action": "filesystem.write", "resource": "/home/user/project/vendor/*",
      "effect": { "decision": "deny", "reason": "vendored code is not editable" } },
    { "action": "filesystem.write", "resource": "/home/user/project/*",
      "effect": { "decision": "require_approval", "prompt": "let the agent edit this file?" } }
] }
```

Reading and writing are separate tools requiring separate permissions, so holding one never
implies the other: an agent given `filesystem.read` and not `filesystem.write` cannot change
anything, and the write tool can simply not be registered at all.

Authorization narrows what a tool will do; it can never widen it. Both tools resolve and
confine every path to their own configured root before policy is even consulted, so a
permissive or misconfigured policy cannot make them touch anything outside it. The write
path goes further, because a misdirected write cannot be undone: the target directory is
resolved, opened, verified against the root and then written *through that handle*, and the
final path segment is never followed, so the path policy authorized and the file that
receives the bytes are the same object. See the `aik_api::tool` module docs,
[`FsReadTool`](crates/fs/src/tool.rs) and [`FsWriteTool`](crates/fs/src/write.rs) for the
full authorization flow, the time-of-check-to-time-of-use discussion, and exactly what
confinement does and does not guarantee against a concurrent, adversarial filesystem.

## Asking a human

`require_approval` is only as good as the thing that answers it.
[`aik-approval`](crates/approval) is that thing: an `ApprovalBroker` parks the question and
waits, and a frontend takes it off an `ApprovalGate` and answers. No terminal, socket or
window appears anywhere in the kernel — the broker is a rendezvous, so a CLI prompt, a
desktop popup and a chat bridge are all the same shape.

```rust,ignore
let broker = Arc::new(ApprovalBroker::new());

let kernel = Kernel::builder()
    .component(ApprovalComponent::new(broker.clone()))
    .component(
        ToolsComponent::new()
            .with_tool(FsWriteTool::new("/home/user/project")?)
            .with_policy(Arc::new(policy))
            .with_approvals(broker.clone() as Arc<dyn ApprovalSink>),
    )
    .build()?;
```

The frontend side is a loop:

```rust,ignore
let mut stream = broker.gate().subscribe();
while let Some(pending) = stream.recv().await {
    println!("{} — {}", pending.prompt, pending.request.action);
    match ask_the_user(&pending) {
        true => stream.gate().approve(&pending.id)?,
        false => stream.gate().deny(&pending.id)?,
    }
}
```

Every way of *not* getting an answer is a refusal, and none of them can become an allow: no
frontend attached, nobody answered before the deadline, the operation was cancelled, too
many prompts queued at once, the system shut down mid-question. Holding a gate is what
tells the broker somebody is listening — with none attached, a question is refused
immediately rather than waiting out its timeout, so a headless deployment behaves exactly
like one with no approval sink at all. An answer that arrives after the requester gave up
does nothing, and the responder is told so.

## Not re-sending everything, every turn

`ModelProvider::complete` takes a `Vec<Message>`, and nothing holds one between calls. An
agent written against that directly has one option: keep the whole history locally and send
all of it, every turn. The same system prompt, the same early turns and the same 4 KB file
read get paid for again on every request, cost grows quadratically in turns, and when the
history outgrows the model's context window there is no answer at all.

[`aik-context`](crates/context) is the fix, and it is not a compressor. It stops treating the
model payload as the place state lives:

```rust,ignore
// Append what happened. Full fidelity, kept forever, never sent anywhere.
store.append(&session, ContextEntry::new(system_prompt).pinned(), &cx).await?;
store.append(&session, ContextEntry::new(tool_result), &cx).await?;

// Derive what to send. Recomputed each turn under a budget, then thrown away.
let budget = ContextBudget::tokens(8_000).with_max_part_tokens(512);
let window = store.window(&session, &budget, &cx).await?;

let request = CompletionRequest::new(model, window.messages);
```

The store is append-only and the window is a pure function of it, so the same records under
two budgets give two windows and change nothing. Assembly is deterministic — no model call,
no invented text:

* **pinned records always survive**, so a system prompt is never silently dropped to hit a
  number;
* **oversized parts are elided**, with the bulk of a file read, a directory listing or a
  base64 image replaced by a marker naming the record it came from — the full value is still
  in the store and still fetchable by `ContextStore::get`;
* **the oldest turns are evicted**, keeping a contiguous run of the most recent ones rather
  than whichever happen to fit;
* **a tool result whose call was evicted is removed**, because a result answering nothing is
  a request most providers reject.

Every window reports what it cost and what it left out, and publishes a `ContextAssembled`
event on the same bus the audit events use — counts only, never conversation content.
Counting itself goes through `dyn TokenCounter`: a documented byte-length heuristic by
default, so budgeting works everywhere without the kernel acquiring a tokenizer, and
replaceable by a provider that knows its own.

Security is about what a model can influence. It can influence the text of a record. It
cannot influence who the record is attributed to, where it sits, whether it is pinned, or
which sessions it can see — all of those come from the `ExecutionContext` and the kernel
clock, not from the payload. A session is owned by the principal that created it, and a
`ContextStore` is not a `Tool` and must never be registered as one: there is no path from
model output to it that does not go through trusted code deciding to record something.

## Actually using it

[`aik-cli`](crates/cli) is the frontend: a terminal that starts a kernel, holds one
conversation with the agent, prints what it does, and answers approval prompts. See
[`docs/CLI.md`](docs/CLI.md) for the full manual — every option explained with examples, how to
write a policy without hitting its sharpest edge, verbose-mode output explained event by event,
and a troubleshooting table. This section is the short version.

For what a run actually costs — exact provider-reported tokens versus locally estimated
ones, tool-schema overhead, context accounting, latency, a `--record`ed JSONL format for
machine-readable analysis, and reproducible benchmark commands — see
[`docs/MEASUREMENTS.md`](docs/MEASUREMENTS.md), which supersedes the baseline
`docs/CLI.md` previously carried inline.

### Prerequisites

- Rust 1.85 or newer (edition 2024)
- A running [Ollama](https://ollama.com) server (`ollama serve`) with at least one model pulled
  (`ollama pull llama3.1:8b`). For tool-calling examples specifically, the model needs to report
  the `tools` capability — check with `ollama show <model>`; not every model does, and one that
  doesn't will still answer, just in prose rather than by calling anything.

```bash
cargo build -p aik-cli
cargo run -p aik-cli -- --config crates/cli/aik.example.json --root /path/to/project
```

```
aik 0.1.0
  agent:  assistant acting for user
  root:   /path/to/project
  store:  /home/you/.local/share/aik/aik.redb
  memory: remember

› what is the codename in notes.txt?
  → filesystem.read {"path":"notes.txt"}
  ← {"content":"The project codename is Halibut.\n","path":"notes.txt"}
The codename is Halibut.

  [2 turns, 1 tool calls, 1181 in / 125 out tokens, window 95 tokens]
```

One prompt as an argument runs it once and exits:

```bash
cargo run -p aik-cli -- -c crates/cli/aik.example.json "what is in src?"
```

| Option | |
|---|---|
| `-m, --model <ID>` | model to use; otherwise the configured one, otherwise the provider's first |
| `-a, --agent <ID>` | the agent's identity, as policy sees it (default `assistant`) |
| `-u, --user <ID>` | the user's identity, as policy sees it (default `user`) |
| `-r, --root <DIR>` | what the filesystem tools are confined to (default: the current directory) |
| `-c, --config <FILE>` | JSON configuration, including the policy |
| `-p, --policy <FILE>` | a policy document on its own, overriding the one in `--config` |
| `--write` | also register the write tool |
| `--no-tools` | register none, memory included |
| `--memory <MODE>` | which memory tools to register: `off`, `recall`, `remember`, `full` (default `remember`) |
| `--db <FILE>` | the shared database (default: `components.store.db.path`, else `$XDG_DATA_HOME/aik/aik.redb`) |
| `--ephemeral` | open no database; the transcript, memories and schedule live only for this process |
| `-v, --verbose` | print authorization and context events as they happen |
| `-R, --record <FILE>` | append a JSONL measurement record of the run (counts and timings only) |

In a session, `/new` starts a fresh conversation, `/session` says who is acting,
`/tools` lists what the agent has, `/quit` leaves. Ctrl-C cancels the turn in progress.

### What persists, and where

One [redb](https://github.com/cberner/redb) database holds three things: the conversation
transcript, the agent's memories, and any scheduled job marked persistent. It is created
`0600` inside a `0700` directory, defaults to `$XDG_DATA_HOME/aik/aik.redb`, and refuses to
start rather than guessing a location when neither `XDG_DATA_HOME` nor `HOME` is set — the
working directory would put a file holding every transcript somewhere a backup or a
repository could pick it up. `--ephemeral` opens nothing at all, and a persistent job asked
of an ephemeral run is refused rather than accepted and forgotten.

Memory is reached the same way everything else is: four tools behind the registry, so a
policy decides each call and an audit event records it. Nothing is recalled automatically —
the agent asks for what it wants. `memory.delete` is not registered unless you ask for it
with `--memory full`, and the shipped policy still puts every deletion to a human.

### The agent is not you

Every turn runs as `Principal::new(agent, Agent).on_behalf_of(user)` — never as the user.
So a policy can distinguish what a person may do from what a model acting for them may do,
and a rule naming `alice` does not hand her permissions to whatever is answering on her
behalf:

```json
{ "principal": { "id": "assistant", "kind": "agent" },
  "action": "filesystem.read", "resource": "*",
  "effect": { "decision": "allow" } }
```

That identity also owns the transcript, so a `ContextStore` session written by one agent is
not readable as anyone else.

### Interactive and one-shot are different security postures

An `ApprovalBroker` parks a question only while a gate exists; with none, it refuses
immediately. An interactive session holds one for as long as it runs. **A one-shot run does
not**, so anything a policy defers to a human is refused rather than waiting out a timeout
in front of an empty terminal:

```
  → filesystem.write {"content":"hi","path":"hello.txt"}
  ✗ {"kind":"permission","message":"permission denied: no approval responder is attached, so nobody can answer"}
```

Scripted use therefore needs a policy that says `allow` outright, which is the point: the
decision is written down in advance rather than made by whoever is not watching.

### What the frontend is not allowed to do

It authorizes nothing. It holds a `dyn Agent` and cannot reach a tool at all, so there is no
path from the terminal to an operation that skips the registry. It never invents a policy —
with none configured every tool call is denied, and it says so at startup rather than
shipping a permissive default. And it can only narrow: not registering the write tool
removes a capability that no policy can then restore.

One thing it *does* own is the screen. Assistant text, tool arguments and file contents are
all untrusted, and a terminal executes some bytes rather than printing them — a model that
emits `\x1b[2K` or a bare `\r` could otherwise repaint a line, including the approval prompt
somebody is about to answer. Every untrusted string is escaped before it is printed, and
approval prompts deliberately show only what the policy engine and the registry wrote:
the question, the action and the resource, never the tool's arguments.

## Running programs

[`aik-exec`](crates/exec) is the first capability where the thing being authorized is not a
request the implementation carries out, but *arbitrary host code it starts*. `aik-fs` resolves
a path, checks it against a root and reads bytes; nothing it hands the host can decide to do
something else. A program can — `git` is not a promise to read a repository, it is a promise
to run whatever `/usr/bin/git` is.

So four things decide whether a program runs, and only one of them is a boundary:

| Measure | Answers | Bounds |
|---|---|---|
| Registration (`--exec`) | Does this deployment run anything at all? | What exists |
| The allowlist (`agent.exec.programs`) | Which programs? | What is asked for |
| Policy (`process.execute`) | Which commands, for whom? | What is asked for |
| The sandbox | What can it reach once running? | **What happens** |

The first three are cooperative and worth having — they are what an audit trail records and a
human approves — but none survives contact with a program that does something other than what
its name suggests. The sandbox does. A sandboxed child gets its own user, mount, pid, ipc and
uts namespaces; a read-only view of `/usr` and the loader's files and *nothing else of `/etc`*;
a private `/proc`, a minimal `/dev` and a size-capped tmpfs `/tmp`; no network unless the
deployment granted one; the confinement root, read-only by default, as its single writable
path; an environment built from nothing rather than inherited; a session of its own with no
terminal; and resource limits the OS keeps enforcing whatever the kernel does afterwards.

It is off unless asked for, and asking for it verifies it: `--exec sandboxed` finds
`bwrap`, starts a throwaway sandbox at startup to prove the host can provide one, and fails to
start if it cannot — rather than running programs unconfined and saying nothing.

```bash
aik --exec sandboxed          # allowlisted programs, confined
aik --exec unconfined         # no sandbox: the allowlist is then the only limit
```

There is no shell. A call names one program and gives its arguments as separate strings;
nothing is split on whitespace, glob-expanded or passed to `sh -c`, so an argument containing
`; rm -rf /` is one argument containing those characters. A deployment that allowlists a shell
has undone both the allowlist and this paragraph, which is the one entry nobody should add.

Each call declares two resources, so a policy can answer at either grain, and the command a
human is asked about is the command that will actually run:

```json
{ "action": "process.execute", "resource": "program/git",       "effect": { "decision": "allow" } },
{ "action": "process.execute", "resource": "command/git log *", "effect": { "decision": "allow" } },
{ "action": "process.execute", "resource": "command/git *",
  "effect": { "decision": "require_approval", "prompt": "let the agent run this git command?" } }
```

The command is rendered so that distinct argument vectors always produce distinct strings —
anything not made purely of unambiguous characters is single-quoted — because a rule written
for `git commit -m fix` must not also match one argument that merely looks like three.
Resource patterns match by prefix with no word boundary, which is why every pattern above
carries its separating space: `command/git*` would also match `gitk`.

## Configuration

The kernel reads no files. It accepts JSON layers, deep-merged in order, so the host
decides where settings come from:

```rust,ignore
let config = Config::builder()
    .layer(defaults)              // compiled-in
    .layer(from_toml_file()?)     // the host's choice of format
    .env("AIK_")                  // AIK_KERNEL__EVENT_CAPACITY=512
    .build();
```

Components read their own section, `components.<id>`, via `ctx.settings()`.

Kernel settings:

| Key | Default | Meaning |
|---|---|---|
| `kernel.event_capacity` | 256 | Per-event-type broadcast buffer |
| `kernel.shutdown_timeout_ms` | 10000 | How long shutdown waits for background tasks |

Deployment-wide settings live in one `agent` section, read by every frontend so a terminal and
a host process over one database cannot describe two different assistants:

| Key | Default | Meaning |
|---|---|---|
| `agent.agent` / `agent.user` | `assistant` / `user` | The identities policy and the audit trail see |
| `agent.root` | the working directory | The confinement root, for files and for programs |
| `agent.exec.programs` | none | The bare program names `aik-exec` will run |
| `agent.exec.writable` | `false` | Whether a program may write to the root |
| `agent.exec.network` | `false` | Whether a program has a network |

## Development

The full verification suite, in the order it is meaningful to run it:

```bash
cargo fmt --all --check
cargo check --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

Requires Rust 1.85 or newer (edition 2024). No platform-specific code: the workspace
compiles anywhere Tokio does, even though the target system is Arch Linux with Hyprland.

## Status

The kernel is complete and tested. The `ModelProvider` contract has one real implementation
(`aik-ollama`); the `Tool`/`ToolRegistry`/permission contracts have a reference
implementation (`aik-tools`) with resource-level authorization, tool-initiated
authorization for resources discovered mid-run, and audit events on the existing
`EventBus`; a real `PolicyEngine` (`aik-policy`) makes that enforceable from configuration;
`aik-fs` is where the system touches the host, with read, write and directory-listing tools,
each confined to a configured root; `aik-approval` closes the last gap in that path, so a
policy that defers to a human reaches one instead of failing closed by default; and
`aik-context` makes the agent loop affordable, by making the transcript a piece of kernel
state rather than something reassembled into every request; `aik-agent` is the loop
itself, the first thing that uses all of the above together; and `aik-audit` makes the whole
of that accountable after the fact rather than only observable while it happens. Each proves the
registry/component architecture hosts a real capability cleanly, without changing `aik-core`
itself.

The system works end to end: `aik-cli` starts a kernel, `aik-ollama` carries tool calls in
both directions, and a model can ask for a tool, have the request authorized, have a human
approve it, and answer from the result — with every step of that visible in the audit
events, and now kept: `aik-audit` writes those events into the shared database as an
append-only trail, and `aik audit` is how an operator reads it back afterwards. Both of the
things that were next have since been built the same way, *on* the kernel rather than into it:
`aik-daemon` hosts the kernel for more than one frontend, and `aik-exec` runs programs behind
an OS-level sandbox — the first capability here whose subject is arbitrary host code rather
than a request the implementation carries out itself, and therefore the first one where a
cooperative check was not enough and an enforcement boundary was required.

What is genuinely not built yet: semantic memory (`aik-memory` has no `Embedder` behind it),
context summarisation, and any platform integration at all. `aik-scheduler` now defines a
cron dialect of its own — five-field, UTC, `cron(5)`-compatible — and refuses only an
expression that does not parse in it, not the concept.

The full pipeline — filesystem confinement, policy evaluation, human approval, tool exposure
narrowing, verbose auditing, and the CLI's own error and session handling — has been manually
exercised end to end against a real Ollama server, not only through the automated suite; see
[`docs/CLI.md`](docs/CLI.md#known-limitations-and-fixes-made-during-this-review) for what that
covered, the two bugs it found and fixed, and the token/context cost baseline it produced.
`docs/CLI.md`'s [limitations sections](docs/CLI.md#other-known-limitations-not-bugs) separate
what is a genuine defect from what is a documented, deliberate property of the current
implementation (no summarisation, no semantic memory, a heuristic token counter, an unclosed
filesystem TOCTOU window bounded but not eliminated by handle-pinning). `aik-exec` documents
its own such property in the crate: it installs no seccomp filter, so a sandboxed child is
separated from the host by namespaces and mount visibility rather than by a syscall policy,
and the boundary is only as strong as the kernel's user-namespace implementation.
