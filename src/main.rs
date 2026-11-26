mod audio;
mod config;
mod daemon;
mod servers;
mod session;
#[cfg(test)]
mod test_support;
mod xdg;

use std::ffi::OsString;
use std::io;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::audio::microphone::Microphone;
use crate::config::Config;
use crate::daemon::protocol;
use crate::servers::final_server::HttpFinalServer;
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
        Command::Toggle => match protocol::socket_path(&env) {
            Ok(socket) => {
                daemon::toggle::run(&socket, &mut io::stdout().lock(), &mut io::stderr().lock())
            }
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
    let socket = protocol::socket_path(env)?;
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
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use clap::CommandFactory;

    use super::{Cli, session};
    use crate::config::Config;
    use crate::session::Outcome;
    use crate::test_support::{FakeHttpServer, FakeMicrophone, final_server_config};

    #[test]
    fn the_daemons_session_stops_at_the_configured_limit_and_sends_to_the_configured_endpoint() {
        let fake = FakeHttpServer::start(vec![(200, r#"{"text":"at the limit"}"#.to_owned())]);
        let (microphone, _events) = FakeMicrophone::new(vec![1; 160]);
        let config = Config {
            final_server: final_server_config(&fake.url, None),
            max_recording: Duration::from_millis(300),
        };
        let session = session(config, Box::new(microphone));

        assert_eq!(session.toggle(), Outcome::Started);

        let request = fake.requests.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(request.path, "/v1/audio/transcriptions");
    }

    fn rust_files(dir: &Path, base: &Path, files: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, base, files);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path.strip_prefix(base).unwrap().display().to_string());
            }
        }
    }

    #[test]
    fn every_module_sits_in_the_directory_of_its_role() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&src, &src, &mut files);
        files.sort();

        assert_eq!(
            files,
            [
                "audio.rs",
                "audio/microphone.rs",
                "audio/resample.rs",
                "config.rs",
                "daemon.rs",
                "daemon/protocol.rs",
                "daemon/toggle.rs",
                "main.rs",
                "servers.rs",
                "servers/final_server.rs",
                "servers/wav.rs",
                "session.rs",
                "test_support.rs",
                "xdg.rs",
            ]
        );
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
