use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

pub(crate) const API_KEY_VARIABLE: &str = "COPPER_DICTATE_FINAL_API_KEY";
const DEFAULT_MAX_RECORDING_SECONDS: u64 = 15 * 60;

/// Looks up an environment variable; tests pass their own instead of the process's.
pub(crate) type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Config {
    pub(crate) final_server: FinalServerConfig,
    pub(crate) max_recording: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinalServerConfig {
    pub(crate) endpoint: String,
    pub(crate) model: String,
    pub(crate) api_key: Option<String>,
}

#[derive(Debug, Error)]
pub(crate) enum ConfigError {
    #[error("neither XDG_CONFIG_HOME nor HOME is set, so the configuration file cannot be found")]
    NoConfigDir,
    #[error("the configuration file {0} does not exist")]
    Missing(PathBuf),
    #[error("cannot read the configuration file {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("the configuration file {path} is invalid: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("cannot read the API key file {path} (api_key_file in {config}): {source}")]
    KeyFile {
        path: PathBuf,
        config: PathBuf,
        source: io::Error,
    },
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
#[serde(deny_unknown_fields)]
struct RecordingSection {
    #[serde(default = "default_max_seconds")]
    max_seconds: u64,
}

impl Default for RecordingSection {
    fn default() -> Self {
        Self {
            max_seconds: default_max_seconds(),
        }
    }
}

fn default_max_seconds() -> u64 {
    DEFAULT_MAX_RECORDING_SECONDS
}

pub(crate) fn path(env: Env<'_>) -> Result<PathBuf, ConfigError> {
    let config_home = absolute(env("XDG_CONFIG_HOME"))
        .or_else(|| absolute(env("HOME")).map(|home| home.join(".config")))
        .ok_or(ConfigError::NoConfigDir)?;
    Ok(config_home.join("copper-dictate").join("config.toml"))
}

fn absolute(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
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

    let api_key = match &file.final_server.api_key_file {
        Some(key_file) => Some(read_key(key_file).map_err(|source| ConfigError::KeyFile {
            path: key_file.clone(),
            config: path.to_owned(),
            source,
        })?),
        None => env(API_KEY_VARIABLE)
            .and_then(|key| key.into_string().ok())
            .filter(|key| !key.is_empty()),
    };

    Ok(Config {
        final_server: FinalServerConfig {
            endpoint: file.final_server.endpoint,
            model: file.final_server.model,
            api_key,
        },
        max_recording: Duration::from_secs(file.recording.max_seconds),
    })
}

fn read_key(path: &Path) -> io::Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{Config, ConfigError, FinalServerConfig, load, path};
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
                    endpoint: "http://127.0.0.1:8000/v1/audio/transcriptions".to_owned(),
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

        assert_eq!(config.final_server.api_key.as_deref(), Some("env-key"));
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

        assert_eq!(config.final_server.api_key.as_deref(), Some("file-key"));
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
