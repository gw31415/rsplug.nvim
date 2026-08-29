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
    let dir = tempfile::tempdir().expect("create temp dir");
    let output = isolated_command(&dir)
        .env("RSPLUG_CONFIG_FILES", "secret-a.toml:secret-b.toml")
        .arg("--help")
        .output()
        .expect("run --help");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let tokens: Vec<_> = stdout
        .split_whitespace()
        .map(|token| token.trim_end_matches(','))
        .collect();
    for expected in [
        "-i",
        "--install",
        "-u",
        "--update",
        "--locked",
        "--lockfile",
        "--completion",
        "--man",
    ] {
        assert!(
            tokens.contains(&expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }
    assert!(stdout.contains("CONFIG_FILES"), "{stdout}");
    for shell in ["bash", "zsh", "fish", "nu", "powershell"] {
        assert!(
            stdout.contains(shell),
            "missing shell {shell:?} in:\n{stdout}"
        );
    }
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("Usage: rsplug ")),
        "{stdout}"
    );
    assert!(
        stdout.contains("A blazingly fast Neovim plugin manager written in Rust"),
        "{stdout}"
    );
    assert!(!stdout.contains("secret-a.toml"));
    assert!(!stdout.contains("secret-b.toml"));
}

#[test]
fn version_succeeds_and_contains_package_version() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let output = isolated_command(&dir)
        .arg("--version")
        .output()
        .expect("run --version");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        format!("rsplug {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn missing_config_input_is_parse_error() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let output = isolated_command(&dir).output().expect("run without config");
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
    fs::write(dir.path().join("b.toml"), "not = [").expect("write invalid b.toml");
    let status = isolated_command(&dir)
        .arg("a.toml:b.toml")
        .status()
        .expect("run colon-delimited argv");
    assert_eq!(status.code(), Some(1));
}

#[test]
fn config_files_environment_fallback_is_split() {
    let dir = fixture();
    fs::write(dir.path().join("b.toml"), "not = [").expect("write invalid b.toml");
    let status = isolated_command(&dir)
        .env("RSPLUG_CONFIG_FILES", "a.toml:b.toml")
        .status()
        .expect("run colon-delimited environment fallback");
    assert_eq!(status.code(), Some(1));
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
    let dir = fixture();
    let output = isolated_command(&dir)
        .args(["--update", "--locked", "a.toml"])
        .output()
        .expect("run conflicting args");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn unknown_flags_are_rejected() {
    let dir = fixture();
    let output = isolated_command(&dir)
        .args(["--definitely-unknown", "a.toml"])
        .output()
        .expect("run unknown flag");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn lockfile_requires_one_value_and_accepts_a_path() {
    let missing_dir = tempfile::tempdir().expect("create temp dir");
    let missing = isolated_command(&missing_dir)
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
    let dir = fixture();
    let output = isolated_command(&dir)
        .args([
            "--lockfile",
            "first.lock.json",
            "--lockfile",
            "second.lock.json",
            "a.toml",
        ])
        .output()
        .expect("run duplicate lockfile option");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn man_generation_succeeds_without_config_and_has_no_runtime_side_effects() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let output = isolated_command(&dir)
        .arg("--man")
        .output()
        .expect("generate manpage");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with(".TH RSPLUG 1\n"), "{stdout}");
    assert!(stdout.contains(".SH NAME"), "{stdout}");
    assert!(stdout.contains(".SH OPTIONS"), "{stdout}");
    assert!(stdout.contains("rsplug"), "{stdout}");
    assert!(!dir.path().join(".cache/rsplug").exists());
}

#[test]
fn completion_generation_supports_documented_shells_without_config() {
    for shell in ["bash", "zsh", "fish", "nu", "powershell"] {
        let dir = tempfile::tempdir().expect("create temp dir");
        let output = isolated_command(&dir)
            .args(["--completion", shell])
            .output()
            .expect("generate completion");

        assert!(
            output.status.success(),
            "{shell}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("rsplug"), "{shell}: {stdout}");
        assert!(stdout.contains("__complete_word__"), "{shell}: {stdout}");
        assert!(!dir.path().join(".cache/rsplug").exists());
    }
}

#[test]
fn completion_protocol_uses_current_cli_metadata() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let output = isolated_command(&dir)
        .args([
            "__complete_word__",
            "--shell",
            "bash",
            "--line",
            "rsplug --",
        ])
        .output()
        .expect("ask for completion candidates");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "--completion",
        "--install",
        "--locked",
        "--lockfile",
        "--man",
        "--update",
    ] {
        assert!(
            stdout.lines().any(|line| line == expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }
}

#[test]
fn completion_detects_current_shell_when_omitted() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let output = isolated_command(&dir)
        .env("SHELL", "/opt/homebrew/bin/fish")
        .env("RSPLUG_CONFIG_FILES", "definitely-missing.toml")
        .arg("--completion")
        .output()
        .expect("generate completion for detected shell");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("# @generated by usage-argv"), "{stdout}");
    assert!(stdout.contains("shell fish"), "{stdout}");
}

#[test]
fn completion_rejects_an_unsupported_explicit_shell() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let invalid = isolated_command(&dir)
        .args(["--completion", "tcsh"])
        .output()
        .expect("run unsupported completion shell");
    assert_eq!(invalid.status.code(), Some(2));
}

#[test]
fn generation_modes_are_exclusive_with_normal_cli_inputs() {
    let normal_inputs: &[&[&str]] = &[
        &["--install"],
        &["--update"],
        &["--locked"],
        &["--lockfile", "custom.lock.json"],
        &["a.toml"],
    ];

    for generation in [&["--man"][..], &["--completion", "bash"][..]] {
        for normal in normal_inputs {
            let dir = fixture();
            let mut args = Vec::from(generation);
            args.extend_from_slice(normal);
            let output = isolated_command(&dir)
                .args(&args)
                .output()
                .expect("run exclusive generation mode");
            assert_eq!(
                output.status.code(),
                Some(2),
                "expected conflict for {args:?}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    let dir = fixture();
    let both = isolated_command(&dir)
        .args(["--man", "--completion", "bash"])
        .output()
        .expect("run both generation modes");
    assert_eq!(both.status.code(), Some(2));
}

#[test]
fn generation_modes_ignore_config_environment_fallback() {
    for generation in [&["--man"][..], &["--completion", "bash"][..]] {
        let dir = fixture();
        let output = isolated_command(&dir)
            .env("RSPLUG_CONFIG_FILES", "definitely-missing.toml")
            .args(generation)
            .output()
            .expect("run generation mode with config environment fallback");
        assert!(
            output.status.success(),
            "{generation:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!dir.path().join(".cache/rsplug").exists());
    }
}
