use clap::Parser;

#[macro_use]
mod macros;

mod audio;
mod commands;
mod config;
mod errors;
mod grammar;
mod hotkey;
mod matcher;
mod output;
mod recognition;
mod telemetry;

pub use commands::Command;
pub use human_errors::Error;

/// Speak a phrase, press the keys: a Linux-native voice macro tool.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let session = telemetry::setup();

    match commands::dispatch(args).await {
        Ok(code) => {
            session.shutdown();
            std::process::exit(code);
        }
        Err(e) => {
            session.record_error(&e);
            // The failure itself goes straight to stderr rather than through
            // tracing: the user must see why their command failed even when
            // telemetry is disabled or misconfigured.
            eprintln!("{}", human_errors::pretty(&e));
            session.shutdown();
            std::process::exit(1);
        }
    }
}
