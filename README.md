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

`aik-core` does not depend on `aik-api`, and neither depends on `aik-ollama`. A kernel can
be built and run with none of the subsystem contracts present, and with no model provider
registered at all.

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
(`aik-ollama`), proving the registry/component architecture hosts a real, cancellable,
streaming integration cleanly. What comes next builds *on* the kernel rather than *into*
it: further `aik-api` contracts (tools, memory, agents), each as a component in its own
crate, following the same pattern.
