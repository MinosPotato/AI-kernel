//! The `aikd` binary.
//!
//! Everything is in the library, so the host can be started, driven and stopped from a test
//! without a process. This is the process.

fn main() -> std::process::ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!(
                "{}: starting the async runtime: {error}",
                aik_daemon::args::PROGRAM
            );
            return std::process::ExitCode::from(1);
        }
    };
    let code = runtime.block_on(aik_daemon::main(std::env::args().skip(1)));
    std::process::ExitCode::from(u8::try_from(code).unwrap_or(1))
}
