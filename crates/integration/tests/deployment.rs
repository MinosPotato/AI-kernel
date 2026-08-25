//! One configuration file, two frontends, one deployment.
//!
//! A terminal and a host process over the same database are the same assistant. Everything
//! that decides *which* assistant — the agent's identity, the person it acts for, the
//! directory it is confined to, the model that answers, the instructions it is given — is a
//! property of the deployment, and each of them used to be read by each frontend out of a
//! section of its own. Two readers of two keys agree only by coincidence, and the shipped
//! configuration file had to name both:
//!
//! ```text
//!   "cli":    { "agent": "assistant"   }   ← what one process resolved
//!   "daemon": { "agent": "aikd-agent"  }   ← what the other resolved
//! ```
//!
//! Nothing rejected that. The host recorded memories owned by `aikd-agent`, the terminal
//! searched as `assistant` and found none, and both were behaving exactly as configured. The
//! same shape of mistake in `user` hid audit records from `aik audit`, and in `root` moved the
//! filesystem confinement boundary depending on which process happened to be running.
//!
//! So the tests here are not "do the two readers agree today". They assert the property that
//! makes the question uninteresting: there is one reader,
//! [`aik_runtime::Deployment::resolve`], both frontends call it, and the keys the old readers
//! used are now unknown fields that stop the process.
//!
//! This lives outside both frontends deliberately. A test that only ever asks one of them what
//! it read is the test that let the two drift apart in the first place.

use std::path::{Path, PathBuf};

use aik_cli::settings::Settings;
use aik_core::Error;
use aik_daemon::settings::DaemonSettings;
use aik_runtime::{ExecSet, MemorySet, RuntimeSettings, Storage, ToolSet};
use serde_json::{Value, json};

/// An XDG data root that exists nowhere.
///
/// Settings resolution never touches the filesystem for this, and no test here starts a
/// kernel, so a path that cannot exist is the safest one to resolve against: anything that did
/// open it would fail loudly rather than reach the database of whoever ran the suite.
const FAKE_DATA: &str = "/nonexistent/xdg-data-home";

/// The same, for the socket a host would bind.
const FAKE_RUNTIME: &str = "/nonexistent/xdg-runtime-dir";

fn env() -> Vec<(String, String)> {
    vec![
        ("XDG_DATA_HOME".to_owned(), FAKE_DATA.to_owned()),
        ("XDG_RUNTIME_DIR".to_owned(), FAKE_RUNTIME.to_owned()),
    ]
}

/// Writes `config` to a file in `directory` and returns its path.
fn write(directory: &Path, config: &Value) -> PathBuf {
    let path = directory.join("aik.json");
    std::fs::write(&path, config.to_string()).expect("a configuration file");
    path
}

/// What the terminal frontend resolves from `config`, with no flags of its own.
fn terminal(config: &Path) -> Result<RuntimeSettings, Error> {
    Settings::resolve_from(
        &aik_cli::args::Options {
            config: Some(config.to_path_buf()),
            ..Default::default()
        },
        env(),
    )
    .map(|settings| settings.runtime)
}

/// What the host frontend resolves from the same file, likewise.
fn host(config: &Path) -> Result<RuntimeSettings, Error> {
    DaemonSettings::resolve_from(
        &aik_daemon::args::Options {
            config: Some(config.to_path_buf()),
            ..Default::default()
        },
        env(),
    )
    .map(|settings| settings.runtime)
}

/// The configuration this repository actually ships, which `docs/CLI.md` starts both with.
fn shipped() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("cli")
        .join("aik.example.json");
    let text = std::fs::read_to_string(&path).expect("the example configuration is readable");
    serde_json::from_str(&text).expect("the example configuration is valid JSON")
}

/// Asserts that two resolutions describe the same deployment.
///
/// Field by field rather than by one comparison, so a failure names the setting that drifted
/// instead of printing two settings structs.
fn same_deployment(terminal: &RuntimeSettings, host: &RuntimeSettings) {
    same_agent(terminal, host);
    assert_eq!(terminal.storage, host.storage, "where durable state lives");
    assert_eq!(terminal.tools, host.tools, "which filesystem tools exist");
    assert_eq!(terminal.memory, host.memory, "which memory tools exist");
    assert_eq!(terminal.exec, host.exec, "whether programs may be run");
    assert_eq!(
        terminal.exec_settings, host.exec_settings,
        "which programs may be run, and how",
    );
}

/// Asserts that two resolutions describe the same *assistant*.
///
/// Everything a memory's owner, an audit record's reader and a tool's boundary are decided by.
/// Kept separate from [`same_deployment`] because one legitimate pair differs below it: a
/// client of a host process is the same assistant and deliberately not the process holding the
/// database.
fn same_agent(terminal: &RuntimeSettings, host: &RuntimeSettings) {
    assert_eq!(terminal.agent, host.agent, "the agent's identity");
    assert_eq!(terminal.user, host.user, "the person it acts for");
    assert_eq!(terminal.root, host.root, "the confinement root");
    assert_eq!(terminal.model, host.model, "the model every turn goes to");
    assert_eq!(
        terminal.system_prompt, host.system_prompt,
        "what the agent is told before its first turn",
    );
    assert_eq!(
        terminal.config.value("policy"),
        host.config.value("policy"),
        "the policy document",
    );
    assert_eq!(
        terminal.principal(),
        host.principal(),
        "the principal memories and sessions are owned by",
    );
    assert_eq!(
        terminal.operator(),
        host.operator(),
        "the identity the audit trail is read under",
    );
}

#[test]
fn the_shipped_configuration_resolves_to_one_deployment_in_both_frontends() {
    // The regression, at the exact file the documentation tells people to start both processes
    // with. Nothing here is a fixture: if `aik.example.json` ever grows a second place to say
    // who the agent is, this fails.
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = write(directory.path(), &shipped());

    let terminal = terminal(&path).expect("the terminal frontend resolves the shipped file");
    let host = host(&path).expect("the host frontend resolves the shipped file");

    same_deployment(&terminal, &host);
    assert!(
        terminal.has_policy() && host.has_policy(),
        "the shipped configuration carries a policy for both",
    );
    assert!(
        terminal.has_system_prompt() && host.has_system_prompt(),
        "the shipped prompt is how the agent is told its memory exists",
    );
}

#[test]
fn every_deployment_wide_value_reaches_both_frontends_from_the_shared_section() {
    // Distinctive values, so a field that silently fell back to a default is a failure rather
    // than a coincidence: none of these is what the built-in default would produce.
    let directory = tempfile::tempdir().expect("a temporary directory");
    let root = directory.path().join("project");
    std::fs::create_dir(&root).expect("a project directory");

    let path = write(
        directory.path(),
        &json!({
            "agent": {
                "agent": "shared-agent",
                "user": "shared-user",
                "root": root,
                "model": "shared-model",
                "system_prompt": "you have a durable memory",
            },
            "cli": { "socket": "/nonexistent/client.sock" },
            "daemon": { "max_connections": 3 },
        }),
    );

    let host = host(&path).expect("the host frontend resolves it");
    assert_eq!(host.agent.as_str(), "shared-agent");
    assert_eq!(host.user.as_str(), "shared-user");
    assert_eq!(host.root, root.canonicalize().expect("a real directory"));
    assert_eq!(
        host.model.as_ref().map(|model| model.as_str()),
        Some("shared-model")
    );
    assert_eq!(
        host.system_prompt.as_deref(),
        Some("you have a durable memory"),
    );

    // The terminal frontend reads a socket out of its own section here, which makes this run a
    // client: everything above still has to arrive, and the database deliberately does not.
    let terminal = terminal(&path).expect("the terminal frontend resolves it");
    assert_eq!(terminal.agent, host.agent);
    assert_eq!(terminal.user, host.user);
    assert_eq!(terminal.root, host.root);
    assert_eq!(terminal.model, host.model);
    assert_eq!(terminal.system_prompt, host.system_prompt);
    assert_eq!(
        terminal.storage,
        Storage::Ephemeral,
        "a client opens no database however the deployment configures one",
    );
    assert_eq!(
        host.storage,
        Storage::Persistent(PathBuf::from(FAKE_DATA).join("aik").join("aik.redb")),
        "and the host still owns the one the deployment configures",
    );
}

#[test]
fn what_may_be_executed_is_the_deployment_s_and_reaches_both_frontends() {
    // The mode is a per-run decision either frontend makes; *what* may run is not, and a host
    // and a terminal that disagreed about the allowlist would be two different boundaries over
    // one project. The default matters as much: a configuration that says nothing about
    // execution must leave it off in both.
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = write(
        directory.path(),
        &json!({
            "agent": {
                "exec": {
                    "programs": ["git", "rg"],
                    "writable": true,
                    "timeout_ms": 5000,
                },
            },
        }),
    );

    let terminal = terminal(&path).expect("the terminal frontend resolves it");
    let host = host(&path).expect("the host frontend resolves it");

    assert_eq!(terminal.exec_settings.programs, ["git", "rg"]);
    assert!(terminal.exec_settings.writable);
    assert!(
        !terminal.exec_settings.network,
        "a network is granted, never defaulted into",
    );
    assert_eq!(terminal.exec_settings.timeout_ms, Some(5000));
    assert_eq!(terminal.exec_settings, host.exec_settings);

    assert_eq!(
        (terminal.exec, host.exec),
        (ExecSet::Off, ExecSet::Off),
        "naming programs does not by itself let anything run them",
    );
}

#[test]
fn the_shipped_configuration_runs_no_programs_unless_asked() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = write(directory.path(), &shipped());

    let terminal = terminal(&path).expect("the terminal frontend resolves the shipped file");
    assert_eq!(terminal.exec, ExecSet::Off);
}

#[test]
fn a_frontend_specific_setting_stays_in_its_own_section() {
    // The other half of the split: a socket a client connects to and a socket a host binds are
    // not the same setting, and unifying them would make a legitimate deployment unsayable.
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = write(
        directory.path(),
        &json!({
            "cli": { "socket": "/nonexistent/client.sock" },
            "daemon": { "socket": "/nonexistent/host.sock", "max_connections": 3 },
        }),
    );

    let terminal = Settings::resolve_from(
        &aik_cli::args::Options {
            config: Some(path.clone()),
            ..Default::default()
        },
        env(),
    )
    .expect("the terminal frontend resolves it");
    let host = DaemonSettings::resolve_from(
        &aik_daemon::args::Options {
            config: Some(path),
            ..Default::default()
        },
        env(),
    )
    .expect("the host frontend resolves it");

    assert_eq!(
        terminal.socket.as_deref(),
        Some(Path::new("/nonexistent/client.sock")),
    );
    assert_eq!(
        host.endpoint.socket(),
        Path::new("/nonexistent/host.sock"),
        "a host binds where it was told to, not where a client was told to look",
    );
    assert_eq!(host.max_connections, 3);
    // Two different sockets, one assistant. The storage differs by design and only here: a
    // client of a host does not open the host's database.
    same_agent(&terminal.runtime, &host.runtime);
    assert_eq!(terminal.runtime.storage, Storage::Ephemeral);
}

#[test]
fn old_frontend_specific_deployment_keys_fail_loudly() {
    // The migration, made loud. Every one of these used to resolve — to a value only one of
    // the two processes could ever see. Silently ignoring them now would be worse than the
    // drift it replaced: a file that says `"cli": { "agent": "assistant" }` and gets
    // `assistant` from the built-in default looks like it worked, right up until somebody
    // changes it.
    let directory = tempfile::tempdir().expect("a temporary directory");

    for key in ["agent", "user", "root", "model", "system_prompt"] {
        let path = write(directory.path(), &json!({ "cli": { key: "something" } }));
        let error = terminal(&path).expect_err(&format!("`cli.{key}` is no longer a setting"));
        assert!(matches!(error, Error::Config { .. }), "cli.{key}: {error}");

        let path = write(directory.path(), &json!({ "daemon": { key: "something" } }));
        let error = host(&path).expect_err(&format!("`daemon.{key}` is no longer a setting"));
        assert!(
            matches!(error, Error::Config { .. }),
            "daemon.{key}: {error}"
        );
    }
}

#[test]
fn the_root_reported_is_the_root_the_tools_enforce() {
    // The filesystem tools canonicalize the root and check every resolved path against *that*.
    // A banner, a status reply or an audit record naming the raw configured path would name a
    // boundary that is not the one in force, and a reader has no way to tell the difference.
    let directory = tempfile::tempdir().expect("a temporary directory");
    let real = directory.path().join("real");
    std::fs::create_dir(&real).expect("a real directory");
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(&real, &link).expect("a symlink to it");

    let path = write(directory.path(), &json!({ "agent": { "root": link } }));

    let terminal = terminal(&path).expect("resolved");
    let host = host(&path).expect("resolved");
    let enforced = aik_fs::FsReadTool::new(&link)
        .expect("the tool the wiring registers")
        .root()
        .to_path_buf();

    assert_eq!(terminal.root, enforced);
    assert_eq!(host.root, enforced);
    assert_eq!(enforced, real.canonicalize().expect("a real directory"));
}

#[test]
fn a_root_that_does_not_exist_yet_is_carried_through_rather_than_refused() {
    // Canonicalization needs the path to exist. Refusing here would break the two deployments
    // that legitimately never touch it — a run with no filesystem tools, and a client of a host
    // — while the deployment that does register the tools still fails, in the tool, with the
    // message it always had.
    let directory = tempfile::tempdir().expect("a temporary directory");
    let missing = directory.path().join("not-created-yet");
    let path = write(directory.path(), &json!({ "agent": { "root": missing } }));

    let terminal = terminal(&path).expect("resolution does not require the root to exist");
    assert_eq!(terminal.root, missing);
    assert!(
        aik_fs::FsReadTool::new(&missing).is_err(),
        "and registering a tool against it is still refused",
    );
}

#[test]
fn the_defaults_are_the_same_deployment_too() {
    // No configuration file at all: the fallbacks are in one place, so there is no second set
    // of them to disagree with.
    let terminal = Settings::resolve_from(&aik_cli::args::Options::default(), env())
        .expect("resolved")
        .runtime;
    let host = DaemonSettings::resolve_from(&aik_daemon::args::Options::default(), env())
        .expect("resolved")
        .runtime;

    same_deployment(&terminal, &host);
    assert_eq!(terminal.agent.as_str(), aik_runtime::DEFAULT_AGENT);
    assert_eq!(terminal.user.as_str(), aik_runtime::DEFAULT_USER);
    assert_eq!(terminal.tools, ToolSet::ReadOnly);
    assert_eq!(terminal.memory, MemorySet::Remember);
    assert_eq!(terminal.model, None);
}
