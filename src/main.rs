mod audio;
mod config;
mod daemon;
mod log;
mod platform;
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
use crate::session::{AudioSource, BoxError, Insertion, Session};
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
                log::eprint(error);
                false
            }
        },
        Command::Toggle => match protocol::socket_path(&env) {
            Ok(socket) => {
                daemon::toggle::run(&socket, &mut io::stdout().lock(), &mut io::stderr().lock())
            }
            Err(error) => {
                log::eprint(error);
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
    let session = session(
        config,
        Box::new(Microphone),
        Box::new(platform::NoInsertion),
    );
    log::eprint(format_args!("listening at {}", socket.display()));
    daemon::serve(&listener, &session);
    Ok(())
}

fn session(
    config: Config,
    audio: Box<dyn AudioSource>,
    insertion: Box<dyn Insertion>,
) -> Arc<Session> {
    Session::new(
        audio,
        Box::new(HttpFinalServer::new(config.final_server)),
        insertion,
        config.max_recording,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use clap::CommandFactory;

    use super::{Cli, session};
    use crate::config::Config;
    use crate::session::Outcome;
    use crate::test_support::{
        FakeHttpServer, FakeInsertion, FakeMicrophone, final_server_config, source_files,
    };

    const ROLES: [&str; 4] = ["audio", "daemon", "platform", "servers"];
    const AT_THE_TOP: [&str; 6] = [
        "config.rs",
        "log.rs",
        "main.rs",
        "session.rs",
        "test_support.rs",
        "xdg.rs",
    ];

    #[test]
    fn the_daemons_session_stops_at_the_configured_limit_and_sends_to_the_configured_endpoint() {
        let fake = FakeHttpServer::start(vec![(200, r#"{"text":"at the limit"}"#.to_owned())]);
        let (microphone, _events) = FakeMicrophone::new(vec![1; 160]);
        let config = Config {
            final_server: final_server_config(&fake.url, None),
            max_recording: Duration::from_millis(300),
        };
        let session = session(
            config,
            Box::new(microphone),
            Box::new(FakeInsertion::default()),
        );

        assert_eq!(session.toggle(), Outcome::Started);

        let request = fake.requests.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(request.path, "/v1/audio/transcriptions");
    }

    #[test]
    fn only_the_core_the_shared_modules_and_the_roles_sit_at_the_top() {
        let misplaced: Vec<String> = source_files()
            .into_keys()
            .filter(|name| !name.contains('/'))
            .filter(|name| {
                !AT_THE_TOP.contains(&name.as_str())
                    && !ROLES.iter().any(|role| *name == format!("{role}.rs"))
            })
            .collect();

        assert!(misplaced.is_empty(), "{misplaced:?}");
    }

    #[test]
    fn every_directory_is_a_role() {
        let unknown: BTreeSet<String> = source_files()
            .into_keys()
            .filter_map(|name| Some(name.split_once('/')?.0.to_owned()))
            .filter(|directory| !ROLES.contains(&directory.as_str()))
            .collect();

        assert!(unknown.is_empty(), "{unknown:?}");
    }

    #[test]
    fn a_directorys_module_is_the_file_beside_it() {
        let files = source_files();
        let misplaced: Vec<&String> = files
            .keys()
            .filter(|name| {
                name.rsplit_once('/').is_some_and(|(directory, file)| {
                    file == "mod.rs" || !files.contains_key(&format!("{directory}.rs"))
                })
            })
            .collect();

        assert!(misplaced.is_empty(), "{misplaced:?}");
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
