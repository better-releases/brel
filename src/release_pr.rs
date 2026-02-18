use crate::cli::{NextVersionArgs, ReleasePrArgs};
use crate::config::{self, Provider, ReleasePrConfig, ResolvedConfig};
use crate::tag_template::TagTemplate;
use crate::template::{
    self, MANAGED_RELEASE_PR_MARKER, ReleasePrBodyContext, ReleasePrCommitContext,
};
use crate::version_update;
use anyhow::{Context, Result, bail};
use semver::Version;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run(args: ReleasePrArgs) -> Result<()> {
    let repo_root = std::env::current_dir().context("Failed to determine current directory.")?;
    let mut runner = ProcessRunner;
    run_with_runner(&repo_root, args.config.as_deref(), &mut runner, None)
}

pub fn run_next_version(args: NextVersionArgs) -> Result<()> {
    let repo_root = std::env::current_dir().context("Failed to determine current directory.")?;
    let mut runner = ProcessRunner;
    run_next_version_with_runner(&repo_root, args.config.as_deref(), &mut runner)
}

pub(crate) fn run_with_runner(
    repo_root: &Path,
    config_path: Option<&Path>,
    runner: &mut dyn CommandRunner,
    gh_token_override: Option<&str>,
) -> Result<()> {
    let config = load_supported_config(config_path, repo_root, "release-pr")?;
    let tag_template = TagTemplate::parse(&config.release_pr.tagging.tag_template)
        .context("Invalid normalized release tag template.")?;

    let Some(next_release) = resolve_next_release(runner, repo_root, &tag_template)? else {
        println!("No releasable commits found. Skipping release PR.");
        return Ok(());
    };

    if config.release_pr.version_updates.is_empty() {
        println!("No `release_pr.version_updates` configured. Nothing to update.");
        return Ok(());
    }

    let next_version_string = next_release.next_version.to_string();
    let next_tag = tag_template.render(&next_version_string);

    let update_report = version_update::apply_version_updates(
        repo_root,
        &next_version_string,
        &config.release_pr.version_updates,
        &config.release_pr.format_overrides,
    )?;
    if update_report.changed_files.is_empty() {
        println!("Version targets already set to {next_tag}. Nothing to commit.");
        return Ok(());
    }

    let gh_token = resolve_gh_token(gh_token_override)?;
    let gh_env = vec![("GH_TOKEN".to_string(), gh_token)];
    let managed_pr = find_managed_open_pr(runner, repo_root, &config, &gh_env)?;
    let release_branch = managed_pr
        .as_ref()
        .map(|pr| pr.head_ref_name.clone())
        .unwrap_or_else(|| {
            render_release_branch(
                &config.release_pr.release_branch_pattern,
                &next_version_string,
            )
        });

    git_checkout_branch(runner, repo_root, &release_branch)?;
    let mut files_to_stage = update_report.changed_files.clone();
    maybe_append_changelog_file(repo_root, &config.release_pr, &mut files_to_stage);
    git_add_files(runner, repo_root, &files_to_stage)?;
    if !git_has_staged_changes(runner, repo_root)? {
        println!("No staged changes after version updates. Skipping release PR.");
        return Ok(());
    }

    let commit_message = format!("chore(release): {next_tag}");
    git_commit(runner, repo_root, &config.release_pr, &commit_message)?;
    git_push_branch(runner, repo_root, &release_branch)?;

    let template_override = load_template_override(repo_root, &config.release_pr)?;
    let commit_contexts = next_release
        .commits
        .iter()
        .map(|commit| ReleasePrCommitContext {
            sha_short: short_sha(&commit.sha),
            subject: commit.subject.trim(),
        })
        .collect::<Vec<_>>();
    let pr_title = format!("Release {next_tag}");
    let pr_body = template::render_release_pr_body(
        &ReleasePrBodyContext {
            version: &next_version_string,
            tag: &next_tag,
            base_branch: &config.default_branch,
            release_branch: &release_branch,
            commits: &commit_contexts,
        },
        template_override.as_deref(),
    )?;

    match managed_pr {
        Some(pr) => gh_edit_pr(
            runner,
            repo_root,
            pr.number,
            &config.default_branch,
            &pr_title,
            &pr_body,
            &gh_env,
        )?,
        None => gh_create_pr(
            runner,
            repo_root,
            &config.default_branch,
            &release_branch,
            &pr_title,
            &pr_body,
            &gh_env,
        )?,
    }

    println!("Release PR prepared for tag {next_tag}.");
    Ok(())
}

pub(crate) fn run_next_version_with_runner(
    repo_root: &Path,
    config_path: Option<&Path>,
    runner: &mut dyn CommandRunner,
) -> Result<()> {
    let config = load_supported_config(config_path, repo_root, "next-version")?;
    let tag_template = TagTemplate::parse(&config.release_pr.tagging.tag_template)
        .context("Invalid normalized release tag template.")?;
    let Some(next_release) = resolve_next_release(runner, repo_root, &tag_template)? else {
        return Ok(());
    };

    println!("{}", next_release.next_version);
    Ok(())
}

fn load_supported_config(
    config_path: Option<&Path>,
    repo_root: &Path,
    command_name: &str,
) -> Result<ResolvedConfig> {
    let config = config::load(config_path, repo_root)?;
    for warning in &config.warnings {
        eprintln!("warning: {warning}");
    }

    if config.provider != Provider::Github {
        bail!(
            "Provider `{}` is configured, but `brel {command_name}` currently supports only `github`.",
            config.provider
        );
    }

    Ok(config)
}

fn load_template_override(
    repo_root: &Path,
    release_pr: &ReleasePrConfig,
) -> Result<Option<String>> {
    let Some(template_path) = &release_pr.pr_template_file else {
        return Ok(None);
    };

    let full_path = repo_root.join(template_path);
    let contents = fs::read_to_string(&full_path)
        .with_context(|| format!("Failed to read PR template file `{}`.", full_path.display()))?;
    Ok(Some(contents))
}

fn resolve_gh_token(override_token: Option<&str>) -> Result<String> {
    if let Some(token) = override_token {
        if token.trim().is_empty() {
            bail!(
                "Missing GitHub auth token. Set `GH_TOKEN` (or `GITHUB_TOKEN`) before running `brel release-pr`."
            );
        }
        return Ok(token.to_string());
    }

    if let Ok(value) = std::env::var("GH_TOKEN")
        && !value.trim().is_empty()
    {
        return Ok(value);
    }

    if let Ok(value) = std::env::var("GITHUB_TOKEN")
        && !value.trim().is_empty()
    {
        return Ok(value);
    }

    bail!(
        "Missing GitHub auth token. Set `GH_TOKEN` (or `GITHUB_TOKEN`) before running `brel release-pr`."
    )
}

fn render_release_branch(pattern: &str, version: &str) -> String {
    pattern.replace("{{version}}", version).trim().to_string()
}

fn short_sha(sha: &str) -> &str {
    let max = sha.len().min(7);
    &sha[..max]
}

#[derive(Debug, Clone)]
struct TaggedVersion {
    raw: String,
    version: Version,
}

#[derive(Debug, Clone)]
struct NextRelease {
    next_version: Version,
    commits: Vec<CommitInfo>,
}

fn resolve_next_release(
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    tag_template: &TagTemplate,
) -> Result<Option<NextRelease>> {
    let latest_tag = find_latest_release_tag(runner, repo_root, tag_template)?;
    let commits = collect_commits_since(
        runner,
        repo_root,
        latest_tag.as_ref().map(|tag| tag.raw.as_str()),
    )?;
    let Some(next_bump) = highest_bump(commits.iter()) else {
        return Ok(None);
    };

    let base_version = latest_tag
        .as_ref()
        .map(|tag| tag.version.clone())
        .unwrap_or_else(|| Version::new(0, 0, 0));

    Ok(Some(NextRelease {
        next_version: bump_version(&base_version, next_bump),
        commits,
    }))
}

fn find_latest_release_tag(
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    tag_template: &TagTemplate,
) -> Result<Option<TaggedVersion>> {
    let output = run_checked(
        runner,
        repo_root,
        "git",
        vec!["tag".to_string(), "--list".to_string()],
        &[],
        "Failed to list git tags.",
    )?;

    let mut latest: Option<TaggedVersion> = None;
    for raw_tag in output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let Some(parsed_version) = parse_release_tag(raw_tag, tag_template) else {
            continue;
        };

        let candidate = TaggedVersion {
            raw: raw_tag.to_string(),
            version: parsed_version,
        };

        let replace = match latest.as_ref() {
            None => true,
            Some(current) => candidate.version > current.version,
        };
        if replace {
            latest = Some(candidate);
        }
    }

    Ok(latest)
}

fn parse_release_tag(tag: &str, tag_template: &TagTemplate) -> Option<Version> {
    tag_template.parse_stable_version(tag)
}

#[derive(Debug, Clone)]
struct CommitInfo {
    sha: String,
    subject: String,
    body: String,
}

fn collect_commits_since(
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    latest_tag: Option<&str>,
) -> Result<Vec<CommitInfo>> {
    let mut args = vec!["log".to_string(), "--format=%H%x1f%s%x1f%b%x1e".to_string()];
    args.push(match latest_tag {
        Some(tag) => format!("{tag}..HEAD"),
        None => "HEAD".to_string(),
    });

    let output = run_checked(
        runner,
        repo_root,
        "git",
        args,
        &[],
        "Failed to read commit history for release calculation.",
    )?;

    let mut commits = Vec::new();
    for record in output.stdout.split('\u{1e}') {
        if record.trim().is_empty() {
            continue;
        }

        let mut parts = record.splitn(3, '\u{1f}');
        let sha = parts.next().unwrap_or("").trim();
        let subject = parts.next().unwrap_or("").trim();
        let body = parts.next().unwrap_or("").trim();
        if sha.is_empty() {
            continue;
        }

        commits.push(CommitInfo {
            sha: sha.to_string(),
            subject: subject.to_string(),
            body: body.to_string(),
        });
    }

    Ok(commits)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BumpLevel {
    Patch,
    Minor,
    Major,
}

fn highest_bump<'a>(commits: impl Iterator<Item = &'a CommitInfo>) -> Option<BumpLevel> {
    commits.filter_map(classify_commit).max()
}

fn classify_commit(commit: &CommitInfo) -> Option<BumpLevel> {
    if has_breaking_change(commit) {
        return Some(BumpLevel::Major);
    }

    let commit_type = conventional_commit_type(&commit.subject)?;
    if commit_type == "feat" {
        return Some(BumpLevel::Minor);
    }
    if commit_type == "fix" {
        return Some(BumpLevel::Patch);
    }
    None
}

fn has_breaking_change(commit: &CommitInfo) -> bool {
    if commit
        .body
        .lines()
        .any(|line| line.trim_start().starts_with("BREAKING CHANGE"))
    {
        return true;
    }

    let Some((prefix, _)) = commit.subject.split_once(':') else {
        return false;
    };
    prefix.contains('!')
}

fn conventional_commit_type(subject: &str) -> Option<String> {
    let (prefix, _) = subject.split_once(':')?;
    let normalized = prefix
        .trim()
        .trim_end_matches('!')
        .split_once('(')
        .map(|(kind, _)| kind)
        .unwrap_or(prefix)
        .trim()
        .to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }
    Some(normalized)
}

fn bump_version(base: &Version, level: BumpLevel) -> Version {
    let mut version = base.clone();
    match level {
        BumpLevel::Major => {
            version.major += 1;
            version.minor = 0;
            version.patch = 0;
        }
        BumpLevel::Minor => {
            version.minor += 1;
            version.patch = 0;
        }
        BumpLevel::Patch => {
            version.patch += 1;
        }
    }
    version.pre = semver::Prerelease::EMPTY;
    version.build = semver::BuildMetadata::EMPTY;
    version
}

#[derive(Debug, Clone, Deserialize)]
struct GhPullRequest {
    number: u64,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    body: Option<String>,
}

fn find_managed_open_pr(
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    config: &ResolvedConfig,
    gh_env: &[(String, String)],
) -> Result<Option<GhPullRequest>> {
    let output = run_checked(
        runner,
        repo_root,
        "gh",
        vec![
            "pr".to_string(),
            "list".to_string(),
            "--state".to_string(),
            "open".to_string(),
            "--base".to_string(),
            config.default_branch.clone(),
            "--json".to_string(),
            "number,headRefName,body".to_string(),
        ],
        gh_env,
        "Failed to list open pull requests via gh.",
    )?;

    let prs: Vec<GhPullRequest> = serde_json::from_str(&output.stdout)
        .context("Failed to parse `gh pr list` JSON output.")?;
    Ok(prs.into_iter().find(|pr| {
        pr.body
            .as_deref()
            .is_some_and(|body| body.contains(MANAGED_RELEASE_PR_MARKER))
    }))
}

fn git_checkout_branch(
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    branch: &str,
) -> Result<()> {
    run_checked(
        runner,
        repo_root,
        "git",
        vec!["checkout".to_string(), "-B".to_string(), branch.to_string()],
        &[],
        "Failed to create/switch release branch.",
    )?;
    Ok(())
}

fn git_add_files(
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    files: &[std::path::PathBuf],
) -> Result<()> {
    let mut args = vec!["add".to_string(), "--".to_string()];
    args.extend(files.iter().map(|path| path.to_string_lossy().to_string()));

    run_checked(
        runner,
        repo_root,
        "git",
        args,
        &[],
        "Failed to stage version update files.",
    )?;
    Ok(())
}

fn maybe_append_changelog_file(
    repo_root: &Path,
    release_pr: &ReleasePrConfig,
    files_to_stage: &mut Vec<PathBuf>,
) {
    if !release_pr.changelog.enabled {
        return;
    }

    let changelog_relative = PathBuf::from(&release_pr.changelog.output_file);
    if files_to_stage.contains(&changelog_relative) {
        return;
    }

    let changelog_full_path = repo_root.join(&changelog_relative);
    if changelog_full_path.is_file() {
        files_to_stage.push(changelog_relative);
    }
}

fn git_has_staged_changes(runner: &mut dyn CommandRunner, repo_root: &Path) -> Result<bool> {
    let output = runner.run(
        repo_root,
        "git",
        &[
            "diff".to_string(),
            "--cached".to_string(),
            "--quiet".to_string(),
        ],
        &[],
    )?;

    match output.status {
        0 => Ok(false),
        1 => Ok(true),
        _ => bail!(
            "Failed to inspect staged changes: git diff --cached --quiet exited with {}. {}",
            output.status,
            output.stderr.trim()
        ),
    }
}

fn git_commit(
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    release_pr: &ReleasePrConfig,
    message: &str,
) -> Result<()> {
    run_checked(
        runner,
        repo_root,
        "git",
        vec![
            "-c".to_string(),
            format!("user.name={}", release_pr.commit_author.name),
            "-c".to_string(),
            format!("user.email={}", release_pr.commit_author.email),
            "commit".to_string(),
            "-m".to_string(),
            message.to_string(),
        ],
        &[],
        "Failed to commit release changes.",
    )?;
    Ok(())
}

fn git_push_branch(runner: &mut dyn CommandRunner, repo_root: &Path, branch: &str) -> Result<()> {
    run_checked(
        runner,
        repo_root,
        "git",
        vec![
            "push".to_string(),
            "--force-with-lease".to_string(),
            "--set-upstream".to_string(),
            "origin".to_string(),
            branch.to_string(),
        ],
        &[],
        "Failed to push release branch.",
    )?;
    Ok(())
}

fn gh_create_pr(
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    base_branch: &str,
    release_branch: &str,
    title: &str,
    body: &str,
    gh_env: &[(String, String)],
) -> Result<()> {
    run_checked(
        runner,
        repo_root,
        "gh",
        vec![
            "pr".to_string(),
            "create".to_string(),
            "--base".to_string(),
            base_branch.to_string(),
            "--head".to_string(),
            release_branch.to_string(),
            "--title".to_string(),
            title.to_string(),
            "--body".to_string(),
            body.to_string(),
        ],
        gh_env,
        "Failed to create release pull request.",
    )?;
    Ok(())
}

fn gh_edit_pr(
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    number: u64,
    base_branch: &str,
    title: &str,
    body: &str,
    gh_env: &[(String, String)],
) -> Result<()> {
    run_checked(
        runner,
        repo_root,
        "gh",
        vec![
            "pr".to_string(),
            "edit".to_string(),
            number.to_string(),
            "--base".to_string(),
            base_branch.to_string(),
            "--title".to_string(),
            title.to_string(),
            "--body".to_string(),
            body.to_string(),
        ],
        gh_env,
        "Failed to update existing release pull request.",
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

pub trait CommandRunner {
    fn run(
        &mut self,
        cwd: &Path,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<CommandOutput>;
}

struct ProcessRunner;

impl CommandRunner for ProcessRunner {
    fn run(
        &mut self,
        cwd: &Path,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<CommandOutput> {
        let output = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .envs(env.iter().cloned())
            .output()
            .with_context(|| format!("Failed to execute `{program}`. Is it installed?"))?;

        Ok(CommandOutput {
            status: output.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
}

fn run_checked(
    runner: &mut dyn CommandRunner,
    cwd: &Path,
    program: &str,
    args: Vec<String>,
    env: &[(String, String)],
    context: &str,
) -> Result<CommandOutput> {
    let output = runner.run(cwd, program, &args, env)?;
    if output.status != 0 {
        let stderr = output.stderr.trim();
        let details = if stderr.is_empty() {
            "no stderr output"
        } else {
            stderr
        };
        bail!(
            "{context} Command `{}` failed (exit {}): {details}",
            format_command(program, &args),
            output.status
        );
    }
    Ok(output)
}

fn format_command(program: &str, args: &[String]) -> String {
    if args.is_empty() {
        return program.to_string();
    }
    format!("{program} {}", args.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::fs;
    use tempfile::tempdir;

    #[derive(Debug, Clone)]
    struct RecordedCall {
        program: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    }

    struct ScriptedRunner {
        responses: VecDeque<CommandOutput>,
        calls: Vec<RecordedCall>,
    }

    impl ScriptedRunner {
        fn new(responses: Vec<CommandOutput>) -> Self {
            Self {
                responses: responses.into(),
                calls: Vec::new(),
            }
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn run(
            &mut self,
            _cwd: &Path,
            program: &str,
            args: &[String],
            env: &[(String, String)],
        ) -> Result<CommandOutput> {
            self.calls.push(RecordedCall {
                program: program.to_string(),
                args: args.to_vec(),
                env: env.to_vec(),
            });
            self.responses
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("Missing scripted response for `{program}`"))
        }
    }

    fn ok(stdout: &str) -> CommandOutput {
        CommandOutput {
            status: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    fn status(code: i32) -> CommandOutput {
        CommandOutput {
            status: code,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    fn err_status(code: i32, stderr: &str) -> CommandOutput {
        CommandOutput {
            status: code,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    fn log_entry(sha: &str, subject: &str, body: &str) -> String {
        format!("{sha}\u{1f}{subject}\u{1f}{body}\u{1e}")
    }

    #[test]
    fn parse_release_tag_supports_only_configured_template() {
        let template = TagTemplate::parse("release-{version}").unwrap();
        assert_eq!(
            parse_release_tag("release-1.2.3", &template),
            Some(Version::parse("1.2.3").unwrap())
        );
        assert!(parse_release_tag("v1.2.3", &template).is_none());
        assert!(parse_release_tag("1.2.3", &template).is_none());
        assert!(parse_release_tag("release-1.2.3-rc.1", &template).is_none());
    }

    #[test]
    fn classify_commits_uses_conventional_commit_rules() {
        let patch = CommitInfo {
            sha: "a".to_string(),
            subject: "fix: patch bug".to_string(),
            body: String::new(),
        };
        let minor = CommitInfo {
            sha: "b".to_string(),
            subject: "feat(api): add endpoint".to_string(),
            body: String::new(),
        };
        let major = CommitInfo {
            sha: "c".to_string(),
            subject: "refactor!: rewrite API".to_string(),
            body: String::new(),
        };

        assert_eq!(classify_commit(&patch), Some(BumpLevel::Patch));
        assert_eq!(classify_commit(&minor), Some(BumpLevel::Minor));
        assert_eq!(classify_commit(&major), Some(BumpLevel::Major));
    }

    #[test]
    fn resolve_next_release_returns_bumped_version_and_commits() {
        let temp_dir = tempdir().unwrap();
        let mut runner = ScriptedRunner::new(vec![
            ok("v1.2.3\n"),
            ok(&log_entry("abc123456789", "feat: add feature", "")),
        ]);
        let template = TagTemplate::parse("v{version}").unwrap();

        let release = resolve_next_release(&mut runner, temp_dir.path(), &template)
            .unwrap()
            .expect("expected releasable version");

        assert_eq!(release.next_version, Version::new(1, 3, 0));
        assert_eq!(release.commits.len(), 1);
        assert_eq!(release.commits[0].subject, "feat: add feature");
    }

    #[test]
    fn resolve_next_release_returns_none_when_no_releasable_commits() {
        let temp_dir = tempdir().unwrap();
        let mut runner = ScriptedRunner::new(vec![
            ok("v1.2.3\n"),
            ok(&log_entry("abc123456789", "chore: update docs", "")),
        ]);
        let template = TagTemplate::parse("v{version}").unwrap();

        let release = resolve_next_release(&mut runner, temp_dir.path(), &template).unwrap();
        assert!(release.is_none());
    }

    #[test]
    fn no_releasable_commits_exits_without_gh_calls() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
[release_pr.version_updates]
"package.json" = ["version"]
"#,
        )
        .unwrap();

        let mut runner = ScriptedRunner::new(vec![
            ok("v1.2.3\n"),
            ok(&log_entry(
                "abc123456789",
                "chore: update docs",
                "no releasable change",
            )),
        ]);

        run_with_runner(temp_dir.path(), None, &mut runner, Some("token")).unwrap();
        assert_eq!(runner.calls.len(), 2);
        assert!(runner.calls.iter().all(|call| call.program == "git"));
    }

    #[test]
    fn existing_release_pr_branch_is_reused() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
default_branch = "main"

[release_pr.version_updates]
"package.json" = ["version"]
"#,
        )
        .unwrap();
        fs::write(
            temp_dir.path().join("package.json"),
            r#"{ "name": "demo", "version": "1.2.3" }"#,
        )
        .unwrap();

        let existing_pr_json = format!(
            r#"[{{"number":7,"headRefName":"brel/release/v1.2.3","body":"{}\nold body"}}]"#,
            MANAGED_RELEASE_PR_MARKER
        );
        let mut runner = ScriptedRunner::new(vec![
            ok("v1.2.3\n"),
            ok(&log_entry("abc123456789", "feat: add feature", "")),
            ok(&existing_pr_json),
            ok(""),
            ok(""),
            status(1),
            ok(""),
            ok(""),
            ok(""),
        ]);

        run_with_runner(temp_dir.path(), None, &mut runner, Some("token")).unwrap();

        assert!(runner.calls.iter().any(|call| call.program == "git"
            && call.args
                == vec![
                    "checkout".to_string(),
                    "-B".to_string(),
                    "brel/release/v1.2.3".to_string()
                ]));
        assert!(runner.calls.iter().any(|call| {
            call.program == "gh"
                && call
                    .args
                    .starts_with(&["pr".to_string(), "edit".to_string(), "7".to_string()])
        }));
    }

    #[test]
    fn tag_template_updates_commit_and_pr_title() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
[release_pr.tagging]
tag_template = "{version}"

[release_pr.version_updates]
"package.json" = ["version"]
"#,
        )
        .unwrap();
        fs::write(
            temp_dir.path().join("package.json"),
            r#"{ "name": "demo", "version": "1.2.3" }"#,
        )
        .unwrap();

        let mut runner = ScriptedRunner::new(vec![
            ok("1.2.3\n"),
            ok(&log_entry("abc123456789", "feat: add feature", "")),
            ok("[]"),
            ok(""),
            ok(""),
            status(1),
            ok(""),
            ok(""),
            ok(""),
        ]);

        run_with_runner(temp_dir.path(), None, &mut runner, Some("token")).unwrap();

        assert!(runner.calls.iter().any(|call| {
            call.program == "git"
                && call.args.first() == Some(&"-c".to_string())
                && call.args.contains(&"chore(release): 1.3.0".to_string())
        }));

        assert!(runner.calls.iter().any(|call| {
            call.program == "gh"
                && call.args.contains(&"--title".to_string())
                && call.args.contains(&"Release 1.3.0".to_string())
        }));
    }

    #[test]
    fn missing_gh_token_is_an_error() {
        let temp_dir = tempdir().unwrap();
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
            r#"{ "name": "demo", "version": "1.2.3" }"#,
        )
        .unwrap();

        let mut runner = ScriptedRunner::new(vec![
            ok("v1.2.3\n"),
            ok(&log_entry("abc123456789", "fix: patch", "")),
        ]);

        let err = run_with_runner(temp_dir.path(), None, &mut runner, Some("")).unwrap_err();
        assert!(err.to_string().contains("Missing GitHub auth token"));
    }

    #[test]
    fn custom_pr_template_file_is_rendered() {
        let temp_dir = tempdir().unwrap();
        fs::create_dir_all(temp_dir.path().join(".github/brel")).unwrap();
        fs::write(
            temp_dir.path().join(".github/brel/release-pr-body.hbs"),
            "<!-- managed-by: brel -->\nVersion {{version}} from {{base_branch}}",
        )
        .unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
default_branch = "main"

[release_pr]
pr_template_file = ".github/brel/release-pr-body.hbs"

[release_pr.version_updates]
"package.json" = ["version"]
"#,
        )
        .unwrap();
        fs::write(
            temp_dir.path().join("package.json"),
            r#"{ "name": "demo", "version": "1.2.3" }"#,
        )
        .unwrap();

        let mut runner = ScriptedRunner::new(vec![
            ok("v1.2.3\n"),
            ok(&log_entry("abc123456789", "feat: add feature", "")),
            ok("[]"),
            ok(""),
            ok(""),
            status(1),
            ok(""),
            ok(""),
            ok(""),
        ]);

        run_with_runner(temp_dir.path(), None, &mut runner, Some("token")).unwrap();
        assert!(runner.calls.iter().any(|call| {
            call.program == "gh"
                && call.args.contains(&"--body".to_string())
                && call
                    .args
                    .iter()
                    .any(|arg| arg.contains("Version 1.3.0 from main"))
        }));
    }

    #[test]
    fn custom_pr_template_render_error_fails() {
        let temp_dir = tempdir().unwrap();
        fs::create_dir_all(temp_dir.path().join(".github/brel")).unwrap();
        fs::write(
            temp_dir.path().join(".github/brel/release-pr-body.hbs"),
            "{{#if",
        )
        .unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
[release_pr]
pr_template_file = ".github/brel/release-pr-body.hbs"

[release_pr.version_updates]
"package.json" = ["version"]
"#,
        )
        .unwrap();
        fs::write(
            temp_dir.path().join("package.json"),
            r#"{ "name": "demo", "version": "1.2.3" }"#,
        )
        .unwrap();

        let mut runner = ScriptedRunner::new(vec![
            ok("v1.2.3\n"),
            ok(&log_entry("abc123456789", "feat: add feature", "")),
            ok("[]"),
            ok(""),
            ok(""),
            status(1),
            ok(""),
            ok(""),
        ]);

        let err = run_with_runner(temp_dir.path(), None, &mut runner, Some("token")).unwrap_err();
        assert!(err.to_string().contains("Failed to register template"));
    }

    #[test]
    fn no_tags_use_zero_zero_zero_baseline() {
        let temp_dir = tempdir().unwrap();
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
            r#"{ "name": "demo", "version": "0.0.0" }"#,
        )
        .unwrap();

        let mut runner = ScriptedRunner::new(vec![
            ok(""),
            ok(&log_entry("abc123456789", "fix: patch", "")),
            ok("[]"),
            ok(""),
            ok(""),
            status(1),
            ok(""),
            ok(""),
            ok(""),
        ]);

        run_with_runner(temp_dir.path(), None, &mut runner, Some("token")).unwrap();
        assert!(runner.calls.iter().any(|call| {
            call.program == "git"
                && call.args
                    == vec![
                        "checkout".to_string(),
                        "-B".to_string(),
                        "brel/release/v0.0.1".to_string(),
                    ]
        }));
    }

    #[test]
    fn gh_failure_is_actionable() {
        let temp_dir = tempdir().unwrap();
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
            r#"{ "name": "demo", "version": "1.2.3" }"#,
        )
        .unwrap();

        let mut runner = ScriptedRunner::new(vec![
            ok("v1.2.3\n"),
            ok(&log_entry("abc123456789", "fix: patch", "")),
            err_status(127, "gh: command not found"),
        ]);

        let err = run_with_runner(temp_dir.path(), None, &mut runner, Some("token")).unwrap_err();
        let err_text = format!("{err:#}");
        assert!(err_text.contains("Failed to list open pull requests via gh."));
        assert!(err_text.contains("gh pr list"));
    }

    #[test]
    fn gh_commands_receive_token_env() {
        let temp_dir = tempdir().unwrap();
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
            r#"{ "name": "demo", "version": "1.2.3" }"#,
        )
        .unwrap();

        let mut runner = ScriptedRunner::new(vec![
            ok("v1.2.3\n"),
            ok(&log_entry("abc123456789", "fix: patch", "")),
            ok("[]"),
            ok(""),
            ok(""),
            status(1),
            ok(""),
            ok(""),
            ok(""),
        ]);

        run_with_runner(temp_dir.path(), None, &mut runner, Some("abc-token")).unwrap();

        let gh_calls = runner
            .calls
            .iter()
            .filter(|call| call.program == "gh")
            .collect::<Vec<_>>();
        assert!(!gh_calls.is_empty());
        assert!(gh_calls.iter().all(|call| {
            call.env
                .iter()
                .any(|(key, value)| key == "GH_TOKEN" && value == "abc-token")
        }));
    }

    #[test]
    fn stages_changelog_file_when_enabled_and_present() {
        let temp_dir = tempdir().unwrap();
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
            r#"{ "name": "demo", "version": "1.2.3" }"#,
        )
        .unwrap();
        fs::write(temp_dir.path().join("CHANGELOG.md"), "# Changelog\n").unwrap();

        let mut runner = ScriptedRunner::new(vec![
            ok("v1.2.3\n"),
            ok(&log_entry("abc123456789", "fix: patch", "")),
            ok("[]"),
            ok(""),
            ok(""),
            status(1),
            ok(""),
            ok(""),
            ok(""),
        ]);

        run_with_runner(temp_dir.path(), None, &mut runner, Some("abc-token")).unwrap();

        let add_call = runner
            .calls
            .iter()
            .find(|call| call.program == "git" && call.args.first() == Some(&"add".to_string()))
            .expect("missing git add call");

        assert!(add_call.args.contains(&"package.json".to_string()));
        assert!(add_call.args.contains(&"CHANGELOG.md".to_string()));
    }

    #[test]
    fn does_not_stage_changelog_file_when_disabled() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
[release_pr.changelog]
enabled = false

[release_pr.version_updates]
"package.json" = ["version"]
"#,
        )
        .unwrap();
        fs::write(
            temp_dir.path().join("package.json"),
            r#"{ "name": "demo", "version": "1.2.3" }"#,
        )
        .unwrap();
        fs::write(temp_dir.path().join("CHANGELOG.md"), "# Changelog\n").unwrap();

        let mut runner = ScriptedRunner::new(vec![
            ok("v1.2.3\n"),
            ok(&log_entry("abc123456789", "fix: patch", "")),
            ok("[]"),
            ok(""),
            ok(""),
            status(1),
            ok(""),
            ok(""),
            ok(""),
        ]);

        run_with_runner(temp_dir.path(), None, &mut runner, Some("abc-token")).unwrap();

        let add_call = runner
            .calls
            .iter()
            .find(|call| call.program == "git" && call.args.first() == Some(&"add".to_string()))
            .expect("missing git add call");

        assert!(add_call.args.contains(&"package.json".to_string()));
        assert!(!add_call.args.contains(&"CHANGELOG.md".to_string()));
    }
}
