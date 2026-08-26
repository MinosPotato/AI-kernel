# Architecture

At the center of this repository is the **kernel** (`aik-core`, `aik-api`): the small,
permanent foundation everything else in the workspace is built on top of, and that any future
part of the system (Quickshell UI, chat bridges, background services, external apps) would be
built on top of too.

The kernel is deliberately *not* an LLM wrapper. It knows nothing about models, agents,
tools, memory, Linux, Hyprland or Docker. It knows how to **hold** such things: how they
are named, wired together, started, stopped, configured, discovered and how they talk to
each other. Everything that *does* know about models, tools, agents or a terminal — a real
`ModelProvider`, a real `ToolRegistry`, the agent loop, the `aik` CLI binary — lives in its
own crate downstream of the kernel; see [Workspace layout](#workspace-layout).

## Design rules

1. **Mechanism, not policy.** The kernel provides lifecycle, wiring and messaging. What
   is wired in is decided by the process that builds the kernel.
2. **Everything replaceable.** Anything that eventually touches the OS, a model provider,
   a datastore or a UI is reached through a trait object resolved at runtime.
3. **No global mutable state.** All state is owned by a `Kernel` value and reached through
   an explicitly passed, cheaply cloneable `KernelContext`.
4. **Portable.** Only `std`, `tokio` and serde-shaped data. No `#[cfg(target_os)]` anywhere.
5. **Small.** If a feature can be added later without an architectural rewrite, it is not
   in the kernel yet.

## Workspace layout

```
AI-kernel/
├─ crates/
│  ├─ core/     → aik-core     : the kernel itself (mechanisms)
│  ├─ api/      → aik-api      : domain contracts for downstream subsystems
│  ├─ aik/      → aik          : thin facade re-exporting both
│  ├─ ollama/   → aik-ollama   : a ModelProvider, talking to a local Ollama server
│  ├─ anthropic/→ aik-anthropic: a ModelProvider, talking to the Anthropic Messages API
│  ├─ tools/    → aik-tools    : the reference ToolRegistry (authorization-gated)
│  ├─ mcp/      → aik-mcp      : a ToolCatalog over external Model Context Protocol servers
│  ├─ policy/   → aik-policy   : a deterministic, configuration-driven PolicyEngine
│  ├─ fs/       → aik-fs       : filesystem Tools, confined to a configured root
│  ├─ exec/     → aik-exec     : running allowlisted programs behind an OS-level sandbox
│  ├─ approval/ → aik-approval : a human-in-the-loop ApprovalSink
│  ├─ context/  → aik-context  : a durable transcript and budgeted model windows
│  ├─ store/    → aik-store    : the shared redb database backing context and memory
│  ├─ memory/   → aik-memory   : a persistent record store, retrieved by kind, metadata or meaning
│  ├─ scheduler/→ aik-scheduler: time- and event-triggered jobs, optionally durable
│  ├─ summary/  → aik-summary  : a session's oldest turns, replaced by a recap of them
│  ├─ agent/    → aik-agent    : the agent loop tying every capability above together
│  ├─ runtime/  → aik-runtime  : system assembly — settings in, wired kernel out
│  ├─ ipc/      → aik-ipc      : the authenticated local protocol, host and client halves
│  ├─ daemon/   → aik-daemon   : the host process (the `aikd` binary)
│  └─ cli/      → aik-cli      : a terminal frontend (the `aik` binary)
```

`aik-core` and `aik-api` are the kernel proper: `aik-core` must be able to compile and be
reasoned about without any opinion at all about agents or models, and `aik-api` is where the
*shape* of everything downstream lives — object-safe trait definitions only, nothing
implemented, allowed to churn while `aik-core` stays stable.

Every other crate is a concrete implementation of one or more `aik-api` contracts, built in
dependency order: `aik-ollama` implements `ModelProvider`, and `aik-anthropic` implements it a
second time — over HTTPS, against a service, which is what makes the contract a contract rather
than a description of Ollama, and what makes this the one crate holding a credential; `aik-policy` implements
`PolicyEngine`; `aik-tools` is the reference `ToolRegistry`, gating every call through
whichever `PolicyEngine` and `ApprovalSink` (`aik-approval`) it is given; `aik-fs` is the
first `Tool` implementation, and the first code in the workspace that touches the host
filesystem; `aik-exec` is the first whose subject is host code rather than a request it
carries out itself, and so the first that needs an enforcement boundary — namespaces — rather
than a check it makes on itself; `aik-context` implements `ContextStore`; `aik-scheduler` implements `Scheduler`, running
unattended work against the same shared database; `aik-summary` implements `ContextCompactor`
— the first crate here that is *beside* a subsystem rather than under it, composing the
context store with a model to do the one thing that store cannot do without becoming
fallible; `aik-agent` composes a `ModelProvider`,
a `ToolRegistry` and a `ContextStore` into a request/response loop; `aik-runtime` is the one
thing that assembles a real kernel out of all of them; `aik-daemon` is the long-lived process
that owns that kernel, the database under it and the schedule over it, and serves clients over
a local socket; `aik-cli` lets a human type a question, either into a kernel of its own or
into a running host. None of this layer is itself part of the kernel — see
[What deliberately is not in the kernel](#what-deliberately-is-not-in-the-kernel) — but it is
part of this repository, developed against `aik-api`'s contracts to keep them honest.

The dependency direction is what keeps the two frontends honest about assembly: `aik-runtime`
depends on every implementation crate and nothing depends back on it except `aik-cli` and
`aik-daemon`. Neither frontend can register a component the other does not, because neither
registers components at all.

## `aik-core` — the kernel

### Identity and errors (`id`, `error`)

Two families of strongly typed identifiers, both generated by macros so new ones are one
line each:

- **String ids** (`ComponentId`, `PluginId`, `EventName`) — stable, human-authored, used
  in configuration and dependency declarations. Backed by `Arc<str>`, so cloning is free.
- **UUID ids** (`EventId`, `TaskId`, `CorrelationId`) — generated, unique, time-ordered
  (UUIDv7) so they sort by creation.

A single `Error` enum with a coarse `ErrorKind` classification covers the kernel. It carries
component attribution and lifecycle phase for failures raised by components, and has an
escape hatch (`Error::wrap`) for downstream errors.

### Configuration (`config`)

`Config` is an immutable, cheaply cloned snapshot of a JSON tree assembled from ordered
layers (defaults → file → environment → explicit overrides), deep-merged. Access is by
dotted path and deserialises into any `serde` type.

The kernel is **format-agnostic on purpose**: it accepts `serde_json::Value` layers rather
than reading files. Whoever builds the kernel decides whether config comes from TOML on
disk, a database, or a UI. An environment-variable layer is provided because env vars are
portable and universally useful.

### Registry (`registry`)

The dependency-injection container. Maps `(type, name) → Arc<dyn Trait>`, so a caller asks
for a *capability* rather than a concrete type:

```rust
let provider: Arc<dyn ModelProvider> = ctx.registry().resolve()?;          // the default
let provider: Arc<dyn ModelProvider> = ctx.registry().get(&"openai".into())?; // by name
let all = ctx.registry().list::<dyn ModelProvider>();                      // discovery
```

This is the seam that makes every future subsystem replaceable without touching the kernel.

### Components and lifecycle (`component`, `kernel`)

A **component** is a lifecycle-managed unit. It declares an id and its dependencies, and
gets three phases:

- `init` — register services into the registry, subscribe to events. No activity yet.
- `start` — begin doing work, spawn background tasks.
- `stop` — release resources.

The kernel topologically sorts components by their declared dependencies (deterministically,
so start order is reproducible), runs all `init`s in order, then all `start`s. A failure
during startup rolls back everything already started, in reverse order, and — like an
ordinary shutdown — waits for whatever background tasks those components spawned to actually
finish before returning, not just for them to be told to stop. Shutdown is reverse order too.

Components take `&self`, never `&mut self`: they own their own interior mutability, which
keeps them shareable and avoids a kernel-wide lock.

### Events (`event`)

Typed publish/subscribe. An `Event` is a serialisable type with a stable `NAME`. Each event
type gets its own broadcast channel, so subscribers only pay for what they listen to, and
delivery is type-checked at compile time.

Because every event is serialisable, the bus also offers a **firehose** (`subscribe_any`)
that yields events as JSON with their metadata. This is how a Telegram bridge or a
Quickshell socket will observe the system without knowing any event types. Serialisation
only happens when someone is actually listening to the firehose.

Envelopes carry an id, a timestamp, the source component and an optional correlation id, so
a request can be traced across subsystems.

### Tasks (`task`)

Structured concurrency. `Tasks` combines a `TaskTracker` (so shutdown can wait for
completion) with a hierarchical `CancellationToken`. Each component gets a child scope: a
single component can be cancelled without touching the rest, and cancelling the kernel
cancels everything. Shutdown waits for all tasks with a configurable timeout.

### Plugins (`plugin`)

A `Plugin` is a unit of *registration*: given a registrar, it contributes components. This
covers compiled-in extensions today; dynamic loading later only needs a loader that produces
`Box<dyn Plugin>`, which is why plugin metadata carries an ABI version the kernel checks.

### Clock (`clock`)

Time is injected (`Arc<dyn Clock>`), so scheduling and anything time-dependent stays
testable. `SystemClock` and `ManualClock` are provided.

## `aik-api` — contracts for downstream subsystems

Object-safe, async trait definitions. Nothing in `aik-core` depends on `aik-api` — a kernel
can be built and run with none of these present — and `aik-api` itself implements nothing;
every "Implemented by" column below is a separate crate.

| Module       | Contract                                                              | Implemented by |
|--------------|------------------------------------------------------------------------|----------------|
| `execution`  | `ExecutionContext`: correlation, principal, deadline, cancellation     | — (a plain value type, not a trait) |
| `model`      | `ModelProvider`, `Embedder`, provider-neutral message/content types    | `aik-ollama` (both), `aik-anthropic` (`ModelProvider`) |
| `tool`       | `Tool`, `ToolCatalog`, JSON-Schema specs, invocation and outcome       | `aik-tools` (`ToolRegistry`), `aik-fs` and `aik-exec` (`Tool`), `aik-mcp` (`ToolCatalog`) |
| `context`    | `ContextStore`, `ContextBudget`, `TokenCounter`: transcript vs. model payload | `aik-context` |
| `context`    | `ContextCompactor`: replacing evicted turns with a recap, rather than losing them | `aik-summary` |
| `memory`     | `MemoryStore`: records, queries, optional embeddings                   | `aik-memory` (semantic query needs an `Embedder`) |
| `permission` | `PolicyEngine`, `ApprovalSink`, principals and decisions               | `aik-policy` (`PolicyEngine`), `aik-approval` (`ApprovalSink`) |
| `scheduler`  | `Scheduler`, `JobHandler`, triggers (at / after / interval / cron / event) | `aik-scheduler` |
| `agent`      | `Agent`, sessions, streaming updates                                   | `aik-agent` (`AgentLoop`) |
| `platform`   | `PlatformIntegration`: the single seam for Hyprland/Wayland/OS backends | not yet implemented |

These types are provisional by design and evolve as the subsystems built against them reveal
what the shape should actually be — `aik-tools`, `aik-fs`, `aik-agent` and the rest exist
in part to validate that this contract layer holds up against a real implementation, not
just a plan for one — `scheduler` gained an `ExecutionContext` on every method, and a job an
owner, precisely because building it showed that a schedule nobody owns cannot be isolated.
`platform` remains a shape with no implementation yet, deliberately not built ahead of the
evidence that would justify one.

`ToolCatalog` is the most recent of these to gain an implementation, and it did so without
gaining a method: it was written with an MCP server named in its documentation as the kind of
thing that would implement it, and `aik-mcp` is that thing arriving. What the exercise found
was not a missing method but a missing *seam*: nothing consumed a `ToolCatalog`, because
`aik-tools` could only be handed tools that already existed as values. A catalogue's tools do
not — they are discovered, asynchronously, from a program — so `ToolsComponent` gained a
`with_catalog` that drains one during `init`, before anything holds an `Arc<dyn ToolRegistry>`.
The guarantee the registry rests on is unchanged: the set of tools is still frozen by the time
it is reachable, and there is still no path that adds a tool to a registry already in use.

`ContextCompactor` is the only contract added *because* an existing one refused to grow. `ContextStore::compact` says, in the contract itself, that it will
never summarise: it is deterministic, model-free and cannot fail interestingly, and a store
that summarised would have to be all three of the opposite things. So the capability became a
second trait in the same module rather than a method on the first, implemented by a crate
that holds a `ContextStore` and a `ModelProvider` and adds no state of its own. What that
buys is the ability to say no in one place: an implementation is required to summarise before
it removes anything, so every failure mode — an unreachable model, an empty answer, a caller
who does not own the session — leaves a session holding more than it needs rather than less
than it had.

`Embedder` was the previous one to acquire an implementation, and it is worth noting
what that cost: nothing in `aik-api` changed shape, because the contract already said what an
embedder was. `aik-memory` gained an optional collaborator and `MemoryStore` gained a
`capabilities()` method with a default, so a store that cannot rank by meaning still compiles
and still says so — which is the same "refuse, never silently degrade" rule the rest of the
contract layer is written to.

## What deliberately is not in the kernel

`aik-core` itself knows nothing about UI, Quickshell, Hyprland, Wayland, any OS-specific
code, LLM clients, memory backends, tool implementations, agent loops, Docker, a CLI, bots,
databases, signal handling, or logging setup. Several of these now exist in this repository
as the implementation crates listed under [Workspace layout](#workspace-layout) — an LLM
client (`aik-ollama`), tool implementations (`aik-fs`), an agent loop (`aik-agent`), a CLI
(`aik-cli`) — but every one of them reaches `aik-core` only through the registry, the event
bus and the component lifecycle, exactly as any other downstream consumer would; none of
them is compiled into, or required by, `aik-core` itself. What genuinely is not here yet:
Quickshell, Hyprland/Wayland or any other platform integration, and any UI beyond the
terminal.
