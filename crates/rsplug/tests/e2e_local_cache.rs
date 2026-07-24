use std::{fs, path::Path, process::Command};

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git must be installed for the local E2E fixture");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_rsplug(home: &Path, lockfile: &Path, config: &Path, mode: Option<&str>) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rsplug"));
    command.env("HOME", home).arg("--lockfile").arg(lockfile);
    if let Some(mode) = mode {
        command.arg(mode);
    }
    let output = command.arg(config).output().expect("rsplug must run");
    assert!(
        output.status.success(),
        "rsplug {:?} failed:\nstdout={}\nstderr={}",
        mode,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn isolated_local_git_install_refresh_and_locked_are_deterministic() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let work = root.path().join("work");
    let bare = root.path().join("remote.git");
    let config = root.path().join("plugins.toml");
    let lockfile = root.path().join("rsplug.lock.json");
    fs::create_dir_all(&home).unwrap();

    git(root.path(), &["init", "--bare", bare.to_str().unwrap()]);
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init"]);
    git(&work, &["config", "user.email", "rsplug@example.invalid"]);
    git(&work, &["config", "user.name", "rsplug E2E"]);
    fs::create_dir_all(work.join("plugin")).unwrap();
    fs::write(work.join("plugin/init.lua"), "vim.g.rsplug_e2e = true\n").unwrap();
    for index in 0..20 {
        fs::write(
            work.join(format!("plugin/file_{index:02}.lua")),
            format!("vim.g.rsplug_e2e_{index} = true\n"),
        )
        .unwrap();
    }
    git(&work, &["add", "."]);
    git(&work, &["commit", "-m", "initial"]);
    git(&work, &["branch", "-M", "main"]);
    git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&work, &["push", "origin", "main"]);
    // A bare repository created by `git init --bare` may retain HEAD on the
    // old default branch.  Point it at the branch used by this fixture so
    // rev resolution is deterministic across Git versions and runners.
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let repo = format!("file://{}", bare.display());
    fs::write(&config, format!("[[plugins]]\nrepo = {:?}\n", repo)).unwrap();

    run_rsplug(&home, &lockfile, &config, Some("--install"));
    let app = home.join(".cache/rsplug");
    let init = app.join("init.lua");
    assert!(init.is_file(), "cold install must publish init.lua");
    assert!(lockfile.is_file(), "cold install must write the lockfile");
    let first_mtime = fs::metadata(&init).unwrap().modified().unwrap();

    run_rsplug(&home, &lockfile, &config, Some("--install"));
    assert_eq!(
        fs::metadata(&init).unwrap().modified().unwrap(),
        first_mtime,
        "warm identical install must not mutate init.lua"
    );

    run_rsplug(&home, &lockfile, &config, None);
    assert_eq!(
        fs::metadata(&init).unwrap().modified().unwrap(),
        first_mtime,
        "flagless identical refresh must be a no-op"
    );

    run_rsplug(&home, &lockfile, &config, Some("--locked"));
    assert_eq!(
        fs::metadata(&init).unwrap().modified().unwrap(),
        first_mtime,
        "locked warm refresh must preserve the published generation"
    );

    // Change one of the twenty plugin files (a small, isolated update) and
    // verify that update publishes exactly the changed semantic input.
    fs::write(
        work.join("plugin/file_07.lua"),
        "vim.g.rsplug_e2e_7 = 'updated'\n",
    )
    .unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-m", "small update"]);
    git(&work, &["push", "origin", "main"]);
    run_rsplug(&home, &lockfile, &config, Some("--update"));
    let updated_mtime = fs::metadata(&init).unwrap().modified().unwrap();
    assert_ne!(updated_mtime, first_mtime, "changed snapshot must publish");

    run_rsplug(&home, &lockfile, &config, Some("--update"));
    assert_eq!(
        fs::metadata(&init).unwrap().modified().unwrap(),
        updated_mtime,
        "no-change update must be a no-op"
    );

    run_rsplug(&home, &lockfile, &config, None);
    run_rsplug(&home, &lockfile, &config, Some("--locked"));
    assert_eq!(
        fs::metadata(&init).unwrap().modified().unwrap(),
        updated_mtime,
        "flagless and locked refreshes must preserve the updated generation"
    );

    let lock: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&lockfile).unwrap()).unwrap();
    assert_eq!(lock["locked"].as_object().unwrap().len(), 1);
    assert!(
        app.join("pack/_gen").is_dir(),
        "isolated cache must contain generated pack output"
    );
}
