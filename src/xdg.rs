use std::ffi::OsString;
use std::fs::DirBuilder;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

/// Looks up an environment variable; tests pass their own instead of the process's.
pub(crate) type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

pub(crate) fn app_dir(base: &Path) -> PathBuf {
    base.join("copper-dictate")
}

/// Creates the directory and any missing parents so that only the user can open them.
pub(crate) fn create_private_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::create_private_dir;
    use crate::test_support::TempDir;

    #[test]
    fn a_private_dir_and_its_missing_parents_open_only_to_the_user() {
        let base = TempDir::new();
        let parent = base.path().join("state");
        let dir = parent.join("copper-dictate");

        create_private_dir(&dir).unwrap();
        create_private_dir(&dir).unwrap();

        let mode = |path: &Path| path.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&parent), 0o700);
    }
}
