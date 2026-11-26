use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;
use ureq::http::Uri;

use crate::xdg::{self, Env};

const API_KEY_VARIABLE: &str = "COPPER_DICTATE_FINAL_API_KEY";
const DEFAULT_MAX_RECORDING_SECONDS: u64 = 15 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Config {
    pub(crate) final_server: FinalServerConfig,
    pub(crate) max_recording: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinalServerConfig {
    pub(crate) endpoint: Uri,
    pub(crate) model: String,
    pub(crate) api_key: Option<ApiKey>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ApiKey(String);

impl ApiKey {
    pub(crate) fn parse(raw: &[u8]) -> Result<Self, KeyError> {
        let key = raw.trim_ascii();
        if key.is_empty() {
            return Err(KeyError::Empty);
        }
        if !key.iter().all(u8::is_ascii_graphic) {
            return Err(KeyError::Invalid);
        }
        String::from_utf8(key.to_vec())
            .map(Self)
            .map_err(|_| KeyError::Invalid)
    }

    pub(crate) fn bearer(&self) -> String {
        format!("Bearer {}", self.0)
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey(..)")
    }
}

#[derive(Debug, Error)]
pub(crate) enum KeyError {
    #[error("cannot be read: {0}")]
    Read(io::Error),
    #[error("is empty")]
    Empty,
    #[error(
        "holds no valid key: only visible ASCII characters are allowed, without spaces or line breaks"
    )]
    Invalid,
}

#[derive(Debug, Error)]
pub(crate) enum ConfigError {
    #[error(
        "neither XDG_CONFIG_HOME nor HOME is set to an absolute path, so the configuration file cannot be found"
    )]
    NoConfigDir,
    #[error("the configuration file {0} does not exist")]
    Missing(PathBuf),
    #[error("cannot read the configuration file {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("the configuration file {path} is invalid: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("the API key file {path} (api_key_file in {config}) {problem}")]
    KeyFile {
        path: PathBuf,
        config: PathBuf,
        problem: KeyError,
    },
    #[error("the environment variable {API_KEY_VARIABLE} {0}")]
    KeyVariable(KeyError),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(rename = "final")]
    final_server: FinalSection,
    #[serde(default)]
    recording: RecordingSection,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinalSection {
    endpoint: String,
    model: String,
    api_key_file: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RecordingSection {
    max_seconds: u64,
}

impl Default for RecordingSection {
    fn default() -> Self {
        Self {
            max_seconds: DEFAULT_MAX_RECORDING_SECONDS,
        }
    }
}

pub(crate) fn path(env: Env<'_>) -> Result<PathBuf, ConfigError> {
    let config_home = xdg::config_home(env).ok_or(ConfigError::NoConfigDir)?;
    Ok(config_home.join("copper-dictate").join("config.toml"))
}

pub(crate) fn load(path: &Path, env: Env<'_>) -> Result<Config, ConfigError> {
    let text = fs::read_to_string(path).map_err(|source| match source.kind() {
        io::ErrorKind::NotFound => ConfigError::Missing(path.to_owned()),
        _ => ConfigError::Read {
            path: path.to_owned(),
            source,
        },
    })?;
    let invalid = |message: String| ConfigError::Invalid {
        path: path.to_owned(),
        message,
    };
    let file: File = toml::from_str(&text).map_err(|error| invalid(error.to_string()))?;
    if file.recording.max_seconds == 0 {
        return Err(invalid(
            "max_seconds in [recording] must be at least 1".to_owned(),
        ));
    }

    let endpoint = parse_endpoint(&file.final_server.endpoint).ok_or_else(|| {
        invalid(format!(
            "endpoint in [final] must be an http or https URL with a host, not {:?}",
            file.final_server.endpoint
        ))
    })?;

    let api_key = match file.final_server.api_key_file {
        Some(key_file) => {
            let key_file = path.parent().unwrap_or(Path::new("")).join(key_file);
            let key = fs::read(&key_file)
                .map_err(KeyError::Read)
                .and_then(|raw| ApiKey::parse(&raw));
            Some(key.map_err(|problem| ConfigError::KeyFile {
                path: key_file,
                config: path.to_owned(),
                problem,
            })?)
        }
        None => match env(API_KEY_VARIABLE) {
            Some(value) if !value.is_empty() => {
                Some(ApiKey::parse(value.as_encoded_bytes()).map_err(ConfigError::KeyVariable)?)
            }
            _ => None,
        },
    };

    Ok(Config {
        final_server: FinalServerConfig {
            endpoint,
            model: file.final_server.model,
            api_key,
        },
        max_recording: Duration::from_secs(file.recording.max_seconds),
    })
}

fn parse_endpoint(text: &str) -> Option<Uri> {
    let uri: Uri = text.parse().ok()?;
    let web = matches!(uri.scheme_str(), Some("http" | "https"));
    let has_host = uri.host().is_some_and(|host| !host.is_empty());
    (web && has_host).then_some(uri)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::ffi::OsStringExt;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{ApiKey, Config, ConfigError, FinalServerConfig, KeyError, load, path};
    use crate::test_support::TempDir;

    const MINIMAL: &str = r#"
        [final]
        endpoint = "http://127.0.0.1:8000/v1/audio/transcriptions"
        model = "whisper-large-v3"
    "#;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, OsString)> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(candidate, _)| candidate == name)
                .map(|(_, value)| value.clone())
        }
    }

    fn key(text: &str) -> ApiKey {
        ApiKey::parse(text.as_bytes()).unwrap()
    }

    fn with_key_file(dir: &TempDir, key_file: &str, contents: &[u8]) -> PathBuf {
        fs::write(dir.path().join(key_file), contents).unwrap();
        write_config(dir, &format!("{MINIMAL}api_key_file = {key_file:?}\n"))
    }

    fn write_config(dir: &TempDir, text: &str) -> PathBuf {
        let path = dir.path().join("config.toml");
        fs::write(&path, text).unwrap();
        path
    }

    fn invalid_message(result: Result<Config, ConfigError>, path: &Path) -> String {
        match result {
            Err(ConfigError::Invalid {
                path: reported,
                message,
            }) => {
                assert_eq!(reported, path);
                message
            }
            other => panic!("expected an invalid configuration, got {other:?}"),
        }
    }

    #[test]
    fn the_file_lies_in_xdg_config_home() {
        let found = path(&env(&[
            ("XDG_CONFIG_HOME", "/xdg"),
            ("HOME", "/home/someone"),
        ]));

        assert_eq!(found.unwrap(), Path::new("/xdg/copper-dictate/config.toml"));
    }

    #[test]
    fn the_file_lies_in_dot_config_without_xdg_config_home() {
        let found = path(&env(&[("HOME", "/home/someone")]));

        assert_eq!(
            found.unwrap(),
            Path::new("/home/someone/.config/copper-dictate/config.toml")
        );
    }

    #[test]
    fn a_relative_xdg_config_home_is_ignored() {
        let found = path(&env(&[
            ("XDG_CONFIG_HOME", "relative"),
            ("HOME", "/home/someone"),
        ]));

        assert_eq!(
            found.unwrap(),
            Path::new("/home/someone/.config/copper-dictate/config.toml")
        );
    }

    #[test]
    fn without_any_config_directory_there_is_no_path() {
        assert!(matches!(path(&env(&[])), Err(ConfigError::NoConfigDir)));
    }

    #[test]
    fn a_minimal_file_gives_the_endpoint_the_model_and_a_fifteen_minute_limit() {
        let dir = TempDir::new();
        let config_path = write_config(&dir, MINIMAL);

        let config = load(&config_path, &env(&[])).unwrap();

        assert_eq!(
            config,
            Config {
                final_server: FinalServerConfig {
                    endpoint: "http://127.0.0.1:8000/v1/audio/transcriptions"
                        .parse()
                        .unwrap(),
                    model: "whisper-large-v3".to_owned(),
                    api_key: None,
                },
                max_recording: Duration::from_mins(15),
            }
        );
    }

    #[test]
    fn the_recording_limit_is_configurable() {
        let dir = TempDir::new();
        let config_path =
            write_config(&dir, &format!("{MINIMAL}\n[recording]\nmax_seconds = 60\n"));

        let config = load(&config_path, &env(&[])).unwrap();

        assert_eq!(config.max_recording, Duration::from_secs(60));
    }

    #[test]
    fn a_zero_recording_limit_is_refused() {
        let dir = TempDir::new();
        let config_path = write_config(&dir, &format!("{MINIMAL}\n[recording]\nmax_seconds = 0\n"));

        let message = invalid_message(load(&config_path, &env(&[])), &config_path);

        assert!(message.contains("max_seconds"), "{message}");
    }

    #[test]
    fn the_key_comes_from_the_environment_without_a_key_file() {
        let dir = TempDir::new();
        let config_path = write_config(&dir, MINIMAL);

        let config = load(
            &config_path,
            &env(&[("COPPER_DICTATE_FINAL_API_KEY", "env-key")]),
        )
        .unwrap();

        assert_eq!(config.final_server.api_key, Some(key("env-key")));
    }

    #[test]
    fn the_key_file_wins_over_the_environment() {
        let dir = TempDir::new();
        let key_path = dir.path().join("final.key");
        fs::write(&key_path, "file-key\n").unwrap();
        let config_path = write_config(
            &dir,
            &format!("{MINIMAL}api_key_file = {:?}\n", key_path.to_str().unwrap()),
        );

        let config = load(
            &config_path,
            &env(&[("COPPER_DICTATE_FINAL_API_KEY", "env-key")]),
        )
        .unwrap();

        assert_eq!(config.final_server.api_key, Some(key("file-key")));
    }

    #[test]
    fn a_missing_key_file_names_its_path() {
        let dir = TempDir::new();
        let key_path = dir.path().join("absent.key");
        let config_path = write_config(
            &dir,
            &format!("{MINIMAL}api_key_file = {:?}\n", key_path.to_str().unwrap()),
        );

        let error = load(&config_path, &env(&[])).unwrap_err();

        assert!(matches!(&error, ConfigError::KeyFile { path, .. } if *path == key_path));
        assert!(
            error.to_string().contains(key_path.to_str().unwrap()),
            "{error}"
        );
    }

    #[test]
    fn an_empty_key_variable_means_no_key() {
        let dir = TempDir::new();
        let config_path = write_config(&dir, MINIMAL);

        let config = load(&config_path, &env(&[("COPPER_DICTATE_FINAL_API_KEY", "")])).unwrap();

        assert_eq!(config.final_server.api_key, None);
    }

    #[test]
    fn a_key_variable_of_only_whitespace_is_refused_as_empty() {
        let dir = TempDir::new();
        let config_path = write_config(&dir, MINIMAL);

        let error = load(
            &config_path,
            &env(&[("COPPER_DICTATE_FINAL_API_KEY", " \n")]),
        )
        .unwrap_err();

        assert!(
            matches!(error, ConfigError::KeyVariable(KeyError::Empty)),
            "{error:?}"
        );
    }

    #[test]
    fn a_key_variable_that_is_not_utf8_stops_the_start_and_names_the_variable() {
        let dir = TempDir::new();
        let config_path = write_config(&dir, MINIMAL);
        let env = |name: &str| {
            (name == "COPPER_DICTATE_FINAL_API_KEY")
                .then(|| OsString::from_vec(vec![b's', b'k', 0xff]))
        };

        let error = load(&config_path, &env).unwrap_err();

        assert!(
            matches!(error, ConfigError::KeyVariable(KeyError::Invalid)),
            "{error:?}"
        );
        assert!(
            error.to_string().starts_with(
                "the environment variable COPPER_DICTATE_FINAL_API_KEY holds no valid key"
            ),
            "{error}"
        );
    }

    #[test]
    fn an_empty_key_file_stops_the_start_and_names_the_file() {
        let dir = TempDir::new();
        let config_path = with_key_file(&dir, "final.key", b" \n");

        let error = load(&config_path, &env(&[])).unwrap_err();

        let key_path = dir.path().join("final.key");
        assert!(
            matches!(&error, ConfigError::KeyFile { path, problem: KeyError::Empty, .. } if *path == key_path),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "the API key file {} (api_key_file in {}) is empty",
                key_path.display(),
                config_path.display()
            )
        );
    }

    #[test]
    fn a_key_file_with_more_than_one_line_is_refused() {
        let dir = TempDir::new();
        let config_path = with_key_file(&dir, "final.key", b"sk-one\nsk-two\n");

        let error = load(&config_path, &env(&[])).unwrap_err();

        assert!(
            matches!(
                error,
                ConfigError::KeyFile {
                    problem: KeyError::Invalid,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_relative_key_file_is_read_from_the_configuration_files_directory() {
        let dir = TempDir::new();
        let config_path = with_key_file(&dir, "final.key", b"sk-beside\n");

        let config = load(&config_path, &env(&[])).unwrap();

        assert_eq!(config.final_server.api_key, Some(key("sk-beside")));
    }

    #[test]
    fn the_key_stays_out_of_debug_output() {
        let dir = TempDir::new();
        let config_path = write_config(&dir, MINIMAL);

        let config = load(
            &config_path,
            &env(&[("COPPER_DICTATE_FINAL_API_KEY", "sk-secret")]),
        )
        .unwrap();

        let debug = format!("{config:?}");
        assert!(!debug.contains("sk-secret"), "{debug}");
    }

    #[test]
    fn the_endpoint_must_be_an_http_or_https_url_with_a_host() {
        let dir = TempDir::new();
        for endpoint in [
            "whisper.lan:8000/v1/audio/transcriptions",
            "ftp://whisper.lan/v1/audio/transcriptions",
            "http:///v1/audio/transcriptions",
            "http://:8000/v1/audio/transcriptions",
            "not a url",
        ] {
            let config_path = write_config(
                &dir,
                &format!("[final]\nendpoint = {endpoint:?}\nmodel = \"m\"\n"),
            );

            let message = invalid_message(load(&config_path, &env(&[])), &config_path);

            assert!(
                message.starts_with("endpoint in [final] must be an http or https URL"),
                "{endpoint}: {message}"
            );
        }
    }

    #[test]
    fn an_https_endpoint_is_accepted() {
        let dir = TempDir::new();
        let config_path = write_config(
            &dir,
            "[final]\nendpoint = \"https://whisper.lan/v1/audio/transcriptions\"\nmodel = \"m\"\n",
        );

        let config = load(&config_path, &env(&[])).unwrap();

        assert_eq!(
            config.final_server.endpoint,
            "https://whisper.lan/v1/audio/transcriptions"
        );
    }

    #[test]
    fn the_main_configuration_cannot_hold_a_key() {
        let dir = TempDir::new();
        let config_path = write_config(&dir, &format!("{MINIMAL}api_key = \"secret\"\n"));

        let message = invalid_message(load(&config_path, &env(&[])), &config_path);

        assert!(message.contains("unknown field `api_key`"), "{message}");
    }

    #[test]
    fn a_missing_file_names_its_path() {
        let dir = TempDir::new();
        let config_path = dir.path().join("config.toml");

        let error = load(&config_path, &env(&[])).unwrap_err();

        assert!(matches!(&error, ConfigError::Missing(path) if *path == config_path));
        assert!(
            error.to_string().contains(config_path.to_str().unwrap()),
            "{error}"
        );
    }

    #[test]
    fn a_missing_required_field_is_named_with_the_path() {
        let dir = TempDir::new();
        let config_path = write_config(
            &dir,
            "[final]\nendpoint = \"http://127.0.0.1:8000/v1/audio/transcriptions\"\n",
        );

        let error = load(&config_path, &env(&[])).unwrap_err();

        assert!(
            error.to_string().contains(config_path.to_str().unwrap()),
            "{error}"
        );
        assert!(
            error.to_string().contains("missing field `model`"),
            "{error}"
        );
    }
}
