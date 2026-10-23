mod age;
mod backend;
mod cli;
mod config;
mod history;
mod operations;

use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    let _ = log::set_logger(&TRASH_LOGGER);
    log::set_max_level(log::LevelFilter::Warn);
    match operations::run(cli::Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("bin: {error:#?}");
            ExitCode::FAILURE
        }
    }
}

static TRASH_LOGGER: TrashLogger = TrashLogger;

struct TrashLogger;

impl log::Log for TrashLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn && metadata.target().starts_with("trash::")
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("bin: {}", record.args());
        }
    }

    fn flush(&self) {}
}
