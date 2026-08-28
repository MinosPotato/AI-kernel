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
| [`aik-anthropic`](crates/anthropic) | A `ModelProvider` backed by the Anthropic Messages API, credential and all |
| [`aik-tools`](crates/tools) | The authorization-gated `ToolRegistry` implementation |
| [`aik-mcp`](crates/mcp) | External Model Context Protocol tool servers, as one `ToolCatalog` |
| [`aik-policy`](crates/policy) | A deterministic, configuration-driven `PolicyEngine` |
| [`aik-quota`](crates/quota) | Cumulative ceilings on what a principal may spend on models |
| [`aik-resilience`](crates/resilience) | Retrying, circuit breaking and concurrency limiting in front of any `ModelProvider` |
| [`aik-fs`](crates/fs) | Filesystem `Tool`s — read and write — each confined to a configured root |
| [`aik-exec`](crates/exec) | Running allowlisted programs behind an OS-level sandbox |
| [`aik-approval`](crates/approval) | A human-in-the-loop `ApprovalSink`, answered by a frontend |
| [`aik-context`](crates/context) | An agent's transcript, and the budgeted model window derived from it |
| [`aik-summary`](crates/summary) | Replacing a session's oldest turns with a model-written recap of them |
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

## Talking to a hosted model

[`aik-anthropic`](crates/anthropic) is the second `ModelProvider`, and the first that leaves
the machine: it speaks the [Anthropic Messages API](https://docs.anthropic.com/en/api/messages),
with streaming, tool calling, cancellation, deadlines and bounded retries. Everything above it
is unchanged — the agent loop, the tool registry, the transcript all resolve `dyn
ModelProvider` and cannot tell which one they got — which is what a second implementation is
for: a contract with one implementation is a description of that implementation.

Two shapes differ from Ollama's and are resolved inside the crate. Instructions are hoisted
out of the conversation into the API's top-level `system` field, and tool results become
`tool_result` blocks on a user turn, because the API has neither a `system` nor a `tool` role.
And a streamed tool call arrives in fragments — an id and a name first, then its arguments as
partial JSON — so the fragments are reassembled and a call is only emitted once it parses.
Arguments that never parse are an error rather than an empty call: a tool invoked with `{}`
because its arguments were lost is a tool invoked with the wrong arguments.

Select it per deployment, with the model to go with it:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
aik --provider anthropic -m claude-sonnet-4-5 "what is a kernel?"
```

### Where a credential may live

This is the first secret in the workspace, so the rules are enforced by code rather than
written down as a practice:

- **Configuration says where the key is, never what it is.** `api_key_env` (default
  `ANTHROPIC_API_KEY`) or `api_key_file`. A section carrying `api_key` — or four other
  spellings — fails at startup with a message saying where a key belongs instead. The kernel's
  `Config` is a JSON tree that is cloned, merged and `Debug`-printed throughout the process,
  so anything in it is effectively public to the process.
- **The key cannot be printed.** `ApiKey` has no `Display` and no `Serialize`, its `Debug` is
  `ApiKey(<redacted>)`, and the only reader is crate-private. Its header is marked sensitive,
  so the HTTP stack will not log it either.
- **The transport is checked before the key is sent.** A non-`https` endpoint is refused
  unless it is loopback, redirects are never followed, and a key file other users can read is
  a startup failure rather than a warning.

A missing or malformed key stops the kernel from starting, rather than being discovered on the
first turn somebody types.

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

### When the budget is not enough

Eviction is honest and it is still a loss: past some length, the model stops being told how
the conversation started, and from its side that is indistinguishable from it never having
happened. [`aik-summary`](crates/summary) is the answer to that, and it is deliberately *not*
part of the store — replacing turns with a paragraph needs a model, which makes the operation
fallible, costly and, since a transcript is full of tool output, an injection surface.

So it is a separate capability behind its own contract, `ContextCompactor`, and the loop asks
for it only when its own window says it is dropping records:

```text
window drops records ──▶ read the oldest turns ──▶ model writes a recap (no tools offered)
                                                            │
   the turns it covered ◀── ContextStore::compact ◀── appended back, unpinned
```

The order is the safety property. Nothing is removed until something has replaced it, so a
model outage costs a call and no history. What comes back is treated as what it is — model
output, from untrusted input:

* the summarising call **offers no tools**, so nothing in a transcript can turn a
  summarisation into an action;
* the recap is **never pinned and never a system message**, so a model cannot make its own
  words permanent or give them the authority of the deployment's own prompt;
* the excerpt is bounded per part and in total, binary payloads are described rather than
  carried, and its delimiter is neutralised wherever transcript content contains it;
* the recap is marked with `aik.summary` and framed as a recap, because an append-only
  transcript stores it as the *newest* record — so the loop compacts before it records the
  next question, and the last thing in the window stays the thing being answered.

It is on by default and costs nothing until a session actually overflows. `agent.summary`
turns it off, points it at a cheaper model, or changes how much of the recent conversation
survives a round; every round publishes a `ContextCompacted` event carrying counts and no
content.

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

- Rust 1.90 or newer (edition 2024)
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

## Tools this repository did not write

[`aik-mcp`](crates/mcp) connects to Model Context Protocol servers — the programs that expose
a filesystem, an issue tracker, a database or an API as tools — and contributes what they
offer as ordinary kernel tools, named `mcp.<server>.<tool>`.

It is the second thing in the workspace that supplies tools and the first that supplies tools
it did not write, which is the point: `aik-anthropic` exists so that `ModelProvider` is a
contract rather than a description of Ollama, and this does the same for `ToolCatalog`.

```json
{ "agent": { "mcp": { "servers": [
  { "label": "files",
    "command": "mcp-server-filesystem",
    "args": ["/home/user/project"],
    "env": { "PATH": "/usr/bin:/bin" },
    "tools": ["read_file", "list_directory"] }
] } } }
```

```bash
aik --mcp on
```

**Two trust boundaries, and they are not the same one.** A server is trusted code: it runs as
your account, it is not sandboxed, and which servers exist is a configuration decision an
operator makes — never a model, never a conversation. A server's *output* is not trusted at
all, because in any deployment where the server talks to the outside world, its tool
descriptions and results are written by whoever can reach the thing it talks to. So names are
validated, descriptions are stripped of control characters and capped, schemas are checked for
the shape a provider needs, results are truncated, binary content is described rather than
carried, and a frame is size-limited before it is parsed.

**Nothing here is a second, softer path to a tool.** The catalogue hands `Box<dyn Tool>` to
whatever assembles the registry and never to anything that could call one; from there an MCP
tool goes through the same policy engine, the same approval sink and the same audit events as
`fs.write`. Four limits apply, and only one of them is policy:

| Limit | Answers | Set by |
|---|---|---|
| Registration (`--mcp`) | Is there an MCP tool at all? | The command line |
| `agent.mcp.servers` and each `tools` list | Which servers, and which of their tools? | Configuration, at startup |
| Policy (`mcp.invoke` on `mcp:<server>/<tool>`) | Who may call which, and when is a human asked? | The policy document |
| Frames, results, tool counts, timeouts | What may one call cost? | Configuration, conservative by default |

```json
{ "action": "mcp.invoke", "resource": "mcp:files/read_file", "effect": { "decision": "allow" } },
{ "action": "mcp.invoke", "resource": "mcp:files/write_file",
  "effect": { "decision": "require_approval", "prompt": "let the agent write that file?" } }
```

The refusals are the interesting part again:

* **A server cannot make itself auto-approvable.** MCP tools may carry a `readOnlyHint`, and
  it is written by the thing being authorized, so it is not read. Every remote tool is
  `read_only: false`; a deployment that knows better says so in *its* policy.
* **A server cannot ask anything of this process.** This client advertises no capabilities, so
  `sampling/createMessage` — a server asking your model to generate text, with a prompt the
  server wrote and a bill you pay — is answered "method not found", promptly and by id. So is
  a request for your filesystem roots.
* **A server gets an environment built from nothing.** `env_clear`, then exactly what
  `agent.mcp.servers[].env` names. Your model credential, your database path and your
  `SSH_AUTH_SOCK` do not reach third-party code because nobody thought about it.
* **A server cannot shadow a native tool.** Names are namespaced by the deployment's own label,
  a remote name that could punctuate that namespace is refused, and a collision with an
  already-registered tool fails the kernel's startup rather than displacing anything.
* **A misconfigured server is a startup failure, not a missing capability.** A `tools` entry
  the server does not offer, a command that is not on the configured search path, a listing
  larger than the deployment accepts: each names the setting that is wrong, because an agent
  that quietly cannot do what the operator granted is the worse outcome.

## Spending only so much

Every bound the agent loop has is a bound on one run: sixteen model turns, sixty-four tool
calls, an eight-thousand-token window. They stop a conversation that will not stop itself,
which is what they are for, and then the next run starts at zero. Nothing in them answers the
question an operator actually has — how much may this deployment spend today — and once
`schedule.create` existed, a model could arrange for runs to happen while nobody was watching.

[`aik-quota`](crates/quota) is the ceiling that does not reset. It is a second document beside
the policy one: policy decides *whether* something may happen, this decides *how much*.

```json
{ "quota": {
    "limits": [
      { "subject": "*",         "period": "day",   "max_turns": 500 },
      { "subject": "*",         "period": "month", "max_cost_micros": 50000000 },
      { "subject": "scheduler", "period": "hour",  "max_turns": 20,
        "description": "autonomous work, unattended" }
    ],
    "prices": {
      "claude-*": { "input_micros_per_million": 3000000, "output_micros_per_million": 15000000 },
      "*":        { "input_micros_per_million": 0, "output_micros_per_million": 0 }
    }
} }
```

Unlike the policy document, this one is not first-match-wins: **every rule whose subject
matches applies**, and the check refuses as soon as any of them is exhausted. Order is
therefore insignificant, and adding a rule can only tighten what a deployment permits — which
is the property worth having in the document that decides how much money can be spent. Prices
are quoted per million tokens because that is how providers publish them, in millionths of a
currency unit so that `$3.00 / Mtok` is the exact integer `3000000` with no floating point
between an intention and an enforced ceiling. No currency is named anywhere.

A charge lands on **both** identities in play. A turn taken by `assistant` acting for `alice`
is added to the agent's counters and to Alice's, as two independent rows, because they answer
two different questions: a ceiling written for a person should hold however many agents do
that person's work, and one written for `scheduler` should hold across everybody whose jobs it
is running. Every identity comes from the `ExecutionContext`; nothing a model emits reaches
this.

The loop asks before the turn and reports after it:

```text
check(model) ──▶ assemble window ──▶ model turn ──▶ record(turns, tokens, price)
      │                                                        │
   refused: no window, no compaction, no request      failed: the run stops
```

Checking first and charging afterwards means a period can end at most **one turn** over its
ceiling — the turn that crossed it — because what a turn costs is only knowable once it has
been taken. That bound is documented rather than hidden; reserving an estimate up front would
trade an explicable overshoot for a systematic over- or under-charge, since the estimate is a
heuristic and the real figure comes back with the response.

The refusals are again where the design is:

- **A refused turn costs nothing.** The check happens before the window is assembled, so an
  exhausted budget does not pay for a compaction, which is itself a model call.
- **A provider that reports no usage is not free.** Its turns are charged from the run's own
  token estimate, marked as an estimate, because charging zero would make a token or cost
  ceiling silently unreachable.
- **An unpriced model under a cost ceiling is refused**, naming the model and the key to add.
  Pricing it at zero is how a deployment says a model is genuinely free.
- **Spend that cannot be recorded ends the run.** A ledger that cannot be written is not a
  reason to keep spending; the turn already taken stays in the transcript, and no further turn
  is started.
- **A restart is not a way to reset a budget.** The durable ledger lives in the shared
  database alongside the transcript, the schedule and the audit trail. An `--ephemeral`
  deployment gets the volatile one, bounded while it runs, exactly as its audit trail is.

The ledger is an enforcement counter and not a record of what happened — the audit trail is
that. So every write drops the windows that have closed, and the table holds one row per
subject per period rather than one per day for ever.

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

The Anthropic provider's section is `components.model.anthropic` — the dots in its component
id nest — and holds locations and limits, never the key itself:

| Key | Default | Meaning |
|---|---|---|
| `api_key_env` | `ANTHROPIC_API_KEY` | The environment variable the key is read from |
| `api_key_file` | none | A file holding the key; wins over the variable, must be mode `600` |
| `endpoint` | `https://api.anthropic.com` | Must be `https` unless it is loopback |
| `api_version` | `2023-06-01` | The `anthropic-version` header |
| `max_output_tokens` | 4096 | The `max_tokens` a request does not set itself |
| `request_timeout_ms` | 300000 | Ceiling on one request, unless the caller's deadline is shorter |

Retrying, circuit breaking and concurrency limiting are configured once for whichever provider
the deployment chose, under `components.model.resilient`. The layer is always registered — the
way to have none is to configure a pass-through, so the question "does this deployment retry?"
is answered by this section rather than by whether something happens to be wired in:

| Key | Default | Meaning |
|---|---|---|
| `retry.max_attempts` | 3 | Attempts per call, including the first; `1` disables retrying |
| `retry.base_delay_ms` | 500 | The first backoff ceiling, doubling per attempt |
| `retry.max_delay_ms` | 8000 | The largest backoff ceiling, however many attempts failed |
| `retry.max_retry_after_ms` | 60000 | The longest a service's own `retry-after` may park a call |
| `breaker.failure_threshold` | 5 | Consecutive transient failures before calls are refused outright; `0` disables the breaker |
| `breaker.cooldown_ms` | 30000 | How long an open circuit waits before letting one call through |
| `max_concurrent` | 4 | Calls in flight at once; `0` is unlimited |
| `acquire_timeout_ms` | 30000 | How long a call waits for a slot, unless the caller's deadline is shorter |

Only a failure the provider itself marked transient is ever repeated: a rate limit, a 5xx, a
connection that could not be made. A refused credential, a malformed request and a model that
does not exist are answered once. The delay is fully jittered, so several callers that failed
together do not march back in step.

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
| `agent.provider` | `ollama` | Which model provider answers: `ollama` or `anthropic` |
| `agent.model` | the provider's first model | The model every turn is sent to |
| `agent.embedding_model` | none | Embed memories with this, so `memory.query` can search by meaning; needs the `ollama` provider |
| `agent.summary.enabled` | `true` | Whether an overflowing session is compacted rather than silently shortened |
| `agent.summary.model` | `agent.model` | The model that writes recaps; usually worth pointing at a smaller one |
| `agent.summary.keep_recent` | 8 | How many recent records survive a round when no token budget bounds the window |
| `agent.exec.programs` | none | The bare program names `aik-exec` will run |
| `agent.exec.writable` | `false` | Whether a program may write to the root |
| `agent.exec.network` | `false` | Whether a program has a network |
| `agent.mcp.servers[].label` | required | The name the server's tools are namespaced under: `mcp.<label>.<tool>` |
| `agent.mcp.servers[].command` / `.args` | required / none | The bare program name to run, and its argument vector; there is no shell |
| `agent.mcp.servers[].env` | none | The server's entire environment; nothing is inherited |
| `agent.mcp.servers[].cwd` | `agent.root` | Where the server is started |
| `agent.mcp.servers[].tools` | all of them | Which of the server's tools this deployment exposes at all |
| `agent.mcp.servers[].permission` | `mcp.invoke` | The action policy is asked about for every call |
| `agent.mcp.servers[].search_path` | `/usr/bin:/bin:/usr/local/bin` | Where the command is looked for; never the inherited `PATH` |
| `agent.mcp.servers[].call_timeout_ms` | 60000 | The wall-clock budget for one `tools/call` |
| `agent.mcp.servers[].max_result_bytes` | 65536 | The largest result carried back to a model |
| `agent.mcp.servers[].max_tools` | 128 | The largest listing accepted from the server |

Spend ceilings live in their own top-level `quota` section, read and validated while the
kernel is assembled so a malformed rule stops the process rather than the first turn:

| Key | Default | Meaning |
|---|---|---|
| `quota.limits[].subject` | `*` | Which identity this rule counts: `*`, a prefix like `agent.*`, or an exact principal id |
| `quota.limits[].period` | required | `hour`, `day`, `week`, `month` or `total`, in UTC |
| `quota.limits[].max_turns` | none | The most model turns one window may take |
| `quota.limits[].max_input_tokens` / `.max_output_tokens` / `.max_total_tokens` | none | Token ceilings, each enforced independently |
| `quota.limits[].max_cost_micros` | none | What one window may cost, priced by `quota.prices` |
| `quota.limits[].description` | none | What the rule is for, quoted back in the refusal |
| `quota.prices.<model>` | none | `input_micros_per_million` and `output_micros_per_million`; the key is a pattern, exact beats longest prefix |

A rule that sets no ceiling, or sets one to zero, is refused rather than ignored: zero is
never something anybody configures on purpose, and a prohibition belongs in the policy
document where it is auditable as an authorization decision.

## Development

The full verification suite, in the order it is meaningful to run it:

```bash
cargo fmt --all --check
cargo check --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

Requires Rust 1.90 or newer (edition 2024). No platform-specific code: the workspace
compiles anywhere Tokio does, even though the target system is Arch Linux with Hyprland.

Two things that suite does not cover, because they are questions about the dependency
tree rather than about this code:

```bash
cargo deny --all-features check                        # advisories, licences, duplicates, sources
cargo +1.90 check --workspace --all-targets --locked   # the MSRV actually claimed
```

`cargo deny` needs installing once, pinned to the version CI uses:

```bash
cargo install --locked --version 0.20.2 cargo-deny
```

### What CI runs

Every command above is a job in [`.github/workflows/ci.yml`](.github/workflows/ci.yml),
on each push to `main` and each pull request. A suite that only runs when somebody
remembers is a suite that has already stopped running, so the two lists are meant to
stay identical: a command added here belongs there, and vice versa.

Three of those jobs are not just the local suite repeated:

- **`msrv`** reads `rust-version` out of `Cargo.toml` and checks the workspace with a
  compiler that old, rather than with whatever stable happens to be. The claim and the
  check cannot drift, because there is only one place the version is written down.
- **`test`** installs bubblewrap and lifts Ubuntu's AppArmor restriction on
  unprivileged user namespaces. `aik-exec`'s confinement tests skip themselves on a
  host with no `bwrap`, so without this the one boundary here that is *enforcement*
  rather than a cooperative check would be the least-tested thing in the workspace on
  the only machine that gates merges.
- **`cargo-deny`** is governed by [`deny.toml`](deny.toml): permissive licences only,
  no wildcard versions, crates.io as the only source, and `openssl-sys`/`native-tls`
  denied outright so that nothing can quietly move the TLS stack off rustls without a
  diff to notice. A separate daily
  [advisories workflow](.github/workflows/advisories.yml) re-asks the vulnerability
  question against an unchanged `Cargo.lock`, because the advisory database moves on
  days when nobody pushes.

The workflows depend on `actions/checkout` and `actions/cache` and nothing else, and
neither is granted more than `contents: read`. A pipeline whose job is to police what
the workspace links against would be a strange place to widen the set of third parties
trusted to run in it, so the cargo cache is a small first-party composite in
[`.github/actions/cargo-cache`](.github/actions/cargo-cache/action.yml) rather than the
usual third-party action, and cargo-deny is installed from a pinned version in
[`.github/actions/cargo-deny`](.github/actions/cargo-deny/action.yml) rather than from
whatever `cargo install` resolves to that morning.

One thing CI deliberately does *not* set is a global `RUSTFLAGS: -D warnings`. Cargo
passes RUSTFLAGS to every unit it builds, dependencies included, so a global one would
let a warning in somebody else's crate fail a pull request that never touched it.
Warnings are denied where the flag can be scoped to this workspace instead: `clippy`
passes `-D warnings` after `--`, `cargo doc` is scoped by `--no-deps`, and the lints
the workspace actually cares about are declared in `[workspace.lints]` in `Cargo.toml`.

[Dependabot](.github/dependabot.yml) opens the upgrade pull requests these jobs then
judge: cargo weekly with minor and patch bumps grouped into one review, and the
workflows' own action pins monthly.

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

`aik-anthropic` is the newest of these, and the first to send anything off the machine: a
second `ModelProvider` behind the same contract, which is what keeps the contract from being a
description of Ollama, and the first credential in the workspace — held in a type that cannot
print it, resolved from a location configuration names rather than from configuration itself,
and refused outright over a transport that is not `https`.

Semantic memory was the first capability assembled out of two subsystems rather than added
to one: `aik-ollama` implements `Embedder` over `/api/embed`, and
a memory store given one embeds every record it stores and every search it is handed, ranking
by cosine similarity instead of by recency. Set `agent.embedding_model` — or pass
`--embedding-model` — and `memory.query` gains a `text` argument the model can actually use;
leave it unset and everything behaves exactly as before. The refusals are the interesting
part: a store with no embedder answers `text` with `Unsupported` rather than quietly returning
the newest records, a write that cannot embed fails rather than storing a record no search
will ever find, and a provider with no embeddings endpoint fails at startup rather than
serving a memory that silently never searches.

Context summarisation was the first capability that had to be built *beside* a subsystem
rather than into it. `aik-context` deliberately cannot summarise —
replacing turns with a paragraph needs a model, and a store that needed one would be a store
that could fail, cost money and be talked into things. So `aik-summary` is its own crate
behind its own contract, `ContextCompactor`: it reads the turns a window is about to drop,
has a model write down what they amounted to, appends that back as an ordinary unpinned
record, and only then asks the store to reclaim exactly what the recap covered. The agent
loop gained one optional collaborator and no prompt of its own; a deployment with no
compactor registered behaves exactly as it did before. The interesting parts are the
refusals again: nothing is removed until something has replaced it, so a model outage costs a
call and no history; the summarising call offers no tools, so a transcript cannot turn a
summarisation into an action; and the recap is never pinned and never a system message, so a
model cannot make its own words permanent or give them the authority of the deployment's own
prompt.

`aik-mcp` is the newest of these, and the first that supplies tools this repository did not
write. Everything that acted before it — `aik-fs`, `aik-exec`, the memory tools — was code
reviewed here, changing only when somebody changed it. An MCP server is a program the operator
points at, whose tool names, descriptions, schemas and results are authored elsewhere and can
change between one start and the next. So it splits the trust question in two: the server is
trusted code, run as your account and not sandboxed, chosen once in configuration by a person;
its output is untrusted input, parsed narrowly and bounded everywhere. It also needed the one
seam `ToolCatalog` never had — nothing consumed a catalogue, because a registry could only be
handed tools that already existed as values — so `aik-tools` gained a `with_catalog` that
drains one during `init`, which keeps the property the registry rests on: the set of tools is
frozen before anything can reach it. The refusals are again where the design is: a server
cannot sample from this kernel's model, cannot see its filesystem roots, cannot declare its own
tools auto-approvable, cannot shadow a native tool, and inherits none of this process's
environment.

`aik-scheduler` could already run jobs and fire an agent turn when one came due, but nothing
let a conversation create one — only a deployment's own configuration could. `aik-runtime`
now closes that: three tools (`schedule.create`, `schedule.list`, `schedule.cancel`),
registered by default whenever `JobExecution::Agent` is on, mirroring the split `aik-memory`
already uses instead of one tool with an `operation` argument, so a deployment can hand an
agent the ability to see and cancel its own reminders without the ability to create new ones.
No input carries an `owner`; the scheduler stamps it from the `ExecutionContext` the registry
already hands the tool. And `ScheduleCreateTool` has no `handler` argument at all — every job
it creates targets one fixed component, set once when the tool is built, never read from a
call's arguments, so a model can schedule a reminder for itself but cannot aim a job at a
handler with no business taking one from a model's whim. Binding the tools to the scheduler by
`Arc` would have held the scheduler, which holds every registered job handler, which in this
deployment holds the agent, which holds the tool registry these tools are registered in — a
cycle a daemon test caught by reopening the shared database after stopping the host and finding
it still locked. The binding holds a `Weak<dyn Scheduler>` instead.

`aik-quota` is the newest of these, and the first that constrains the system rather than
enabling it. Everything before it added a capability; this one takes one away, on purpose. The
gap it closes had been visible since the agent loop was written and became load-bearing the
moment `schedule.create` existed: every bound in `AgentLoopSettings` is per run and resets with
it, so a principal that could start runs — a person at a terminal, or a cron expression a model
wrote for itself — had no cumulative ceiling on tokens or money at all, and
`aik_api::measurement` reported what was spent to whoever was listening without anything being
able to refuse. So there is now a second document beside the policy one, read from the same
configuration at the same point in start-up, and one more thing in the shared database: policy
decides whether an action may happen, the ledger decides whether there is any budget left for
one that may. The two are independent and both apply. A charge lands on the acting principal
*and* on whoever it acted for, so a ceiling written for a person holds however many agents do
that person's work. Everything interesting is a refusal: a check happens before the window is
assembled so an exhausted budget cannot pay for a compaction; a provider that reports no usage
is charged the run's own estimate rather than nothing, because zero would make a token ceiling
unreachable; a cost ceiling over a model nobody priced is refused rather than treated as free;
spend that cannot be recorded ends the run rather than continuing on a budget nobody is
keeping; and the durable ledger means restarting the process is not a way to buy another day's
turns.

`aik-resilience` is the newest of these, and the first that wraps a contract rather than
implementing one. Every subsystem before it was registered *beside* the others; this one is
registered *in front of* one — a `ModelProvider` that holds another and hands back something
that behaves identically and fails less often. Everything that resolves `dyn ModelProvider` by
capability therefore gets it without being told, which is a property of component
initialisation order and nothing else, so the wiring declares the dependency rather than
hoping.

The gap it closes had been open since the first model call: everything a provider does crosses
a boundary this process does not control, and a single rate limit, restarting server or
connection cut ended a run — taking a transcript's worth of assembled context, and the money
already spent building it, with it. Three mechanisms answer three different failures. Retry
answers one 503. A circuit breaker answers the hundredth, because a provider that is down does
not come back for being asked again, and retrying into it converts one outage into a queue of
calls that each take several seconds to fail. A concurrency limit answers the failure a client
causes itself: a scheduler firing several agent jobs on the same minute, each retrying, is how
a deployment manufactures its own rate limit.

The interesting part is what decides that a call may be repeated, because `ErrorKind` could
not. A rate limit, an overloaded upstream and a malformed request are all `ErrorKind::Other`
once a provider has wrapped them, which left a caller two options: match on message text, or
retry everything. So `aik_api::resilience` adds a third — a `TransientFailure` a provider
attaches to the error's source chain — and everything unmarked is terminal. That is the
fail-closed direction: not retrying something that would have worked costs one failed run,
while retrying a request a service refused on its merits pays for the same refusal several
times over. The refusals follow from it: a 401 is never repeated, an open circuit is never
retried around, a terminal failure is never evidence for the breaker, a wait that would
outlast the caller's deadline is not taken, and cancellation beats a pending retry.

Streaming is where the rule bites hardest. Establishing a stream is retried; a stream that has
begun never is, because no provider can resume a response and a second attempt would either
duplicate what the caller already saw or silently replace it. Its failure still reaches the
breaker, so a service that accepts every request and then breaks halfway through the answer is
not reported as healthy.

Two things had to move for this to be one answer rather than two. `aik-anthropic` had grown its
own retry loop — its own backoff, its own status list, its own deadline arithmetic — and
leaving it in place would have multiplied the attempt counts: a bound of three attempts would
have been nine upstream calls and nine times the tokens, with neither layer's configuration
describing what actually happened. So that loop is gone and its `max_retries` setting with it,
answered by name at start-up rather than by `deny_unknown_fields`, so a deployment that had
configured retrying is told where it went instead of being told it made a typo. What stays in
each provider is the half only a provider can do: recognising that a failure was the service's,
and passing on how long the service asked to be left alone.

And it charges nothing. An attempt that failed on the way out cost an upstream real work and
this client no tokens it can count, and inventing a figure would put a number nobody can check
into the ledger. What keeps that honest is where the retrying sits: strictly *below* the point
where a response exists to charge for, so the quota guard charges exactly once per turn however
many attempts that turn took, and `retry.max_attempts` is the stated bound on how many that can
be. A cross-subsystem test asserts exactly that, because neither crate could: `aik-quota` has
never heard of a retry and `aik-resilience` never touches a ledger.

What is genuinely not built yet: any platform integration at all. `aik-scheduler` now defines
a cron dialect of its own — five-field, UTC, `cron(5)`-compatible — and refuses only an
expression that does not parse in it, not the concept.

Nothing enforced this document's own five-command suite or checked the dependency tree before
now, so a regression only surfaced if a contributor happened to run it by hand. GitHub Actions
now gates every push and pull request to `main` on it: `fmt`, `check`, `clippy -D warnings`,
`test`, `rustdoc --no-deps`, and an `msrv` build that reads `rust-version` out of `Cargo.toml`
rather than pinning a second copy of the number, so the claim and the check cannot drift.
`cargo-deny` runs alongside — advisories, licences, bans, sources — on the same trigger and
again daily against the unchanged lockfile, because the advisory database moves on days when
nobody pushes. `test` installs bubblewrap and lifts Ubuntu's AppArmor restriction on
unprivileged user namespaces, because `aik-exec`'s confinement tests otherwise skip themselves
on a host with no `bwrap` — without it, the one boundary here that is enforcement rather than a
cooperative check would have been the least-tested thing in the workspace on the only machine
that gates merges. Running the suite for the first time surfaced two real defects it then
fixed: a redundant doc link in `schedule_tools.rs` that fails `cargo doc -D warnings`, and this
document and `docs/CLI.md` both still claiming Rust 1.85 after `Cargo.toml` had moved to 1.90.

The full pipeline — filesystem confinement, policy evaluation, human approval, tool exposure
narrowing, verbose auditing, and the CLI's own error and session handling — has been manually
exercised end to end against a real Ollama server, not only through the automated suite; see
[`docs/CLI.md`](docs/CLI.md#known-limitations-and-fixes-made-during-this-review) for what that
covered, the two bugs it found and fixed, and the token/context cost baseline it produced.
`docs/CLI.md`'s [limitations sections](docs/CLI.md#other-known-limitations-not-bugs) separate
what is a genuine defect from what is a documented, deliberate property of the current
implementation (a heuristic token counter, an unclosed filesystem TOCTOU window bounded but
not eliminated by handle-pinning; the summarisation limitation recorded there has since been
closed by `aik-summary`). `aik-exec` documents
its own such property in the crate: it installs no seccomp filter, so a sandboxed child is
separated from the host by namespaces and mount visibility rather than by a syscall policy,
and the boundary is only as strong as the kernel's user-namespace implementation.
