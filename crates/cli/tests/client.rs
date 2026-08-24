//! `aik --socket`: the terminal against a running host.
//!
//! One test binary that starts a real `aikd` in-process — the shipped assembly, the shipped
//! listener, the shipped protocol — and then runs the shipped `aik` against it. What is being
//! asserted is the seam: that the terminal really becomes a client, that it assembles nothing
//! of its own, and that everything it refuses to do locally it also refuses to do here.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aik_api::model::ModelId;
use aik_cli::args::{Invocation, Options, parse};
use aik_cli::settings::Settings;
use aik_core::ComponentId;
use aik_daemon::settings::DaemonSettings;
use support::{Reply, STUB_MODEL, ScriptedModel, StubModelComponent};
use tokio_util::sync::CancellationToken;

/// A host serving on a socket under `root`, with a scripted model.
struct Host {
    socket: PathBuf,
    shutdown: CancellationToken,
    serving: tokio::task::JoinHandle<aik_core::Result<()>>,
    _data: tempfile::TempDir,
}

impl Host {
    async fn start(root: &Path, replies: Vec<Reply>) -> Self {
        let data = tempfile::tempdir().expect("a temporary data directory");
        let socket = root.join("run").join("aikd.sock");
        let policy = root.join(".policy.json");
        std::fs::write(
            &policy,
            serde_json::json!({
                "rules": [
                    { "action": "*", "resource": "*", "effect": { "decision": "allow" } },
                    { "action": "*", "effect": { "decision": "allow" } }
                ]
            })
            .to_string(),
        )
        .expect("a policy file");

        let options = aik_daemon::args::Options {
            root: Some(root.to_path_buf()),
            socket: Some(socket.clone()),
            database: Some(data.path().join("aik.redb")),
            policy: Some(policy),
            model: Some("scripted".to_owned()),
            ..aik_daemon::args::Options::default()
        };
        let mut settings = DaemonSettings::resolve_from(&options, Vec::<(String, String)>::new())
            .expect("resolved settings");
        settings.runtime.model_component = ComponentId::new(STUB_MODEL);

        let model = Arc::new(ScriptedModel::new(replies));
        let (builder, broker) =
            aik_runtime::wiring::builder(&settings.runtime, ModelId::new("scripted"))
                .expect("the shipped wiring");
        let kernel = builder
            .component(StubModelComponent::new(model))
            .build()
            .expect("a kernel");

        let shutdown = CancellationToken::new();
        let serving = tokio::spawn({
            let settings = settings.clone();
            let shutdown = shutdown.clone();
            async move {
                aik_daemon::serve_assembled(
                    &settings,
                    ModelId::new("scripted"),
                    aik_runtime::wiring::Assembled { kernel, broker },
                    shutdown,
                )
                .await
            }
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !aik_ipc::is_listening(&socket) {
            assert!(
                std::time::Instant::now() < deadline,
                "the host never listened"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        Self {
            socket,
            shutdown,
            serving,
            _data: data,
        }
    }

    async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(10), self.serving)
            .await
            .expect("the host stops")
            .expect("the serving task did not panic")
            .expect("it stopped cleanly");
    }
}

fn root() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

// ---------------------------------------------------------------------------
// what `--socket` makes this command
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_one_shot_run_through_a_host_answers_and_leaves_no_database_behind() {
    let root = root();
    let host = Host::start(root.path(), vec![Reply::answer("through the host")]).await;

    let options = Options {
        socket: Some(host.socket.clone()),
        prompt: Some("hello".to_owned()),
        ..Options::default()
    };

    aik_cli::run(&options).await.expect("the turn is answered");

    // Nothing local: the terminal opened no database of its own, and there is none to find.
    assert!(
        std::fs::read_dir(root.path())
            .expect("listed")
            .filter_map(std::result::Result::ok)
            .all(|entry| entry.file_name() != "aik.redb"),
        "a client assembles nothing and must leave nothing",
    );

    host.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_run_resolves_no_database_even_where_there_is_nowhere_to_put_one() {
    // The machine a thin client is most likely to be. A run that resolved a database it was
    // never going to open would refuse to start here.
    let settings = Settings::resolve_from(
        &Options {
            socket: Some(PathBuf::from("/run/user/1000/aik/aikd.sock")),
            ..Options::default()
        },
        Vec::<(String, String)>::new(),
    )
    .expect("a client needs nowhere to put a database");

    assert_eq!(settings.database(), None);
    assert!(settings.socket.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_environment_can_make_a_terminal_a_client_and_absence_makes_it_a_kernel() {
    let with = Settings::resolve_from(
        &Options::default(),
        vec![
            (aik_ipc::SOCKET_ENV.to_owned(), "/tmp/aikd.sock".to_owned()),
            ("XDG_DATA_HOME".to_owned(), "/nonexistent".to_owned()),
        ],
    )
    .expect("resolved");
    assert_eq!(with.socket.as_deref(), Some(Path::new("/tmp/aikd.sock")));

    // Absent, and it is a kernel of its own again — never a client of whatever happens to be
    // listening at the host's default location.
    let without = Settings::resolve_from(
        &Options::default(),
        vec![
            ("XDG_DATA_HOME".to_owned(), "/nonexistent".to_owned()),
            ("XDG_RUNTIME_DIR".to_owned(), "/nonexistent/run".to_owned()),
        ],
    )
    .expect("resolved");
    assert_eq!(without.socket, None);
    assert!(without.database().is_some());
}

// ---------------------------------------------------------------------------
// the options a client is not allowed to pretend to have
// ---------------------------------------------------------------------------

#[test]
fn every_option_that_describes_an_assembly_is_refused_alongside_a_socket() {
    // Accepted and ignored would be the dangerous outcome: `--no-tools` that did nothing
    // reads as "this connection may reach no tool", which is exactly backwards.
    for extra in [
        vec!["--write"],
        vec!["--no-tools"],
        vec!["--memory", "full"],
        vec!["--db", "/tmp/other.redb"],
        vec!["--ephemeral"],
        vec!["--policy", "/tmp/policy.json"],
        vec!["--root", "/tmp"],
        vec!["--model", "other"],
        vec!["--agent", "someone"],
        vec!["--user", "someone"],
    ] {
        let mut arguments = vec!["--socket".to_owned(), "/tmp/aikd.sock".to_owned()];
        arguments.extend(extra.iter().map(|argument| (*argument).to_owned()));

        let error = parse(arguments.clone()).expect_err(&format!(
            "{extra:?} must not be accepted alongside --socket"
        ));
        assert!(error.to_string().contains("contradict"), "{error}");
    }
}

#[test]
fn a_socket_alone_is_a_client_and_a_session_may_still_be_named() {
    let invocation = parse(vec![
        "--socket".to_owned(),
        "/tmp/aikd.sock".to_owned(),
        "--verbose".to_owned(),
    ])
    .expect("parsed");
    let Invocation::Run(options) = invocation else {
        panic!("expected a run");
    };
    assert_eq!(options.socket.as_deref(), Some(Path::new("/tmp/aikd.sock")));
}

#[test]
fn an_audit_review_through_a_host_refuses_to_name_a_database_or_a_reader() {
    // Both would be accepted and ignored otherwise, and both would let somebody believe they
    // had reviewed a different trail, or reviewed it as somebody else.
    for extra in [vec!["--db", "/tmp/other.redb"], vec!["--user", "someone"]] {
        let mut arguments = vec![
            "audit".to_owned(),
            "--socket".to_owned(),
            "/tmp/aikd.sock".to_owned(),
        ];
        arguments.extend(extra.iter().map(|argument| (*argument).to_owned()));

        let error = parse(arguments).expect_err(&format!(
            "{extra:?} must not be accepted alongside --socket"
        ));
        assert!(error.to_string().contains("contradict"), "{error}");
    }
}

// ---------------------------------------------------------------------------
// failing to reach a host
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_socket_that_is_not_there_is_reported_rather_than_assembled_around() {
    // Never a silent fallback to assembling a kernel: a person who asked to talk to the host
    // has not agreed to start a second one, and starting one would take the database.
    let root = root();
    let options = Options {
        socket: Some(root.path().join("run").join("aikd.sock")),
        prompt: Some("hello".to_owned()),
        ..Options::default()
    };

    let error = aik_cli::run(&options)
        .await
        .expect_err("there is no host to talk to");
    assert!(!error.to_string().is_empty(), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_socket_this_account_does_not_own_is_refused_before_a_token_is_read() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = root();
    let host = Host::start(root.path(), vec![Reply::answer("never reached")]).await;

    std::fs::set_permissions(&host.socket, std::fs::Permissions::from_mode(0o666))
        .expect("loosened");

    let error = aik_cli::run(&Options {
        socket: Some(host.socket.clone()),
        prompt: Some("hello".to_owned()),
        ..Options::default()
    })
    .await
    .expect_err("a socket anyone can reach must not be handed a credential");
    assert_eq!(error.kind(), aik_core::ErrorKind::Permission, "{error}");

    std::fs::set_permissions(&host.socket, std::fs::Permissions::from_mode(0o600))
        .expect("restored");
    host.stop().await;
}
