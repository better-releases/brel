use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::process::Command as ProcessCommand;
use tempfile::tempdir;

#[test]
fn release_pr_no_releasable_commits_exits_successfully() {
    let temp_dir = tempdir().unwrap();
    init_git_repo(temp_dir.path());

    fs::write(
        temp_dir.path().join("brel.toml"),
        r#"
[release_pr.version_updates]
"package.json" = ["version"]
"#,
    )
    .unwrap();
    fs::write(
        temp_dir.path().join("package.json"),
        r#"{ "name": "demo", "version": "0.1.0" }"#,
    )
    .unwrap();

    run_git(temp_dir.path(), &["add", "brel.toml", "package.json"]);
    run_git(temp_dir.path(), &["commit", "-m", "chore: initial files"]);

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("brel"));
    cmd.current_dir(temp_dir.path())
        .arg("release-pr")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "No releasable commits found. Skipping release PR.",
        ));
}

#[test]
fn init_without_config_creates_default_workflow() {
    let temp_dir = tempdir().unwrap();
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("brel"));

    cmd.current_dir(temp_dir.path())
        .args(["init", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "No config file found. Using defaults",
        ));

    let workflow = temp_dir.path().join(".github/workflows/release-pr.yml");
    let content = fs::read_to_string(workflow).unwrap();
    assert!(content.contains("# managed-by: brel"));
    assert!(content.contains("workflow_dispatch"));
    assert!(content.contains("fetch-depth: 0"));
    assert!(content.contains("GH_TOKEN: ${{ github.token }}"));
    assert!(content.contains("uses: orhun/git-cliff-action@v4"));
    assert!(content.contains("run: brel release-pr"));
    assert!(!content.contains("pull_request:"));
}

#[test]
fn init_with_disabled_changelog_omits_git_cliff_step() {
    let temp_dir = tempdir().unwrap();
    fs::write(
        temp_dir.path().join("brel.toml"),
        r#"
[release_pr.changelog]
enabled = false
"#,
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("brel"));
    cmd.current_dir(temp_dir.path())
        .args(["init", "--yes"])
        .assert()
        .success();

    let workflow = temp_dir.path().join(".github/workflows/release-pr.yml");
    let content = fs::read_to_string(workflow).unwrap();
    assert!(!content.contains("uses: orhun/git-cliff-action@v4"));
}

#[test]
fn init_with_enabled_tagging_adds_tag_job() {
    let temp_dir = tempdir().unwrap();
    fs::write(
        temp_dir.path().join("brel.toml"),
        r#"
[release_pr.tagging]
enabled = true
"#,
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("brel"));
    cmd.current_dir(temp_dir.path())
        .args(["init", "--yes"])
        .assert()
        .success();

    let workflow = temp_dir.path().join(".github/workflows/release-pr.yml");
    let content = fs::read_to_string(workflow).unwrap();
    assert!(content.contains("pull_request:"));
    assert!(content.contains("- closed"));
    assert!(content.contains("Create release tag"));
}

#[test]
fn dry_run_prints_diff_and_does_not_write() {
    let temp_dir = tempdir().unwrap();
    let workflow = temp_dir.path().join(".github/workflows/release-pr.yml");
    fs::create_dir_all(workflow.parent().unwrap()).unwrap();
    fs::write(&workflow, "# managed-by: brel\nname: old\n").unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("brel"));
    cmd.current_dir(temp_dir.path())
        .args(["init", "--yes", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Dry run: would overwrite"))
        .stdout(predicate::str::contains("@@"));

    let content = fs::read_to_string(workflow).unwrap();
    assert_eq!(content, "# managed-by: brel\nname: old\n");
}

fn init_git_repo(path: &std::path::Path) {
    run_git(path, &["init", "-q"]);
    run_git(path, &["config", "user.name", "Test User"]);
    run_git(path, &["config", "user.email", "test@example.com"]);
    fs::write(path.join(".gitkeep"), "seed").unwrap();
    run_git(path, &["add", ".gitkeep"]);
    run_git(path, &["commit", "-m", "chore: seed"]);
}

fn run_git(cwd: &std::path::Path, args: &[&str]) {
    let output = ProcessCommand::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}
