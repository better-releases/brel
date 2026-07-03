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
fn next_version_prints_semver_when_releasable_commits_exist() {
    let temp_dir = tempdir().unwrap();
    init_git_repo(temp_dir.path());

    fs::write(temp_dir.path().join("feature.txt"), "feat").unwrap();
    run_git(temp_dir.path(), &["add", "feature.txt"]);
    run_git(temp_dir.path(), &["commit", "-m", "feat: add feature"]);

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("brel"));
    cmd.current_dir(temp_dir.path())
        .arg("next-version")
        .assert()
        .success()
        .stdout(predicate::eq("0.1.0\n"));
}

#[test]
fn next_version_prints_nothing_when_no_releasable_commits_exist() {
    let temp_dir = tempdir().unwrap();
    init_git_repo(temp_dir.path());

    fs::write(temp_dir.path().join("notes.txt"), "docs").unwrap();
    run_git(temp_dir.path(), &["add", "notes.txt"]);
    run_git(temp_dir.path(), &["commit", "-m", "chore: add notes"]);

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("brel"));
    cmd.current_dir(temp_dir.path())
        .arg("next-version")
        .assert()
        .success()
        .stdout(predicate::eq(""));
}

#[test]
fn next_version_uses_configured_tag_template_for_baseline_detection() {
    let temp_dir = tempdir().unwrap();
    init_git_repo(temp_dir.path());

    fs::write(
        temp_dir.path().join("brel.toml"),
        r#"
[release_pr.tagging]
tag_template = "release-{version}"
"#,
    )
    .unwrap();
    run_git(temp_dir.path(), &["tag", "release-1.2.3"]);

    fs::write(temp_dir.path().join("feature.txt"), "feat").unwrap();
    run_git(temp_dir.path(), &["add", "feature.txt"]);
    run_git(temp_dir.path(), &["commit", "-m", "feat: add feature"]);

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("brel"));
    cmd.current_dir(temp_dir.path())
        .arg("next-version")
        .assert()
        .success()
        .stdout(predicate::eq("1.3.0\n"));
}

#[test]
fn next_version_ignores_non_matching_legacy_tags() {
    let temp_dir = tempdir().unwrap();
    init_git_repo(temp_dir.path());

    fs::write(
        temp_dir.path().join("brel.toml"),
        r#"
[release_pr.tagging]
tag_template = "release-{version}"
"#,
    )
    .unwrap();
    run_git(temp_dir.path(), &["tag", "v1.2.3"]);

    fs::write(temp_dir.path().join("feature.txt"), "feat").unwrap();
    run_git(temp_dir.path(), &["add", "feature.txt"]);
    run_git(temp_dir.path(), &["commit", "-m", "feat: add feature"]);

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("brel"));
    cmd.current_dir(temp_dir.path())
        .arg("next-version")
        .assert()
        .success()
        .stdout(predicate::eq("0.1.0\n"));
}

#[test]
fn validate_succeeds_with_discovered_config() {
    let temp_dir = tempdir().unwrap();
    fs::write(temp_dir.path().join("brel.toml"), "provider = \"github\"\n").unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("brel"));
    cmd.current_dir(temp_dir.path())
        .arg("validate")
        .assert()
        .success()
        .stdout(predicate::str::contains("Config file `"))
        .stdout(predicate::str::contains("/brel.toml` is valid."))
        .stderr(predicate::str::is_empty());
}

#[test]
fn validate_succeeds_with_explicit_config_path() {
    let temp_dir = tempdir().unwrap();
    let config_path = temp_dir.path().join("custom.toml");
    fs::write(&config_path, "provider = \"gitlab\"\n").unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("brel"));
    cmd.current_dir(temp_dir.path())
        .args(["validate", "--config", "custom.toml"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Config file `custom.toml` is valid.",
        ))
        .stderr(predicate::str::is_empty());
}

#[test]
fn validate_fails_when_no_config_exists() {
    let temp_dir = tempdir().unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("brel"));
    cmd.current_dir(temp_dir.path())
        .arg("validate")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "No config file found to validate. Create `brel.toml` or pass `--config <path>`.",
        ));
}

#[test]
fn validate_fails_on_invalid_toml() {
    let temp_dir = tempdir().unwrap();
    fs::write(temp_dir.path().join("brel.toml"), "provider = [").unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("brel"));
    cmd.current_dir(temp_dir.path())
        .arg("validate")
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not valid TOML"));
}

#[test]
fn validate_fails_on_semantic_config_errors() {
    let temp_dir = tempdir().unwrap();
    fs::write(
        temp_dir.path().join("brel.toml"),
        r#"
[release_pr]
release_branch_pattern = "brel/release/{{date}}"
"#,
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("brel"));
    cmd.current_dir(temp_dir.path())
        .arg("validate")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unsupported token"));
}

#[test]
fn validate_prints_warnings_but_succeeds() {
    let temp_dir = tempdir().unwrap();
    fs::write(
        temp_dir.path().join("brel.toml"),
        "provider = \"github\"\nexperimental = true\n",
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("brel"));
    cmd.current_dir(temp_dir.path())
        .arg("validate")
        .assert()
        .success()
        .stdout(predicate::str::contains("Config file `"))
        .stdout(predicate::str::contains("/brel.toml` is valid."))
        .stderr(predicate::str::contains(
            "warning: Unknown config key `experimental` was ignored.",
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

    let config = fs::read_to_string(temp_dir.path().join("brel.toml")).unwrap();
    assert!(config.contains("default_branch = \"main\""));

    let workflow = temp_dir.path().join(".github/workflows/release-pr.yml");
    let content = fs::read_to_string(workflow).unwrap();
    assert!(content.contains("# managed-by: brel"));
    assert!(content.contains("workflow_dispatch"));
    assert!(content.contains("fetch-depth: 0"));
    assert!(content.contains("uses: better-releases/setup-brel@v1"));
    assert!(!content.contains("BREL_RELEASE_REPO"));
    assert!(content.contains("id: next-version"));
    assert!(content.contains("next_version=\"$(brel next-version)\""));
    assert!(content.contains("GH_TOKEN: ${{ github.token }}"));
    assert!(content.contains("if: ${{ steps.next-version.outputs.version != '' }}"));
    assert!(
        content.contains("args: --unreleased --tag v${{ steps.next-version.outputs.version }}")
    );
    assert!(content.contains("--prepend CHANGELOG.md"));
    assert!(!content.contains("--output CHANGELOG.md"));
    assert!(content.contains("uses: orhun/git-cliff-action@v4"));
    assert!(!content.contains("uses: actions/setup-node@v6"));
    assert!(!content.contains("changelogen@"));
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
    assert!(!content.contains("uses: actions/setup-node@v6"));
    assert!(!content.contains("changelogen@"));
}

#[test]
fn init_with_changelogen_changelog_provider_emits_node_steps() {
    let temp_dir = tempdir().unwrap();
    fs::write(
        temp_dir.path().join("brel.toml"),
        r#"
[release_pr.changelog]
provider = "changelogen"
output_file = "docs/changelog.md"

[release_pr.changelog.changelogen]
version = "0.5.0"
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
    assert!(content.contains("uses: actions/setup-node@v6"));
    assert!(content.contains("node-version: 24"));
    assert!(content.contains("package-manager-cache: false"));
    assert!(content.contains(
        "run: npx --yes 'changelogen@0.5.0' --to HEAD -r \"${{ steps.next-version.outputs.version }}\" --output docs/changelog.md"
    ));
    assert!(!content.contains("uses: orhun/git-cliff-action@v4"));
    assert!(!content.contains("--bump"));
    assert!(!content.contains("--release"));
    assert!(!content.contains("--commit"));
    assert!(!content.contains("--tag"));
    assert!(!content.contains("--push"));
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
        .success()
        .stdout(predicate::str::contains(
            "Tagging is enabled. Add repository secret `BREL_TAG_PUSH_TOKEN`",
        ));

    let workflow = temp_dir.path().join(".github/workflows/release-pr.yml");
    let content = fs::read_to_string(workflow).unwrap();
    assert!(content.contains("pull_request:"));
    assert!(content.contains("- closed"));
    assert!(content.contains("Create release tag"));
}

#[test]
fn init_with_enabled_tagging_dry_run_prints_pat_notice() {
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
        .args(["init", "--yes", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Dry run: would create"))
        .stdout(predicate::str::contains(
            "Tagging is enabled. Add repository secret `BREL_TAG_PUSH_TOKEN`",
        ));
}

#[test]
fn init_with_disabled_tagging_does_not_print_pat_notice() {
    let temp_dir = tempdir().unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("brel"));
    cmd.current_dir(temp_dir.path())
        .args(["init", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("BREL_TAG_PUSH_TOKEN").not());
}

#[test]
fn init_with_custom_tag_template_updates_cliff_tag_arg() {
    let temp_dir = tempdir().unwrap();
    fs::write(
        temp_dir.path().join("brel.toml"),
        r#"
[release_pr.tagging]
tag_template = "{version}"
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
    assert!(content.contains("args: --unreleased --tag ${{ steps.next-version.outputs.version }}"));
    assert!(content.contains("--prepend CHANGELOG.md"));
    assert!(!content.contains("--output CHANGELOG.md"));
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
