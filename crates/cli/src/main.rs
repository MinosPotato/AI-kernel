//! The `aik` binary.
//!
//! Everything is in the library so the same code paths can be started, driven and asserted
//! on from tests; this is only the process boundary — arguments in, exit code out.

fn main() {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!(
                "{}: could not start a runtime: {error}",
                aik_cli::args::PROGRAM
            );
            std::process::exit(1);
        }
    };

    let code = runtime.block_on(aik_cli::main(std::env::args().skip(1)));
    std::process::exit(code);
}
