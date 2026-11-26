mod config;
mod daemon;
mod final_server;
mod microphone;
mod protocol;
mod resample;
mod session;
#[cfg(test)]
mod test_support;
mod toggle;
mod wav;
mod xdg;

use std::ffi::OsString;
use std::io;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::final_server::HttpFinalServer;
use crate::microphone::Microphone;
use crate::session::{BoxError, Session};
use crate::xdg::Env;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon that records from the default microphone and talks to the final server
    Daemon,
    /// Start a recording, or stop the running one and print its final text
    Toggle,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let env = |name: &str| -> Option<OsString> { std::env::var_os(name) };
    let succeeded = match cli.command {
        Command::Daemon => match run_daemon(&env) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("copper-dictate: {error}");
                false
            }
        },
        Command::Toggle => match daemon::socket_path(&env) {
            Ok(socket) => toggle::run(&socket, &mut io::stdout().lock(), &mut io::stderr().lock()),
            Err(error) => {
                eprintln!("copper-dictate: {error}");
                false
            }
        },
    };
    if succeeded {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn run_daemon(env: Env<'_>) -> Result<(), BoxError> {
    let config = config::load(&config::path(env)?, env)?;
    let socket = daemon::socket_path(env)?;
    let listener = daemon::bind(&socket)?;
    let session = Session::new(
        Box::new(Microphone),
        Box::new(HttpFinalServer::new(config.final_server)),
        config.max_recording,
    );
    eprintln!("copper-dictate: listening at {}", socket.display());
    daemon::serve(&listener, &session)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::Cli;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
