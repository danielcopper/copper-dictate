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
use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::final_server::HttpFinalServer;
use crate::microphone::Microphone;
use crate::session::{AudioSource, BoxError, Session};
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
    let session = session(config, Box::new(Microphone));
    eprintln!("copper-dictate: listening at {}", socket.display());
    daemon::serve(&listener, &session);
    Ok(())
}

fn session(config: Config, audio: Box<dyn AudioSource>) -> Arc<Session> {
    Session::new(
        audio,
        Box::new(HttpFinalServer::new(config.final_server)),
        config.max_recording,
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use clap::CommandFactory;

    use super::{Cli, session};
    use crate::config::{Config, FinalServerConfig};
    use crate::session::Outcome;
    use crate::test_support::{FakeHttpServer, FakeMicrophone};

    #[test]
    fn the_daemons_session_stops_at_the_configured_limit_and_sends_to_the_configured_endpoint() {
        let fake = FakeHttpServer::start(vec![(200, r#"{"text":"at the limit"}"#.to_owned())]);
        let (microphone, _events) = FakeMicrophone::new(vec![1; 160]);
        let config = Config {
            final_server: FinalServerConfig {
                endpoint: fake.url.parse().unwrap(),
                model: "whisper-large-v3".to_owned(),
                api_key: None,
            },
            max_recording: Duration::from_millis(300),
        };
        let session = session(config, Box::new(microphone));

        assert_eq!(session.toggle(), Outcome::Started);

        let request = fake.requests.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(request.path, "/v1/audio/transcriptions");
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
