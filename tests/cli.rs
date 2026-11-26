use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A directory standing in for `HOME`, `XDG_CONFIG_HOME` and `XDG_RUNTIME_DIR`, removed on drop.
struct Sandbox(PathBuf);

impl Sandbox {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("cd-cli-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("config")).unwrap();
        fs::create_dir_all(path.join("runtime")).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_copper-dictate"));
        command
            .args(args)
            .env_clear()
            .env("HOME", self.path())
            .env("XDG_CONFIG_HOME", self.path().join("config"))
            .env("XDG_RUNTIME_DIR", self.path().join("runtime"));
        // cargo-llvm-cov collects the binary's coverage through this variable.
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        command
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn version_flag_prints_the_package_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_copper-dictate"))
        .arg("--version")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("copper-dictate {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn toggle_without_a_running_daemon_fails_and_says_so() {
    let sandbox = Sandbox::new("toggle");

    let output = sandbox.run(&["toggle"]);

    assert!(!output.status.success());
    assert_eq!(output.stdout, b"");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("the daemon is not running"), "{stderr}");
}

#[test]
fn toggle_without_a_runtime_directory_fails_and_says_why() {
    let sandbox = Sandbox::new("no-runtime");

    let output = sandbox
        .command(&["toggle"])
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "copper-dictate: XDG_RUNTIME_DIR is not set to an absolute path, so the daemon's socket has no place\n"
    );
}

#[test]
fn the_daemon_does_not_start_without_a_configuration_and_names_its_path() {
    let sandbox = Sandbox::new("daemon");

    let output = sandbox.run(&["daemon"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    let expected = sandbox.path().join("config/copper-dictate/config.toml");
    assert!(stderr.contains(expected.to_str().unwrap()), "{stderr}");
    assert!(
        !sandbox.path().join("runtime/copper-dictate").exists(),
        "no socket was opened"
    );
}

#[test]
fn the_daemon_does_not_start_with_a_missing_field_and_names_it() {
    let sandbox = Sandbox::new("field");
    let config = sandbox.path().join("config/copper-dictate/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "[final]\nmodel = \"whisper-large-v3\"\n").unwrap();

    let output = sandbox.run(&["daemon"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(config.to_str().unwrap()), "{stderr}");
    assert!(stderr.contains("missing field `endpoint`"), "{stderr}");
}
