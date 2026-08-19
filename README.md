# AI-kernel

The core of a long-term AI operating layer — the small, permanent foundation that agents,
model providers, tools, memory, permissions, scheduling, desktop integration and every
frontend will be built on.

This stage is the kernel only. It is not an LLM wrapper: it contains no models, no agents,
no tools, no storage, no UI and no operating-system code. It contains the mechanisms that
such things need in order to coexist.

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
| [`aik-approval`](crates/approval) | A human-in-the-loop `ApprovalSink`, answered by a frontend |
| [`aik-context`](crates/context) | An agent's transcript, and the budgeted model window derived from it |

`aik-core` does not depend on `aik-api`, and neither depends on `aik-ollama`, `aik-tools`,
`aik-policy`, `aik-fs`, `aik-approval` or `aik-context`. A kernel can be built and run with
none of the subsystem contracts present, no model provider, no tool registry, and no policy
engine at all.

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
talks to [Ollama](https://ollama.com) over HTTP, with streaming, cancellation and timeouts.
Nothing about HTTP or Ollama's wire format leaves that crate — consumers depend on
`dyn ModelProvider`, resolved through the registry, exactly like the `Notifier` example
above.

```bash
cargo run -p aik-ollama --example chat
cargo run -p aik-ollama --example chat -- mistral "what is a kernel?"
```

Requires a running `ollama serve` with a model pulled; if it is not reachable, the example
prints a clear explanation and exits cleanly instead of failing loudly. The crate's own test
suite (`cargo test -p aik-ollama`) needs no such server — it runs against a mocked HTTP
server, deterministically.

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

## Development

```bash
cargo test --workspace
```

```bash
cargo clippy --workspace --all-targets
```

```bash
cargo doc --workspace --no-deps --open
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
`aik-context` is what will make the agent loop affordable, by making the transcript a piece
of kernel state rather than something reassembled into every request. Each proves the
registry/component architecture hosts a real capability cleanly, without changing `aik-core`
itself. What comes next builds *on* the kernel rather than *into* it: process execution
behind an OS-level sandbox, durable audit and transcript storage, a frontend that actually
renders an approval prompt, and eventually the agent loop that will call all of this — each
as a component in its own crate, following the same pattern.
