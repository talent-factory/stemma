//! The `stemma` command line. It is a thin layer over `stemma-core`: it reads the arguments,
//! builds the actor, calls the core, and prints the result (ADR-002, ADR-009).

mod cli;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::main()
}
