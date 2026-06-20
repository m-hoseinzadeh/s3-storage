use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;
use s3_storage::Config;
use tracing_subscriber::EnvFilter;

fn setup_tracing() {
    let enable_color = std::io::stdout().is_terminal();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_ansi(enable_color)
        .init();
}

fn main() -> ExitCode {
    let config = Config::parse();
    setup_tracing();

    if config.access_key.is_some() != config.secret_key.is_some() {
        eprintln!("error: --access-key and --secret-key must be provided together");
        return ExitCode::FAILURE;
    }

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("error: failed to start tokio runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(s3_storage::run(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
