use std::{fs, process::Command};

fn rsplug() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rsplug"))
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create temp dir");
    fs::write(dir.path().join("a.toml"), "").expect("write a.toml");
    fs::write(dir.path().join("b.toml"), "").expect("write b.toml");
    dir
}

fn isolated_command(dir: &tempfile::TempDir) -> Command {
    let mut command = rsplug();
    command
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("USERPROFILE", dir.path())
        .env_remove("RSPLUG_CONFIG_FILES");
    command
}

#[test]
fn help_documents_existing_cli_surface_without_env_value() {
    let output = rsplug()
        .env("RSPLUG_CONFIG_FILES", "secret-a.toml:secret-b.toml")
        .arg("--help")
        .output()
        .expect("run --help");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "-i, --install",
        "-u, --update",
        "--locked",
        "--lockfile <LOCKFILE>",
        "<CONFIG_FILES>...",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }
    assert!(!stdout.contains("secret-a.toml"));
    assert!(!stdout.contains("secret-b.toml"));
}

#[test]
fn version_succeeds_and_contains_package_version() {
    let output = rsplug().arg("--version").output().expect("run --version");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")), "{stdout}");
}

#[test]
fn missing_config_input_is_parse_error() {
    let output = rsplug()
        .env_remove("RSPLUG_CONFIG_FILES")
        .output()
        .expect("run without config");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn single_and_multiple_config_patterns_are_accepted() {
    let dir = fixture();

    assert!(
        isolated_command(&dir)
            .arg("a.toml")
            .status()
            .unwrap()
            .success()
    );
    assert!(
        isolated_command(&dir)
            .args(["a.toml", "b.toml"])
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn colon_delimited_argv_is_split() {
    let dir = fixture();
    assert!(
        isolated_command(&dir)
            .arg("a.toml:b.toml")
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn config_files_environment_fallback_is_split() {
    let dir = fixture();
    assert!(
        isolated_command(&dir)
            .env("RSPLUG_CONFIG_FILES", "a.toml:b.toml")
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn argv_takes_precedence_over_config_files_environment() {
    let dir = fixture();
    assert!(
        isolated_command(&dir)
            .env("RSPLUG_CONFIG_FILES", "[")
            .arg("a.toml")
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn update_and_locked_conflict() {
    let output = rsplug()
        .args(["--update", "--locked", "config.toml"])
        .output()
        .expect("run conflicting args");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn unknown_flags_are_rejected() {
    let output = rsplug()
        .args(["--definitely-unknown", "config.toml"])
        .output()
        .expect("run unknown flag");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn lockfile_requires_one_value_and_accepts_a_path() {
    let missing = rsplug()
        .args(["--lockfile"])
        .output()
        .expect("run missing lockfile value");
    assert_eq!(missing.status.code(), Some(2));

    let dir = fixture();
    assert!(
        isolated_command(&dir)
            .args(["--lockfile", "custom.lock.json", "a.toml"])
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn repeated_scalar_options_are_rejected() {
    let output = rsplug()
        .args([
            "--lockfile",
            "first.lock.json",
            "--lockfile",
            "second.lock.json",
            "config.toml",
        ])
        .output()
        .expect("run duplicate lockfile option");
    assert_eq!(output.status.code(), Some(2));
}
