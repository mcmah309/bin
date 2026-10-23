mod age;
mod backend;
mod cli;
mod config;
#[cfg(all(unix, not(target_os = "macos")))]
mod freedesktop;
mod history;
mod operations;

use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    match operations::run(cli::Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("bin: {error:#?}");
            ExitCode::FAILURE
        }
    }
}
