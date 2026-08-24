# aik-cli: usage, testing and troubleshooting guide

This is the detailed companion to the [README](../README.md)'s "Actually using it" section. It
documents every `aik` command-line option with copy-pasteable examples, explains the security
model as it actually behaves (not just as designed), and records how the CLI was manually
validated against a real Ollama server.

If you only need the option table, see the [README](../README.md#actually-using-it). This
document is for building a mental model of the whole system and for troubleshooting.

`aik` has one subcommand, `aik audit`, which reviews the durable record of what the system was
allowed to do — see [The audit trail](#the-audit-trail-aik-audit).

There is also a second binary, `aikd`, the host process that owns the database and runs the
schedule. With one running, `aik` becomes a client of it rather than assembling a kernel of
its own — see [The host process](#the-host-process-aikd).

## Prerequisites

- Rust 1.85 or newer (edition 2024) — `rustc --version`
- A running [Ollama](https://ollama.com) server, local or remote — `ollama serve`
- At least one model pulled — `ollama pull llama3.1:8b`
- For tool-calling scenarios (the interesting ones), the model must report the `tools`
  capability. Check with `ollama show <model>`; look for `tools` under `Capabilities`. Not
  every model does — `qwen2.5-coder:7b`, for instance, answers tool-shaped requests as prose
  rather than issuing a structured call, which is a model choice the provider reports
  faithfully rather than a bug in `aik-ollama`. `llama3.1:8b` and the `qwen3` family reliably
  issue real structured tool calls.

## Building

```bash
cargo build -p aik-cli
```

The binary is `target/debug/aik` (or `target/release/aik` with `--release`). Everything below
assumes it is on your `PATH` or invoked by full path; examples use `aik` for brevity.

## Quick start

```bash
ollama pull llama3.1:8b
cargo run -p aik-cli -- --config crates/cli/aik.example.json --root .
```

[`crates/cli/aik.example.json`](../crates/cli/aik.example.json) is a real, working policy:
reads and directory listings are allowed outright, `.env` and `.ssh` are denied regardless of
anything else, writes are allowed as a capability but every individual file requires a human's
approval, recall is unconditional, memories may be recorded only under the three kinds it
names, and every deletion is put to a human. It is exercised by the test suite
(`durable.rs`, `the_shipped_example_configuration_describes_the_system_that_exists`), so it
cannot drift away from the system it describes without a test failing.

## Selecting a model

```bash
aik -m llama3.1:8b "what is in src?"
```

Without `-m`/`--model`, `aik` asks the provider for its first reported model and uses that —
non-deterministic across a machine with several models pulled, so pin one explicitly for
anything beyond casual use. The model can also be set in a config file (`cli.model`) or the
`AIK_CLI__MODEL` environment variable; the command line wins over both.

An unknown model name is not caught locally — the CLI has no model catalogue of its own — and
surfaces as whatever Ollama returns, typically an HTTP 404:

```
aik: Ollama returned HTTP 404 Not Found
```

## Interactive mode vs. one-shot mode

```bash
aik --config aik.json --root .                 # interactive
aik --config aik.json --root . "what is in src?" # one shot: run once and exit
```

Any positional arguments (everything after the recognised options, joined with spaces) make
the run one-shot. This is not just a convenience distinction — see
[Approvals](#approvals-and-the-one-shot-security-posture) below, because it changes what a
`require_approval` policy decision does.

In interactive mode:

- `/new` starts a fresh conversation (new session id; the old one is untouched and still in
  the store, just not attached to the prompt anymore — and, on a durable run, still there
  after the process exits).
- `/session` prints the current session id and who is acting.
- `/tools` prints the agent's declared tool set, or explains that it is unrestricted (offered
  whatever the tool registry lists).
- `/quit`, `/q`, `/exit` end the session. So does end-of-input (closing stdin, e.g. piping a
  script that runs out of lines, or pressing Ctrl-D at an interactive terminal) — this is not
  an error, just the ordinary way a piped session ends.
- Ctrl-C at the prompt ends the session immediately. Ctrl-C during a turn interrupts that turn
  (cancels the model call / tool invocation in flight) rather than the whole process; the
  session then continues at the next prompt.
- A blank line or an unrecognised `/command` does not reach the model at all.
- A failed turn (a tool error, a model failure, a context error) is reported inline and the
  session continues to the next prompt — a script driving `aik` interactively should not
  expect the process to exit just because one turn went wrong.

## The filesystem root

```bash
aik --root /home/user/project ...
```

Every filesystem tool this run registers is confined to this directory, resolved once at
startup (symlinks in the root path itself are followed at that point). Defaults to the current
directory. This confinement is enforced independently of policy — see
[Filesystem confinement](#filesystem-confinement) — a permissive policy narrows nothing about
it and cannot widen it either.

## Tool exposure: `--write` and `--no-tools`

| Flags | Tools registered |
|---|---|
| (none) | `filesystem.read`, `filesystem.list` |
| `--write` | `filesystem.read`, `filesystem.list`, `filesystem.write` |
| `--no-tools` | none — the agent can only talk |

`--no-tools` and `--write` together are a usage error (`aik: --no-tools and --write contradict
each other`), rejected before anything starts, rather than silently picking one.

`--no-tools` means *no tools*, memory included; see [Memory](#memory-what-the-agent-may-keep)
below.

This is the **outer** limit on what an agent can do: a tool that was never registered cannot be
reached however permissive the policy is. Policy is the **inner** limit, applied to whatever
was registered. Both apply; neither substitutes for the other. Concretely: without `--write`,
a model asking to write a file never reaches the tool registry at all — the agent loop reports
"no tool named `filesystem.write` is available to this agent" itself, because the tool name is
outside the fixed set the run was given, and the model just sees that as a normal tool-error
result it can react to.

## Memory: what the agent may keep

```bash
aik --memory recall ...    # get + query
aik --memory remember ...  # the default: get + query + put
aik --memory full ...      # also delete
aik --memory off ...       # no memory tools at all
```

| `--memory` | Tools registered |
|---|---|
| `off` | none |
| `recall` | `memory.get`, `memory.query` |
| `remember` (default) | `memory.get`, `memory.query`, `memory.put` |
| `full` | all four, including `memory.delete` |

This is the same **outer** limit `--write`/`--no-tools` is for the filesystem, applied to the
record store: a memory tool that was never registered cannot be reached however permissive the
policy is. Policy is the inner limit and applies to whatever was registered.

`--memory` combined with `--no-tools` is a usage error, for the same reason `--write` is.

### Why `delete` is not on by default

Deletion is the one memory operation that destroys evidence. A model that can forget on its
own can erase both what it was told to do and the record that it was told, so it is withheld
twice over: the tool is not registered unless `--memory full` asks for it, and the shipped
policy still routes every deletion through `require_approval`. Bounding how long a memory
lasts without destroying anything is what `ttl_seconds` on the record is for, and it needs no
tool.

### Nothing is recalled automatically

No memory is read into a prompt on the agent's behalf and nothing watches a conversation for
facts worth keeping. Every memory in the store is one a model asked for and a policy allowed,
which is what makes the audit trail complete. The consequence is that the model has to be told
its memory exists and when to use it — the shipped `agent.system_prompt` in
[`crates/cli/aik.example.json`](../crates/cli/aik.example.json) does exactly that, and is
where to change the deployment's own judgement about what is worth remembering.

The key sits in `agent`, not in `cli` or `daemon`, because it is a property of the deployment
rather than of whichever frontend happens to be running: a terminal and a host process over
the same database are the same assistant, and an instruction that reached only one of them
would be an agent that recalls in one and does not in the other. Both frontends read it
through `aik_runtime::system_prompt`, and both refuse a configuration that names it under
their own section rather than ignoring it.

### Ownership

A memory belongs to the principal that stored it, which for this frontend is the **agent**
identity (`-a`/`--agent`), not the user. Naming another principal's record is
`PermissionDenied`; enumerating simply does not return it, because an error would confirm it
exists. Delegation runs one way: a run started as `--agent helper --user assistant` is
`Principal::new("helper", Agent).on_behalf_of("assistant")` and can therefore reach
`assistant`'s memories, while `assistant` cannot reach `helper`'s.

## Persistence: the shared database

```bash
aik ...                        # durable, at the XDG default
aik --db /srv/aik/state.redb   # durable, somewhere specific
aik --ephemeral                # nothing reaches the disk
```

One [redb](https://github.com/cberner/redb) database holds four subsystems' state: the
conversation transcript (`aik-context`), the agent's memories (`aik-memory`), any scheduled
job marked persistent (`aik-scheduler`), and the audit trail (`aik-audit`). One file rather
than four because redb takes an exclusive lock per file and because one schema version cannot
disagree with itself.

The path is resolved with this precedence, highest first:

1. `--db <FILE>`
2. `components.store.db.path` in the configuration
3. `$XDG_DATA_HOME/aik/aik.redb`, falling back to `$HOME/.local/share/aik/aik.redb`

**Watch the nesting.** The component's id is `store.db`, and each dot in a component id is
another level of object, so the key is:

```json
{ "components": { "store": { "db": { "path": "/srv/aik/state.redb" } } } }
```

Writing `{"components": {"store.db": {...}}}` parses, starts, and silently opens the XDG
default instead.

With neither `XDG_DATA_HOME` nor `HOME` set and no path configured, `aik` refuses to start:

```
aik: components.store.db.path: neither XDG_DATA_HOME nor HOME is set, so there is no
default location for the database; set the path explicitly
```

That is deliberate. The working directory would drop a file holding every transcript wherever
the process happened to launch from, somewhere a backup or a repository could pick it up; a
temporary directory would lose it at the next reboot. Both are worse than not starting.

The file is created `0600` inside a directory created `0700`, and an existing file other
accounts can read is refused rather than used. A database written by a *newer* build of `aik`
is also refused, because an older binary writing to it would silently drop fields it does not
know about.

### `--ephemeral`

Opens no database. The context store, the memory store, the scheduler and the audit trail are
all wired to their in-memory implementations, publishing the same capabilities under the same
component ids, so nothing downstream can tell the difference except by outliving a restart.
The run is still audited — an unaudited run is not a thing `aik` offers — the trail simply
lives as long as the process does. A job asked
to be persistent is refused with `Unsupported` rather than accepted and forgotten — the one
outcome that would let a deployment believe its nightly job exists.

`--ephemeral` and `--db` together are a usage error.

### Releasing the file

redb's lock is held by the `Arc<Db>` the kernel registry owns, and the registry outlives
`Kernel::shutdown`. So a cleanly shut-down kernel is still holding the file; releasing it
means dropping the `Kernel`. In the binary that is process exit, so it never comes up; in a
test or an in-process restart it does, and the suite asserts it (`durable.rs`,
`shutdown_releases_the_database_so_the_next_run_can_open_it`).

## The host process: `aikd`

```bash
aikd --config crates/cli/aik.example.json --root .        # holds the database, runs jobs
aik  --socket "$XDG_RUNTIME_DIR/aik/aikd.sock" "hello"    # a conversation, through the host
aik audit --socket "$XDG_RUNTIME_DIR/aik/aikd.sock"       # the trail, through the host
```

### Why there is one

Two facts, and neither is negotiable:

- **redb locks the database.** Exactly one process may have it open, which is what makes a
  write spanning the transcript, the memories, the schedule and the audit trail one
  transaction. A second `aik` started while a host is running does not get a second view of
  the data; it fails to open it.
- **A schedule needs something that is always there.** A job that fires at 3am fires in
  whatever process is running at 3am. A terminal is not that process, and a terminal that
  *were* would interleave unattended agent turns with somebody's conversation. So a terminal
  run stores jobs and runs none of them; `aikd` runs them.

### What it serves

One kernel, assembled by exactly the same code a terminal run uses
([`aik-runtime`](../crates/runtime)), from the same kind of resolved settings. `aikd` adds a
lifetime and a door; it decides nothing about authorization that `aik` does not.

Clients ask for conversations, session listings, `clear`, `compact`, scheduled jobs and audit
queries. They cannot ask for anything else, and in particular:

- **No request names a principal.** There is nowhere on the wire to put one. Every
  `ExecutionContext` the host builds carries a principal derived from its own settings.
- **No request names a tool, a handler or a policy.** A scheduled job says *when* and *what to
  ask*; the host stamps the handler and the owner.
- **No response carries a tool's arguments or output.** The types on the wire are the kernel's
  own, with the limits their own documentation already states.

### Authentication

Three checks, in this order, and each is doing a different job:

1. **The filesystem.** The socket is mode `0600` in a directory mode `0700`, both owned by the
   account running the host, and a symbolic link in either position is refused rather than
   followed. This is the check the peer cannot influence, and the socket is bound under a
   temporary name and renamed into place so the path a client connects to never exists at any
   looser mode.
2. **Peer credentials.** The operating system reports the connecting account, before a byte of
   the peer's handshake is parsed. Another account is disconnected without being sent a
   protocol at all, and the refusal is logged — reaching the socket from another account means
   the directory's mode is not what it should be.
3. **A token.** 256 bits from `/dev/urandom`, written mode `0600` beside the socket and
   regenerated on every start, compared in constant time. Against an attacker who already runs
   as this account it proves nothing, and is not claimed to; what it stops is a *stale* or
   *confused* client — a process holding an old path, a script pointed at the wrong endpoint.

A client checks the host in the same way before handing over its token: same account, same
modes, no symlinks. Skipping that would make the token an anti-credential.

A version mismatch is only reported *after* the token is accepted, so a peer that has not
authenticated learns nothing about the host, and a malformed handshake is indistinguishable
from a wrong token.

### Where the socket lives

```bash
aikd --socket /run/user/1000/aik/aikd.sock   # explicit
AIK_SOCKET=/run/user/1000/aik/aikd.sock aikd # the environment
aikd                                         # $XDG_RUNTIME_DIR/aik/aikd.sock
```

With neither `--socket` nor `$AIK_SOCKET` nor `$XDG_RUNTIME_DIR`, `aikd` refuses to start
rather than falling back to a shared temporary directory, for the same reason the database
does: a socket somebody else can create first is a socket somebody else can be listening on.

`aik` reads the same `$AIK_SOCKET`, so the two cannot end up naming different sockets. It has
**no** default socket: with none configured, `aik` assembles its own kernel exactly as it
always did. A default that quietly turned every `aik` into a client of whatever happened to be
listening would change what the command is based on something nobody typed.

### Approvals

A client says at connection time whether a human is present. One that does holds an approval
gate for as long as the connection lasts, which is the same assertion an interactive terminal
session makes; one that does not gets the immediate refusal a one-shot run gets. A client that
disappears stops asserting, so a question asked afterwards has nobody to reach and is refused.

Questions are broadcast to every attached console and the first answer wins. That is a
property of a host with more than one console: routing a question to the connection whose call
caused it would silently refuse every scheduled job's approval, since the client that
scheduled it is usually long gone.

### Bounds and shutdown

`--max-clients` (default 16) bounds connections, counting those still in their handshake, so a
client in a restart loop cannot exhaust the host's file descriptors. A peer that never
completes a handshake is timed out. Each connection may have 8 calls in flight; the excess is
refused with an error naming the limit rather than by disconnecting.

`SIGINT` or `SIGTERM` stops the host: clients are told, work in flight is cancelled, parked
approvals are refused by the closing broker, and the socket and its token are removed. A
second signal is not special-cased — shutdown is already bounded, and an operator who wants it
to stop *now* has `SIGKILL`.

There is no `--daemonize`, no pid file and no log configuration. A service manager does that
better, and a process that hides itself hides its failures too.

### Reviewing the trail while a host is running

The database is locked, so `aik audit` cannot open it and says so, naming `--socket` as the way
through. Reading through the host reads under the same identity it would against the file —
the operator, not the agent — because a socket in a `0700` directory establishes exactly what
opening a `0600` file establishes. `--socket` together with `--db` or `--user` is a usage
error rather than a silently ignored flag: both would let somebody believe they had reviewed a
different trail, or reviewed it as somebody else.

`aik audit prune --socket ...` works too, and is the reason the host exposes a destructive
operation at all: the alternative would be an operator stopping the host in order to apply a
retention period, which is a strictly worse thing to make routine. It removes nothing a sweep
would spare, and the removal is itself recorded in the trail.

### There is no network listener

Unix sockets, peer credentials and file modes are the whole of the authentication story, and
none of the three exists on a network address. Remote access needs a transport identity that
is not a uid and a trust decision that is not a file mode; adding it means adding both.

## Configuration and policy files

```bash
aik --config aik.json ...
aik --config aik.json --policy policy.json ...  # policy.json overrides aik.json's "policy" key
```

`--config` is a JSON file layered under the environment (`AIK_*`) and over the compiled-in
defaults, read by the kernel's own layered `Config` mechanism — nothing CLI-specific about the
format. `--policy` reads a policy document on its own and layers it in as the `policy` key,
overriding whatever `--config` set there. Both are ordinary JSON; a missing file or invalid
JSON is a startup error naming the file and the parse problem, not a fallback to defaults.

A run with **no policy at all** is valid and denies every tool call — the banner says so
explicitly rather than the tool calls just failing mysteriously:

```
  policy: none configured, so every tool call will be denied.
          pass --policy <FILE> to allow anything.
```

### Writing a policy: the two-phase gotcha

**This is the single most common mistake when hand-writing a policy**, confirmed while
producing this document: authorization for a tool like `filesystem.read` is checked in **two
independent phases**, and a rule answers only the phase(s) its `resource` field is shaped for.

1. **Tool phase** — "may this principal use `filesystem.read` at all?" Asked with no resource.
   Only a rule that **omits** `resource`, or sets it to the literal `"*"`, can match this
   question.
2. **Resource phase** — "...on this specific path?" Asked with the tool's canonical,
   already-confined path. A rule with `resource` **omitted** never matches this question
   (only `"*"` or a specific pattern does).

A policy that only ever writes resource-specific rules —

```json
{ "rules": [
    { "action": "filesystem.read", "resource": "/home/user/project/secrets*",
      "effect": { "decision": "deny", "reason": "contains credentials" } },
    { "action": "filesystem.read", "resource": "/home/user/project/*",
      "effect": { "decision": "allow" } }
] }
```

— denies **every** read, including the ones the second rule looks like it allows, because
neither rule answers the tool-phase question and an unmatched question is a denial. The fix is
a companion capability-level rule, exactly as
[`crates/cli/aik.example.json`](../crates/cli/aik.example.json) does it:

```json
{ "rules": [
    { "action": "filesystem.read", "effect": { "decision": "allow" } },
    { "action": "filesystem.read", "resource": "/home/user/project/secrets*",
      "effect": { "decision": "deny", "reason": "contains credentials" } },
    { "action": "filesystem.read", "resource": "/home/user/project/*",
      "effect": { "decision": "allow" } }
] }
```

Using a bare `"*"` for `resource` collapses this into one rule that answers both phases, which
is what the [README](../README.md)'s minimal examples and `allow-all` test fixtures do. It is
fine for a broad allow; it is the wrong tool for carving out a specific exception, because a
`require_approval` rule with `resource: "*"` asks the human **twice** for the same write — once
at the tool phase, once at the resource phase — which is confusing in practice even though each
question is individually correct. Prefer: a plain `allow` at the tool phase, and
`require_approval` only at the resource phase, when the interesting decision is "approve this
specific write" rather than "may this principal write at all."

### Rule order is everything

Rules are tried top to bottom; the first match decides and nothing after it is consulted. A
specific deny must come before the general allow it is meant to carve out of — reversing the
two lets the general rule shadow the specific one silently. Verified directly: swapping the
order of the two `filesystem.read` rules above changes whether `secrets.txt` is readable, with
no error or warning either way. There is no rule-specificity heuristic; write the exceptions
first.

### Example policies

Deny everything (the default with no `--policy` at all — shown for clarity):

```json
{ "rules": [] }
```

Read-only, capability plus wildcard resource:

```json
{ "rules": [
    { "action": "filesystem.read", "resource": "*", "effect": { "decision": "allow" } },
    { "action": "filesystem.list", "resource": "*", "effect": { "decision": "allow" } }
] }
```

Read-write, writes require a human:

```json
{ "rules": [
    { "action": "filesystem.read", "resource": "*", "effect": { "decision": "allow" } },
    { "action": "filesystem.list", "resource": "*", "effect": { "decision": "allow" } },
    { "action": "filesystem.write", "effect": { "decision": "allow" } },
    { "action": "filesystem.write", "resource": "*",
      "effect": { "decision": "require_approval", "prompt": "let the agent write this file?" } }
] }
```

Memory: recall freely, record only under kinds this deployment recognises. The kind a memory
is stored under *is* the resource it is authorized as (`kind/<kind>`), so an invented kind
matches no rule and is denied — which is how a model is kept from opening an unbounded
namespace in a durable store nobody is watching:

```json
{ "rules": [
    { "action": "memory.get", "effect": { "decision": "allow" } },
    { "action": "memory.get", "resource": "*", "effect": { "decision": "allow" } },
    { "action": "memory.query", "effect": { "decision": "allow" } },
    { "action": "memory.query", "resource": "*", "effect": { "decision": "allow" } },
    { "action": "memory.put", "effect": { "decision": "allow" } },
    { "action": "memory.put", "resource": "kind/preference", "effect": { "decision": "allow" } },
    { "action": "memory.put", "resource": "kind/fact", "effect": { "decision": "allow" } },
    { "action": "memory.put", "resource": "kind/note", "effect": { "decision": "allow" } }
] }
```

A `memory.query` call that names *no* kind is asking for every kind, so it cannot honestly be
authorized as any single one: it claims the literal resource `kind/*` instead. The `"*"` rule
above matches that, so the unrestricted query is allowed here. Scope `memory.query` to named
kinds instead —

```json
{ "action": "memory.query", "resource": "kind/note", "effect": { "decision": "allow" } }
```

— and the unrestricted query is denied, because `kind/note` does not match `kind/*`. The query
then has to say what it is looking for.

Ownership is applied *after* the policy has spoken, and independently of it: a query returns
only records the calling principal owns or is acting for, so no rule here widens what anyone
can see.

Resource-specific carve-out (see the two-phase note above for why the first rule is required):

```json
{ "rules": [
    { "action": "filesystem.read", "effect": { "decision": "allow" } },
    { "action": "filesystem.read", "resource": "/home/user/project/secrets*",
      "effect": { "decision": "deny", "reason": "contains credentials" } },
    { "action": "filesystem.read", "resource": "/home/user/project/*",
      "effect": { "decision": "allow" } }
] }
```

Principal-specific: only the agent, never a person acting directly, may write —

```json
{ "rules": [
    { "principal": { "kind": "agent" }, "action": "filesystem.write",
      "effect": { "decision": "allow" } },
    { "principal": { "kind": "agent" }, "action": "filesystem.write", "resource": "*",
      "effect": { "decision": "allow" } }
] }
```

(No rule matches a `user`-kind principal at all here, so a direct call as the user — not
something the CLI itself ever does, since every turn runs as the agent, but relevant if this
policy is reused elsewhere — is denied.)

A policy that tries to reach outside the configured root has no effect: `resource` patterns
are just string matchers over whatever canonical path the *tool* declares, and the filesystem
tools canonicalise and confine every path to their root **before** policy is even consulted.
An allow rule for `/etc/*` does not make `filesystem.read` able to read `/etc/passwd` from a
tool confined to `/home/user/project` — the tool never produces `/etc/passwd` as a resource to
ask about; the call is refused as `path resolves outside the tool's allowed root` before
authorization runs at all. Confirmed directly: `--policy` set to allow everything (`"*"`) still
refuses an absolute path or a `../` traversal (`Error::InvalidArgument` — malformed input,
rejected on syntax alone) and a symlink whose target resolves outside the root
(`Error::Confinement` — the tool's own boundary, kept distinct from a malformed request so an
audit consumer can tell a genuine escape attempt from a typo; see
[Filesystem confinement](#filesystem-confinement)), never reaching a permission decision.

## Approvals and the one-shot security posture

`require_approval` is answered by whoever holds an `ApprovalGate`. **Interactive mode holds
one for the life of the session; one-shot mode never attaches one at all.** This is the
security-relevant difference between the two modes, not merely a UX one:

```
$ aik --policy approval-required.json --write "write hello.txt"
  → filesystem.write {"path":"hello.txt", ...}
  ✗ {"kind":"permission","message":"permission denied: no approval responder is attached, so nobody can answer"}
```

There is no timeout wait, no hang — the broker refuses immediately because it can prove nobody
is listening. Scripted, unattended use of `aik` therefore needs a policy that says `allow`
outright for anything it needs to do; `require_approval` is only useful with a human at the
terminal.

In an interactive session, a pending question looks like this:

```
  ⚠ let the agent write this file?
    action:   filesystem.write
    resource: /home/user/project/hello.txt
    asked by: assistant (for user)
  allow? [y/N]
```

Only an unambiguous `y` or `yes` (case-insensitive, whitespace-trimmed) grants it. Verified
directly: a blank line, `n`, `no`, `sure`, `ok`, `1`, `true`, and gibberish all refuse, and so
does closing stdin (EOF) while the question is outstanding — the console read returns nothing,
which is treated as an explicit "no" rather than as a hang. The tool's arguments are
deliberately never shown in the prompt (only the action, the resource and the trusted prompt
text a policy author wrote) — a model that wanted to forge the appearance of a different
question by stuffing escape sequences or fake text into its own arguments has no surface to do
it on. Verified directly with a file containing literal `ESC[2K` and bare `\r` bytes: the CLI
never printed the raw control bytes anywhere, tool output included — everything untrusted
(assistant text, tool arguments, file contents returned by a tool) passes through a sanitiser
that turns control characters into a visible `\u{XXXX}` escape before it reaches the terminal.

Approval cannot widen what confinement or policy already refused. Verified directly: a
`require_approval` write policy plus `y` at the prompt still refuses a `../` traversal target
before any question is even asked (the path is rejected while building the resource claim, a
step that runs before authorization), and a permissive read policy still cannot get a tool to
canonicalise `/etc/passwd` from a root it isn't under.

## Verbose mode

`-v`/`--verbose` prints four kinds of events as they happen, alongside the normal
conversation output:

```
  [ctx]  stored 244 — included 244 (11 records), elided 0 (0 parts), evicted 0 (0 records)
  [req]  turn 2 — system 0, tools 301 (2 offered), conversation 244, total 545 (estimated)
  [req]  provider usage: 337 in / 20 out (exact, as reported)
  [req]  model latency: 401ms
  [auth] agent-1 (Agent) Tool filesystem.write → Allowed (0ms)
  [auth] agent-1 (Agent) Resource filesystem.write on /home/user/project/hello.txt → ApprovalGranted (118ms, 115ms of it waiting on approval)
  [tool] agent-1 (Agent) filesystem.write → Succeeded (4ms exec, 118ms auth)
```

- `[ctx]` — one per model turn, from the context store's `ContextAssembled` event: how many
  tokens are stored in total, how many were included in the window sent to the model, how many
  were elided (and how many parts that touched), and how many were evicted (and how many whole
  records that touched) to fit the budget.
- `[req]` — one per model turn, from the agent loop's own `RequestMeasured` event: a locally
  *estimated* breakdown of the request (system/instructions, tool definitions, conversation,
  total — labelled `(estimated)` because none of it is the provider's real tokenizer), the
  provider's own usage figures when it reports them (labelled `(exact, as reported)`), and how
  long the model call itself took. This is the one line `[ctx]` cannot provide on its own: tool
  definitions are attached to the request by the agent loop, not read from the context store, so
  their cost is invisible to `ContextAssembled` — see
  [`docs/MEASUREMENTS.md`](MEASUREMENTS.md) for why.
- `[auth]` — one per authorization question, prefixed with who was asking (the principal id and
  kind, plus `on behalf of <id>` when the principal is acting under delegated authority — see
  `AuthorizationDecided.on_behalf_of`), then the phase (`Tool`, `Resource`, or
  `DiscoveredResource` — see [Filesystem confinement](#filesystem-confinement) for what the
  third phase is), the outcome (`Allowed`, `Denied`, `ApprovalGranted`, `ApprovalRefused`,
  `ApprovalUnavailable`, `PolicyUnavailable`) and how long the decision took, in parentheses.
  For an approval-related outcome, the parenthetical breaks out how much of that time was
  specifically spent waiting for a human to answer — see
  `AuthorizationDecided.approval_wait_ms`, which is `None` (and so omitted here) whenever no
  approval sink was ever asked.
- `[tool]` — one per completed (or refused, or not-found) invocation, prefixed the same way with
  who was asking, with execution and authorization time in parentheses where they apply.

At the end of each turn, and again at the end of the whole session, a cumulative line is
printed:

```
  [2 turns, 1 tool calls, 337 in / 20 out tokens, window 244 tokens]
  [session] 6 turns, 3 tool calls, 2647 estimated tokens total, provider 2263 in / 152 out (exact)
  [session] latency — model 3044ms, tools 0ms, authorization 0ms (approval 0ms)
```

These are the kernel's own events (`AuthorizationDecided`, `ToolInvoked`, `ContextAssembled`,
`RequestMeasured`), the same ones a durable audit sink or `--record` (below) would subscribe
to; `-v` is a debugging convenience, not a separate mechanism. Verified directly: every
filesystem write, denial and elision produced exactly the events this section describes, in
the order they happened, with correct phase labels (a `filesystem.list` on a directory produces
one `Tool`, one `Resource` for the directory itself, and one `DiscoveredResource` per entry it
found — visible directly in `-v` output).

The verbose renderer also prefixes each `[auth]`/`[tool]` line with who was asking — the
`principal`/`on_behalf_of` fields the underlying `AuthorizationDecided`/`ToolInvoked` events
carry — so delegated-identity behaviour (e.g. `assistant (Agent, on behalf of user)`) is visible
directly in `-v` output, not only in the raw events. Identity is correctly recorded and enforced
independently of this rendering (confirmed by the audit-attribution test suite in
`crates/cli/tests/security.rs`, and independently by `aik-tools`'s own tests).

## Structured recording: `--record`/`-R`

```bash
aik --record run.jsonl ...
```

Appends one JSON object per measurement event to `run.jsonl` (created if it does not
exist), for the same events `-v` renders as text. Intended for later analysis rather than
for reading directly. It never carries prompts, assistant text, tool arguments, tool
results, file contents, resource paths, or policy-authored reasons — see
[`docs/MEASUREMENTS.md`](MEASUREMENTS.md#privacy-what-is-and-is-not-recorded) for the exact
list and the tests that enforce it, and `crates/cli/src/recorder.rs` for the format. A
destination that cannot be opened is a startup error, named, like a malformed config file; a
write that fails mid-run disables recording for the rest of the process after printing
exactly one message, rather than retrying forever or claiming success it did not have.

## The audit trail: `aik audit`

Every authorization decision and every tool call is written to the shared database as it
happens, and `aik audit` is how you read it back afterwards.

```bash
aik audit                                # the 50 most recent records
aik audit --refused                       # only what was not allowed
aik audit --tool filesystem.write         # only one tool
aik audit --since 12h --limit 200         # a window, widened
aik audit --correlation <ID>              # one operation, decisions and call together
aik audit --json                          # one JSON object per line, for piping
aik audit --help                          # every option
```

A record looks like this:

```
#412    2025-08-22T17:19:04Z  authorization  assistant for alice  filesystem.read  allowed  /srv/notes.txt
#413    2025-08-22T17:19:04Z  invocation     assistant for alice  filesystem.read  ok  7ms

2 record(s) shown as `alice`; 413 issued in total.
```

`#412` is the sequence number: it starts at 1, increases by one per record, and is never
reused — including across restarts, and including after a prune. A break in the numbering is
something to look into.

There is **no audit tool**. Every other durable subsystem is reachable by the agent through
the tool registry so that policy is consulted; the audit trail is the record of those
consultations, and a model able to read it would be reading a map of where the boundaries are
and where somebody has already been refused. The only path to it is this command, run by a
person, against a file mode `0600` in a directory mode `0700`. The test that pins this is
`no_tool_exposes_the_audit_trail_to_a_model` in `crates/cli/tests/audit.rs`.

### What you can see

The trail shows what the reading identity did **and what was done on its behalf**. `aik audit`
reads as the user (`--user`, default `user`), and a run is the agent acting for that user, so
by default you see the whole of what your agents did for you. Another principal's records are
not shown, and naming them in `--principal` narrows what you may already see rather than
widening it.

Two kinds of record are shown to every reader, and no `--principal` filter hides them:

| Line | What it means |
|---|---|
| `gap` | The event bus dropped events before the audit sink could read them. That many records are missing and cannot be recovered. |
| `retention` | A prune or a background sweep removed records at or before a cutoff. |

Both exist because a trail that is quietly incomplete is worse than no trail: it reads as a
complete account of a period it cannot account for. Neither is ever removed by a sweep, and
neither can be filtered away by asking about a principal. Asking for one `--kind` or one
`--correlation` does exclude them, because that is what you asked for — `aik audit --kind gap`
lists them on their own.

### Retention, and pruning

Nothing is ever removed unless somebody asks. There is no default retention period, which is
the opposite of the choice made for, say, a cache — retention here destroys the record of what
the system was allowed to do, and a default that discarded last month's decisions would be a
compliance bug shipped as a convenience.

Two ways to ask. A background sweep, configured per deployment:

```json
{ "components": { "audit": { "store": { "retention_days": 90 } } } }
```

(Note the nesting, for the same reason `store.db` needs it: the component's id is
`audit.store`.) `sweep_interval_seconds` overrides how often it runs; the default is hourly,
and nothing observable depends on it because a record stays readable until it is removed. A
`retention_days` of `0` is refused at startup rather than taken literally.

Or explicitly, once:

```bash
aik audit prune --older-than 90d --dry-run   # how many would go
aik audit prune --older-than 90d             # remove them
```

`--older-than` has no default and no short form. Periods take a unit — `90s`, `30m`, `12h`,
`7d`, `4w` — and a bare number is refused rather than guessed at, because this is a flag that
destroys evidence and "30" could plausibly mean three different things.

Either way the sweep leaves a `retention` record saying what it removed and from when, and
never removes a `gap` or a previous `retention` record.

### What is not recorded

The trail stores the published events verbatim, and those carry the shape of what happened,
never its contents: **no tool arguments, no tool output, no file contents, no prompts, no
model text**. A resource identifier — the path a tool was authorized to touch — *is* recorded,
because an audit trail that omits what was touched is not one. Policy-authored denial reasons
are recorded too, since they are written by the policy engine rather than by a model.

### The honest limitation

The audit write happens on a subscriber, after the decision, not inside it. So an audit store
that cannot be written to — a full disk, a database that will not open — does **not** refuse
the tool call; it logs at `error` level and the trail is short by that record. Making the
write synchronous would mean putting a disk inside `ToolRegistry::invoke`, which is a change
to the enforcement point rather than to the trail, and is deliberately not what this does.
Events the *bus* drops are a different matter and are recorded as a `gap`.

## Filesystem confinement

Every filesystem tool resolves and confines its target independently of policy, in this order:

1. **Syntax.** Absolute paths, empty paths, `.`/`..` segments and embedded NUL bytes are
   rejected before any filesystem access.
2. **Canonicalisation.** The remaining path is joined onto the root and resolved with
   `canonicalize`, following every symlink including in intermediate directories.
3. **Containment.** The canonical result must still be inside the canonical root — a
   component-wise check, not a string prefix, so a sibling directory named
   `project-secret` cannot be mistaken for something under `project`.

The write tool goes further, because a misdirected write is not undoable: on Linux/Unix it
opens the parent directory as a handle (`O_DIRECTORY | O_NOFOLLOW`), re-verifies that handle
against the root via `/proc/self/fd`, and then creates the file *through that handle*
(`openat`, `O_NOFOLLOW` on the final component) rather than by reopening a path — so a symlink
swapped in at the last moment, or a directory renamed after being checked, cannot redirect the
write. A target with more than one hard link is refused outright, since a second name for the
same inode could sit outside the root without any path ever showing it.

Every refusal above — steps 2/3 above, the write tool's handle re-verification, its final-
component symlink refusal, and its hard-link refusal — is reported as `Error::Confinement`,
not `Error::InvalidArgument`. The two are deliberately distinct classifications
(`ErrorKind::Confinement` vs. `ErrorKind::InvalidArgument` in `crates/core/src/error.rs`):
step 1's syntax rejections and other malformed input (a missing field, a non-string path) never
resolved anything, so they are `InvalidArgument`, while everything in this section resolved a
path and then refused what it found — the audit trail (`InvocationOutcome::Failed { kind:
"confinement" }` on `ToolInvoked`, see [Verbose mode](#verbose-mode)) lets a consumer alert on
an actual escape attempt without also matching every typo'd filename.

Directory listing (`filesystem.list`) authorizes the directory itself up front, then each entry
individually as it is discovered while reading it — the `DiscoveredResource` phase visible in
`-v` output. A refused entry is simply left out of the listing; it does not fail the call, so a
directory containing one restricted item still lists everything else.

Verified directly against a real symlink pointing outside the configured root
(`ln -s /outside/secret.txt ./escape-link`): reading it, writing through it, and listing a
directory containing it all behave exactly as documented — read and write both refuse it with
`Error::Confinement` (`path resolves outside the tool's allowed root` / `the path's final
component is a symlink; this tool never writes through one`), and listing reports it as a
`symlink` entry without following it.

**What this does not close, by design:** the window between resolving a path and the syscall
that acts on it is a property of the POSIX filesystem API, not of any one process's care.
Nothing at this layer can fully close it without an enforcement boundary outside the process —
a container, a namespace, `openat2(RESOLVE_BENEATH)`. See the `aik_api::tool` module docs and
[`FsWriteTool`](../crates/fs/src/write.rs)'s doc comment for the full discussion; this is a
documented, deliberate limitation, not a gap discovered during this review.

## Failure and deny behaviour

- **A denied or failed tool call does not end the run.** The model sees a structured error
  result (`{"kind": "...", "message": "..."}`) and can react to it — ask something else, try a
  different path, or just tell the user what went wrong. Verified in both one-shot and
  interactive runs: a tool denial is followed by a normal assistant turn, not a crash.
- **A hard model-provider failure (unreachable server, non-2xx response, malformed reply) is
  different: it ends the turn.** In interactive mode the session carries on to the next prompt
  after printing the error. **In one-shot mode this now ends the process with a non-zero exit
  code** — see [Known limitations and fixes](#known-limitations-and-fixes-made-during-this-review)
  below; this was a real bug found and fixed during this review.
- **Startup failures** (a malformed config or policy file, a kernel wiring error, an unreadable
  root) are reported before anything runs, with exit code 1, naming the file or setting at
  fault.
- **A usage error** (an unknown flag, a missing value, contradictory flags) is reported with
  exit code 2, without touching the network or the filesystem at all.
- Every printed error includes its full chain of causes as of this review (see below) — e.g.
  `aik: sending a completion request to Ollama: error sending request for url (...): tcp
  connect error: Connection refused (os error 111)` — rather than only the outermost, often
  unhelpful, context string.

## Delegated identity

Every turn runs as `Principal::new(<agent>, Agent).on_behalf_of(<user>)` — never as the user
directly. `<agent>` defaults to `assistant`, `<user>` to `user`; both are overridable
(`-a`/`--agent`, `-u`/`--user`) and are exactly what a policy's `principal` matcher sees. This
lets a policy say "the person may do X, and the thing acting for them may not" — write such a
rule with `"principal": {"kind": "user"}` vs. `"kind": "agent"`, or match a specific id. The
context store (the conversation transcript) is likewise owned by the agent principal, not the
user: a session written by one agent identity is not readable by a different principal, checked
on every access.

## Context and session behaviour

- Each turn appends the user's input, then the model's response (including any tool calls), to
  the session's transcript — an append-only, full-fidelity record, kept in the shared database
  and therefore still there after a restart (see
  [Persistence](#persistence-the-shared-database); `--ephemeral` keeps it for the life of the
  process instead).
- What is actually *sent* to the model each turn is recomputed from that transcript under a
  budget (default 8192 tokens, 1024 tokens per part) — not the raw history. Oversized tool
  results are elided (the bulk replaced by a marker naming the record, the full value still
  retrievable from the store) before older turns are evicted, and eviction keeps a contiguous
  run of the most recent turns plus anything pinned (the system prompt), never leaving gaps.
- `/new` starts a session with a fresh id; the old one still exists in memory (nothing is
  freed) but is no longer the one the prompt writes to.
- A tool result whose call was evicted from the window is dropped along with it, since a result
  answering nothing is a malformed request most providers reject — verified directly by reading
  far enough into a conversation, under a tight budget, that this had to happen without error.

## Token and context cost: a baseline

**Superseded by [`docs/MEASUREMENTS.md`](MEASUREMENTS.md)**, which re-measures all of this
against the current repository using a dedicated `RequestMeasured` event and a
machine-readable `--record` JSONL sink added specifically to make the numbers reproducible,
rather than transcribed from a terminal by hand. The summary below is kept for history; see
`docs/MEASUREMENTS.md` for the current numbers, the exact-vs-estimated distinction for each
one, and the commands to reproduce them.

Measured against a real Ollama server (`llama3.1:8b`, `qwen2.5-coder:7b`), for future
optimisation work — nothing here has been optimised yet, by design, per the scope of this
review.

**Tool definitions dominate the request size for small conversations.** With an identical
one-line prompt that never triggers a tool call (so the conversation body itself is a fixed 15
tokens by the harness's own accounting):

| Tools registered | Ollama-reported input tokens |
|---|---|
| none (`--no-tools`) | 23 |
| read + list (2 schemas) | 398 |
| read + list + write (3 schemas) | 543 |

That is roughly 375–520 tokens of fixed overhead from tool schemas alone, against 15–23 tokens
of actual conversation — and **the CLI's own `[ctx]` window-token metric does not include this
cost at all**, since it only reflects records read from the `ContextStore`; tool definitions are
attached to the `CompletionRequest` fresh by the agent loop every turn and never go through the
context budget. A `-v` session that reports "77 tokens" for its window can legitimately be
sending 500+ tokens to the model once tool schemas are counted.

**Tool definitions and the system prompt are resent in full on every single turn.** Nothing
about the request is cached or diffed between turns — this is inherent to `ModelProvider`
taking a plain `Vec<Message>` per call with no session concept on the wire, and to Ollama's
`/api/chat` endpoint being stateless per request. A run with a system prompt and three tools
pays for that fixed content on every turn, not just the first.

**Tool results already in the transcript are resent verbatim on every subsequent turn**, until
elided (oversized) or evicted (budget exceeded) — this is visible directly in `-v` output as
`window tokens` growing turn over turn in a multi-step conversation (48 → 124 → 228 tokens
across three turns of listing, reading and writing in the same session, in one of the scenarios
run for this review). This is the quadratic-in-turns cost `aik-context`'s own documentation
names as the problem it exists to bound — bound, not eliminated: the *window* is capped by the
budget, but nothing deduplicates a tool result the model already saw against the same result
resent because it is still inside the window.

No token-saving mechanism was implemented as part of this review, per its explicit scope; these
numbers are a starting point for that work, not a report of it being done.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `policy: none configured, so every tool call will be denied.` | No `--policy`/`--config` with a `policy` section was given. Every tool call will fail with `no policy rule matched this request` until one is. |
| A tool call is denied with `no policy rule matched this request`, even though a resource rule for it exists | The [two-phase gotcha](#writing-a-policy-the-two-phase-gotcha): the tool-phase (capability) question has no matching rule. Add a companion rule with `resource` omitted or `"*"`. |
| `approvals: refused (no responder attached in one-shot mode)` followed by every `require_approval` tool call failing | Expected in one-shot mode — see [Approvals](#approvals-and-the-one-shot-security-posture). Either answer interactively or change the policy to `allow` for scripted use. |
| `aik: Ollama returned HTTP 404 Not Found` | The model name does not exist on the target server — `ollama list` to check, or `ollama pull <model>`. |
| `aik: sending a completion request to Ollama: ... Connection refused` | Ollama is not running, or `endpoint` (default `http://localhost:11434`) points somewhere unreachable. Check with `ollama serve` / `curl localhost:11434/api/version`. |
| A model narrates a plausible-looking answer instead of calling a tool, even though tools are registered | The model does not support tool calling, or chose not to use it — check `ollama show <model>` for the `tools` capability. This is model behaviour, not a harness fault: `aik-ollama` only translates tool calls the model actually issues. |
| A model backfills a *plausible but entirely fabricated* answer after a tool call was refused | Observed directly during this review (asked to read `/etc/passwd`, correctly refused, the model then wrote out a generic, invented `/etc/passwd`-shaped example unprompted). Not a security issue — no real data was involved or disclosed — but a reminder that assistant prose after a refusal is not to be trusted as if it came from the tool. |
| `aik: --no-tools and --write contradict each other` | Both flags were passed; pick one. |
| A model's own hallucinated tool-call-shaped text appears as plain assistant output rather than a real tool call | The model attempted to call a tool that was never registered (e.g. `--write` was not passed) or does not support structured tool calling for this request; it is not offered a schema for that name, so it can only narrate. No unregistered tool is ever reachable regardless of what the model writes. |

## Known limitations and fixes made during this review

Two implementation bugs were found and fixed while producing this document; both are covered
by new regression tests and the full verification suite (`cargo fmt --all --check`,
`cargo check --workspace --all-targets`, `cargo test --workspace`,
`cargo clippy --workspace --all-targets --all-features -- -D warnings`,
`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`) was re-run clean
after each.

1. **One-shot runs always exited 0, even when the model call failed outright.** `Session::turn`
   caught every stream error internally, printed it, and returned `Ok(())` — correct for
   interactive mode (the session should carry on to the next prompt), but it meant
   `Session::one_shot` — which has no next prompt to carry on to — could never report failure to
   its caller. A script piping `aik --policy allow.json "do X"` into a pipeline had no way to
   detect an unreachable model, a denied tool, or any other turn-ending failure via exit code;
   only pre-turn setup errors (bad config, bad policy file) ever produced a non-zero exit.
   Fixed by having `turn()` propagate the error instead of swallowing it; the interactive
   call site already had its own catch-and-continue handling for exactly this (written, but —
   before this fix — never actually reachable, since `turn()` never returned `Err` for this
   case). See `crates/cli/src/session.rs`.
2. **Printed errors showed only the outermost context, discarding the actual cause.** `Error`'s
   `Display` deliberately prints only its own context string (confirmed intentional via
   `aik-core`'s own test suite — `source()` is meant to carry the chain instead), but `aik`'s
   top-level error reporting used `{error}` directly and never walked that chain. A connection
   failure printed as `aik: sending a completion request to Ollama`, with no indication of
   *why* — DNS failure, connection refused, TLS error and a dozen other causes all looked
   identical. Fixed by walking `std::error::Error::source()` in `aik`'s top-level error
   reporting. See `crates/cli/src/lib.rs`.

## Other known limitations (not bugs)

These are documented, deliberate properties of the current implementation, not defects:

- **The approval broker is in-memory.** A restart refuses every approval that was mid-flight.
  This is correct rather than a gap: an approval is a question put to a person who is no
  longer there, and reviving one across a restart would mean acting on consent nobody gave in
  this process.
- **Sessions persist, but the frontend does not resume one.** The transcript survives a
  restart in the shared database, and `/new` still starts a fresh session id every run; there
  is no `--session <ID>` to reattach to a previous conversation. What survives is readable and
  owned, not automatically reopened.
- **No summarisation or semantic memory.** Context management is purely mechanical (elision and
  eviction under a byte-heuristic token count); nothing invents or compresses text. Memory
  retrieval is exact — by id, by kind, by metadata equality — and a query asking for semantic
  ranking gets `Unsupported` rather than a result that quietly ignored the request.
- **Nothing schedules anything by default.** The scheduler is wired and its persistent jobs
  survive a restart, but the frontend registers no job handlers and offers no tool for
  scheduling, so a stock `aik` has an empty schedule. Handlers are contributed by components,
  which is what `crates/cli/tests/durable.rs` does.
- **The heuristic token counter is an estimate**, not the model's real tokenizer — Ollama's
  reported `prompt_eval_count` and the CLI's own `window tokens` figure routinely differ by an
  order of magnitude once tool schemas are counted (see
  [Token and context cost](#token-and-context-cost-a-baseline)).
- **The TOCTOU window between resolving a path and acting on it is not fully closed** — a
  documented property of the POSIX filesystem API, mitigated but not eliminated by the write
  tool's handle-pinning; see [Filesystem confinement](#filesystem-confinement).
- **No process execution, no OS-level sandboxing.** Out of scope for the kernel as it stands
  today, and out of scope for this review, per its explicit constraints.
