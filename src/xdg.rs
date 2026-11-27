use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Looks up an environment variable; tests pass their own instead of the process's.
pub(crate) type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

pub(crate) fn app_dir(base: &Path) -> PathBuf {
    base.join("copper-dictate")
}

pub(crate) fn config_home(env: Env<'_>) -> Option<PathBuf> {
    absolute(env("XDG_CONFIG_HOME"))
        .or_else(|| absolute(env("HOME")).map(|home| home.join(".config")))
}

pub(crate) fn state_home(env: Env<'_>) -> Option<PathBuf> {
    absolute(env("XDG_STATE_HOME"))
        .or_else(|| absolute(env("HOME")).map(|home| home.join(".local").join("state")))
}

pub(crate) fn runtime_dir(env: Env<'_>) -> Option<PathBuf> {
    absolute(env("XDG_RUNTIME_DIR"))
}

fn absolute(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}
