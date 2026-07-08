use crate::cli::{ChangelogArgs, NextVersionArgs, ReleasePrArgs, TagArgs};
use crate::config::{self, ChangelogProvider, Provider, ReleasePrConfig, ResolvedConfig};
use crate::tag_template::TagTemplate;
use crate::template::{
    self, MANAGED_RELEASE_PR_MARKER, ReleasePrBodyContext, ReleasePrCommitContext,
};
use crate::version_update;
use anyhow::{Context, Result, bail};
use semver::Version;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub fn run(args: ReleasePrArgs) -> Result<()> {
    let repo_root = std::env::current_dir().context("Failed to determine current directory.")?;
    let mut runner = ProcessRunner;
    run_with_runner(&repo_root, args.config.as_deref(), &mut runner, None)
}

pub fn run_changelog(args: ChangelogArgs) -> Result<()> {
    let repo_root = std::env::current_dir().context("Failed to determine current directory.")?;
    let mut runner = ProcessRunner;
    run_changelog_with_runner(&repo_root, args.config.as_deref(), &mut runner)
}

pub fn run_tag(args: TagArgs) -> Result<()> {
    let repo_root = std::env::current_dir().context("Failed to determine current directory.")?;
    let mut runner = ProcessRunner;
    run_tag_with_runner(
        &repo_root,
        args.config.as_deref(),
        args.tag.as_deref(),
        args.target.as_deref(),
        &mut runner,
        None,
    )
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
    auth_token_override: Option<&str>,
) -> Result<()> {
    let mut gitlab_client = ReqwestGitlabHttpClient::new()?;
    let mut forgejo_client = ReqwestForgejoHttpClient::new()?;
    run_with_runner_and_clients(
        repo_root,
        config_path,
        runner,
        ProviderRuntime {
            auth_token_override,
            gitlab_env_override: None,
            forgejo_env_override: None,
            gitlab_client: &mut gitlab_client,
            forgejo_client: &mut forgejo_client,
        },
    )
}

#[cfg(test)]
fn run_with_runner_and_gitlab_client(
    repo_root: &Path,
    config_path: Option<&Path>,
    runner: &mut dyn CommandRunner,
    auth_token_override: Option<&str>,
    gitlab_env_override: Option<GitlabEnv>,
    gitlab_client: &mut dyn GitlabHttpClient,
) -> Result<()> {
    let mut forgejo_client = ReqwestForgejoHttpClient::new()?;
    run_with_runner_and_clients(
        repo_root,
        config_path,
        runner,
        ProviderRuntime {
            auth_token_override,
            gitlab_env_override,
            forgejo_env_override: None,
            gitlab_client,
            forgejo_client: &mut forgejo_client,
        },
    )
}

struct ProviderRuntime<'client, 'token> {
    auth_token_override: Option<&'token str>,
    gitlab_env_override: Option<GitlabEnv>,
    forgejo_env_override: Option<ForgejoEnv>,
    gitlab_client: &'client mut dyn GitlabHttpClient,
    forgejo_client: &'client mut dyn ForgejoHttpClient,
}

fn run_with_runner_and_clients(
    repo_root: &Path,
    config_path: Option<&Path>,
    runner: &mut dyn CommandRunner,
    runtime: ProviderRuntime<'_, '_>,
) -> Result<()> {
    let config = load_config(config_path, repo_root)?;
    ensure_release_pr_provider_supported(config.provider, "release-pr")?;
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

    let mut provider = ForgeProvider::new(
        config.provider,
        runtime.auth_token_override,
        runtime.gitlab_env_override,
        runtime.forgejo_env_override,
        runtime.gitlab_client,
        runtime.forgejo_client,
    )?;
    let managed_prs = provider.find_managed_open_prs(runner, repo_root, &config)?;
    let release_branch = render_release_branch(
        &config.release_pr.release_branch_pattern,
        &next_version_string,
    );
    let current_pr = managed_prs
        .iter()
        .find(|pr| pr.head_ref_name == release_branch)
        .cloned();
    let stale_prs = managed_prs
        .into_iter()
        .filter(|pr| pr.head_ref_name != release_branch)
        .collect::<Vec<_>>();

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

    let mut stale_close_failures = Vec::new();
    {
        match current_pr {
            Some(pr) => provider.edit_pr(
                runner,
                repo_root,
                pr.number,
                &config.default_branch,
                &pr_title,
                &pr_body,
            )?,
            None => provider.create_pr(
                runner,
                repo_root,
                &config.default_branch,
                &release_branch,
                &pr_title,
                &pr_body,
            )?,
        }

        for pr in stale_prs {
            if let Err(err) =
                provider.close_stale_release_pr(runner, repo_root, &pr, &release_branch)
            {
                eprintln!(
                    "warning: failed to close stale release PR #{} (`{}`): {err:#}",
                    pr.number, pr.head_ref_name
                );
                stale_close_failures
                    .push(format!("#{} (`{}`): {err:#}", pr.number, pr.head_ref_name));
            }
        }
    }
    if !stale_close_failures.is_empty() {
        bail!(
            "Failed to close {} stale release pull request(s): {}",
            stale_close_failures.len(),
            stale_close_failures.join("; ")
        );
    }

    println!("Release PR prepared for tag {next_tag}.");
    Ok(())
}

pub(crate) fn run_next_version_with_runner(
    repo_root: &Path,
    config_path: Option<&Path>,
    runner: &mut dyn CommandRunner,
) -> Result<()> {
    let config = load_config(config_path, repo_root)?;
    let tag_template = TagTemplate::parse(&config.release_pr.tagging.tag_template)
        .context("Invalid normalized release tag template.")?;
    let Some(next_release) = resolve_next_release(runner, repo_root, &tag_template)? else {
        return Ok(());
    };

    println!("{}", next_release.next_version);
    Ok(())
}

pub(crate) fn run_changelog_with_runner(
    repo_root: &Path,
    config_path: Option<&Path>,
    runner: &mut dyn CommandRunner,
) -> Result<()> {
    let config = load_config(config_path, repo_root)?;
    if !config.release_pr.changelog.enabled {
        println!("Changelog generation is disabled. Skipping changelog.");
        return Ok(());
    }

    let tag_template = TagTemplate::parse(&config.release_pr.tagging.tag_template)
        .context("Invalid normalized release tag template.")?;
    let Some(next_release) = resolve_next_release(runner, repo_root, &tag_template)? else {
        println!("No releasable commits found. Skipping changelog.");
        return Ok(());
    };

    let next_version_string = next_release.next_version.to_string();
    let next_tag = tag_template.render(&next_version_string);
    match config.release_pr.changelog.provider {
        ChangelogProvider::GitCliff => run_git_cliff_changelog(
            runner,
            repo_root,
            &next_tag,
            &config.release_pr.changelog.output_file,
        )?,
        ChangelogProvider::Changelogen => run_changelogen_changelog(
            runner,
            repo_root,
            &next_version_string,
            &config.release_pr.changelog.output_file,
            &config.release_pr.changelog.changelogen.version,
        )?,
    }

    println!("Generated changelog for tag {next_tag}.");
    Ok(())
}

pub(crate) fn run_tag_with_runner(
    repo_root: &Path,
    config_path: Option<&Path>,
    tag_arg: Option<&str>,
    target_arg: Option<&str>,
    runner: &mut dyn CommandRunner,
    github_event_path_override: Option<&Path>,
) -> Result<()> {
    let mut gitlab_client = ReqwestGitlabHttpClient::new()?;
    let mut event_context = TagEventContext {
        github_event_path_override,
        forgejo_event_path_override: github_event_path_override,
        gitlab_token_override: None,
        gitlab_env_override: None,
        gitlab_client: &mut gitlab_client,
    };
    run_tag_with_runner_with_event_context(
        repo_root,
        config_path,
        tag_arg,
        target_arg,
        runner,
        &mut event_context,
    )
}

struct TagEventContext<'a> {
    github_event_path_override: Option<&'a Path>,
    forgejo_event_path_override: Option<&'a Path>,
    gitlab_token_override: Option<&'a str>,
    gitlab_env_override: Option<GitlabEnv>,
    gitlab_client: &'a mut dyn GitlabHttpClient,
}

fn run_tag_with_runner_with_event_context(
    repo_root: &Path,
    config_path: Option<&Path>,
    tag_arg: Option<&str>,
    target_arg: Option<&str>,
    runner: &mut dyn CommandRunner,
    event_context: &mut TagEventContext<'_>,
) -> Result<()> {
    let config = load_config(config_path, repo_root)?;
    if !config.release_pr.tagging.enabled && tag_arg.is_none() {
        println!("Tagging is disabled. Skipping tag creation.");
        return Ok(());
    }

    let tag_template = TagTemplate::parse(&config.release_pr.tagging.tag_template)
        .context("Invalid normalized release tag template.")?;
    let tag_request = resolve_tag_request(
        repo_root,
        &config,
        &tag_template,
        tag_arg,
        target_arg,
        event_context,
    )?;
    let Some(tag_request) = tag_request else {
        return Ok(());
    };

    create_and_push_tag(
        runner,
        repo_root,
        &tag_request.tag,
        &tag_request.target,
        tag_request.mode,
    )
}

fn load_config(config_path: Option<&Path>, repo_root: &Path) -> Result<ResolvedConfig> {
    let config = config::load(config_path, repo_root)?;
    for warning in &config.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(config)
}

fn ensure_release_pr_provider_supported(provider: Provider, command_name: &str) -> Result<()> {
    if !matches!(
        provider,
        Provider::Github | Provider::Gitlab | Provider::Forgejo
    ) {
        bail!(
            "Provider `{}` is configured, but `brel {command_name}` currently supports only `github`, `gitlab`, or `forgejo`.",
            provider
        );
    }
    Ok(())
}

fn run_git_cliff_changelog(
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    tag: &str,
    output_file: &str,
) -> Result<()> {
    run_checked(
        runner,
        repo_root,
        "git-cliff",
        vec![
            "--config".to_string(),
            "cliff.toml".to_string(),
            "--unreleased".to_string(),
            "--tag".to_string(),
            tag.to_string(),
            "--prepend".to_string(),
            output_file.to_string(),
        ],
        &[],
        "Failed to generate changelog with git-cliff.",
    )?;
    Ok(())
}

fn run_changelogen_changelog(
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    version: &str,
    output_file: &str,
    changelogen_version: &str,
) -> Result<()> {
    run_checked(
        runner,
        repo_root,
        "npx",
        vec![
            "--yes".to_string(),
            format!("changelogen@{changelogen_version}"),
            "--to".to_string(),
            "HEAD".to_string(),
            "-r".to_string(),
            version.to_string(),
            "--output".to_string(),
            output_file.to_string(),
        ],
        &[],
        "Failed to generate changelog with changelogen.",
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TagRequest {
    tag: String,
    target: String,
    mode: TagRequestMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagRequestMode {
    Manual,
    GithubEvent,
    GitlabEvent,
    ForgejoEvent,
}

impl TagRequestMode {
    fn label(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::GithubEvent => "GitHub event",
            Self::GitlabEvent => "GitLab event",
            Self::ForgejoEvent => "Forgejo event",
        }
    }
}

fn resolve_tag_request(
    repo_root: &Path,
    config: &ResolvedConfig,
    tag_template: &TagTemplate,
    tag_arg: Option<&str>,
    target_arg: Option<&str>,
    event_context: &mut TagEventContext<'_>,
) -> Result<Option<TagRequest>> {
    if let Some(tag) = tag_arg {
        let tag = tag.trim();
        if tag.is_empty() {
            bail!("`--tag` cannot be empty.");
        }
        validate_explicit_tag(tag, tag_template)?;
        let target = target_arg
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("HEAD");
        return Ok(Some(TagRequest {
            tag: tag.to_string(),
            target: target.to_string(),
            mode: TagRequestMode::Manual,
        }));
    }

    match config.provider {
        Provider::Github => resolve_github_tag_request(
            repo_root,
            tag_template,
            target_arg,
            event_context.github_event_path_override,
        ),
        Provider::Gitlab => resolve_gitlab_tag_request(
            tag_template,
            target_arg,
            event_context.gitlab_token_override,
            event_context.gitlab_env_override.as_ref(),
            event_context.gitlab_client,
        ),
        Provider::Forgejo => resolve_forgejo_tag_request(
            repo_root,
            tag_template,
            target_arg,
            event_context.forgejo_event_path_override,
        ),
        Provider::Gitea => bail!(
            "Provider `{}` is configured, but `brel tag` event mode currently supports only `github`, `gitlab`, or `forgejo`. Pass `--tag` and `--target` for manual tag mode.",
            config.provider
        ),
    }
}

fn resolve_github_tag_request(
    repo_root: &Path,
    tag_template: &TagTemplate,
    target_arg: Option<&str>,
    github_event_path_override: Option<&Path>,
) -> Result<Option<TagRequest>> {
    let event_path = match github_event_path_override {
        Some(path) => path.to_path_buf(),
        None => std::env::var_os("GITHUB_EVENT_PATH")
            .map(PathBuf::from)
            .context("Missing `GITHUB_EVENT_PATH`. Pass `--tag` for manual tag mode.")?,
    };
    let event = read_github_pull_request_event(repo_root, &event_path)?;
    let Some(pull_request) = event.pull_request else {
        println!("GitHub event does not contain a pull request. Skipping tag creation.");
        return Ok(None);
    };
    if !pull_request.merged.unwrap_or(false) {
        println!("GitHub pull request was not merged. Skipping tag creation.");
        return Ok(None);
    }

    let body = pull_request.body.unwrap_or_default();
    if !body.contains(MANAGED_RELEASE_PR_MARKER) {
        println!("PR is not managed by brel. Skipping tag creation.");
        return Ok(None);
    }

    let title = pull_request.title.unwrap_or_default();
    let Some(tag) = parse_release_tag_from_title(&title) else {
        println!("PR title does not match expected release format. Skipping tag creation.");
        return Ok(None);
    };
    if tag_template.parse_stable_version(tag).is_none() {
        println!("PR title tag does not match configured tag template. Skipping tag creation.");
        return Ok(None);
    }

    let target = target_arg
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or(pull_request.merge_commit_sha)
        .unwrap_or_default();
    if target.trim().is_empty() {
        println!("Missing merge commit SHA. Skipping tag creation.");
        return Ok(None);
    }

    Ok(Some(TagRequest {
        tag: tag.to_string(),
        target,
        mode: TagRequestMode::GithubEvent,
    }))
}

fn resolve_forgejo_tag_request(
    repo_root: &Path,
    tag_template: &TagTemplate,
    target_arg: Option<&str>,
    forgejo_event_path_override: Option<&Path>,
) -> Result<Option<TagRequest>> {
    let event_path = match forgejo_event_path_override {
        Some(path) => path.to_path_buf(),
        None => std::env::var_os("FORGEJO_EVENT_PATH")
            .or_else(|| std::env::var_os("GITHUB_EVENT_PATH"))
            .map(PathBuf::from)
            .context(
                "Missing `FORGEJO_EVENT_PATH` (or `GITHUB_EVENT_PATH`). Pass `--tag` for manual tag mode.",
            )?,
    };
    let event = read_forgejo_pull_request_event(repo_root, &event_path)?;
    let Some(pull_request) = event.pull_request else {
        println!("Forgejo event does not contain a pull request. Skipping tag creation.");
        return Ok(None);
    };
    if !pull_request.merged.unwrap_or(false) {
        println!("Forgejo pull request was not merged. Skipping tag creation.");
        return Ok(None);
    }

    let body = pull_request.body.unwrap_or_default();
    if !body.contains(MANAGED_RELEASE_PR_MARKER) {
        println!("PR is not managed by brel. Skipping tag creation.");
        return Ok(None);
    }

    let title = pull_request.title.unwrap_or_default();
    let Some(tag) = parse_release_tag_from_title(&title) else {
        println!("PR title does not match expected release format. Skipping tag creation.");
        return Ok(None);
    };
    if tag_template.parse_stable_version(tag).is_none() {
        println!("PR title tag does not match configured tag template. Skipping tag creation.");
        return Ok(None);
    }

    let target = target_arg
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| non_empty_string(pull_request.merge_commit_sha))
        .or_else(|| non_empty_string(pull_request.merged_commit_id))
        .unwrap_or_default();
    if target.trim().is_empty() {
        println!("Missing merge commit SHA. Skipping tag creation.");
        return Ok(None);
    }

    Ok(Some(TagRequest {
        tag: tag.to_string(),
        target,
        mode: TagRequestMode::ForgejoEvent,
    }))
}

fn resolve_gitlab_tag_request(
    tag_template: &TagTemplate,
    target_arg: Option<&str>,
    gitlab_token_override: Option<&str>,
    gitlab_env_override: Option<&GitlabEnv>,
    gitlab_client: &mut dyn GitlabHttpClient,
) -> Result<Option<TagRequest>> {
    let token = resolve_gitlab_token(gitlab_token_override)?;
    let env = match gitlab_env_override {
        Some(env) => env.clone(),
        None => GitlabEnv::from_env_for_tag()?,
    };
    let merge_requests = gitlab_find_merged_merge_requests_for_commit(gitlab_client, &env, &token)?;
    let managed_merge_requests = merge_requests
        .iter()
        .filter(|mr| {
            mr.description
                .as_deref()
                .is_some_and(|body| body.contains(MANAGED_RELEASE_PR_MARKER))
        })
        .collect::<Vec<_>>();

    if managed_merge_requests.is_empty() {
        println!(
            "GitLab commit is not associated with a brel-managed merge request. Skipping tag creation."
        );
        return Ok(None);
    }

    for merge_request in managed_merge_requests {
        let title = merge_request.title.as_deref().unwrap_or_default();
        let Some(tag) = parse_release_tag_from_title(title) else {
            continue;
        };
        if tag_template.parse_stable_version(tag).is_none() {
            continue;
        }
        let target = target_arg
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| env.commit_sha.clone().expect("tag env includes commit sha"));

        return Ok(Some(TagRequest {
            tag: tag.to_string(),
            target,
            mode: TagRequestMode::GitlabEvent,
        }));
    }

    println!(
        "GitLab merge request title does not match expected release format. Skipping tag creation."
    );
    Ok(None)
}

fn validate_explicit_tag(tag: &str, tag_template: &TagTemplate) -> Result<()> {
    if tag_template.parse_stable_version(tag).is_some() {
        return Ok(());
    }

    bail!("Tag `{tag}` does not match configured `release_pr.tagging.tag_template`.")
}

fn non_empty_string(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    })
}

fn parse_release_tag_from_title(title: &str) -> Option<&str> {
    title
        .strip_prefix("Release ")
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
}

#[derive(Debug, Deserialize)]
struct GithubPullRequestEvent {
    pull_request: Option<GithubEventPullRequest>,
}

#[derive(Debug, Deserialize)]
struct GithubEventPullRequest {
    merged: Option<bool>,
    title: Option<String>,
    body: Option<String>,
    merge_commit_sha: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ForgejoPullRequestEvent {
    pull_request: Option<ForgejoEventPullRequest>,
}

#[derive(Debug, Deserialize)]
struct ForgejoEventPullRequest {
    merged: Option<bool>,
    title: Option<String>,
    body: Option<String>,
    merge_commit_sha: Option<String>,
    merged_commit_id: Option<String>,
}

fn read_github_pull_request_event(
    repo_root: &Path,
    event_path: &Path,
) -> Result<GithubPullRequestEvent> {
    let full_path = if event_path.is_absolute() {
        event_path.to_path_buf()
    } else {
        repo_root.join(event_path)
    };
    let contents = fs::read_to_string(&full_path).with_context(|| {
        format!(
            "Failed to read GitHub event file `{}`.",
            full_path.display()
        )
    })?;
    serde_json::from_str(&contents).with_context(|| {
        format!(
            "Failed to parse GitHub event file `{}`.",
            full_path.display()
        )
    })
}

fn read_forgejo_pull_request_event(
    repo_root: &Path,
    event_path: &Path,
) -> Result<ForgejoPullRequestEvent> {
    let full_path = if event_path.is_absolute() {
        event_path.to_path_buf()
    } else {
        repo_root.join(event_path)
    };
    let contents = fs::read_to_string(&full_path).with_context(|| {
        format!(
            "Failed to read Forgejo event file `{}`.",
            full_path.display()
        )
    })?;
    serde_json::from_str(&contents).with_context(|| {
        format!(
            "Failed to parse Forgejo event file `{}`.",
            full_path.display()
        )
    })
}

fn create_and_push_tag(
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    tag: &str,
    target: &str,
    mode: TagRequestMode,
) -> Result<()> {
    run_checked(
        runner,
        repo_root,
        "git",
        vec![
            "fetch".to_string(),
            "--tags".to_string(),
            "origin".to_string(),
        ],
        &[],
        "Failed to fetch git tags.",
    )?;

    if git_tag_exists(runner, repo_root, tag)? {
        println!("Tag {tag} already exists. Skipping.");
        return Ok(());
    }

    run_checked(
        runner,
        repo_root,
        "git",
        vec!["tag".to_string(), tag.to_string(), target.to_string()],
        &[],
        "Failed to create release tag.",
    )?;
    run_checked(
        runner,
        repo_root,
        "git",
        vec![
            "push".to_string(),
            "origin".to_string(),
            format!("refs/tags/{tag}"),
        ],
        &[],
        "Failed to push release tag.",
    )?;
    println!(
        "Created and pushed tag {tag} at {target} ({}).",
        mode.label()
    );
    Ok(())
}

fn git_tag_exists(runner: &mut dyn CommandRunner, repo_root: &Path, tag: &str) -> Result<bool> {
    let output = runner.run(
        repo_root,
        "git",
        &[
            "rev-parse".to_string(),
            "--verify".to_string(),
            "--quiet".to_string(),
            format!("refs/tags/{tag}"),
        ],
        &[],
    )?;

    match output.status {
        0 => Ok(true),
        1 => Ok(false),
        _ => bail!(
            "Failed to inspect tag `{tag}`: git rev-parse exited with {}. {}",
            output.status,
            output.stderr.trim()
        ),
    }
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

fn resolve_gitlab_token(override_token: Option<&str>) -> Result<String> {
    if let Some(token) = override_token {
        if token.trim().is_empty() {
            bail!(
                "Missing GitLab auth token. Set `BREL_GITLAB_TOKEN` before running GitLab provider commands."
            );
        }
        return Ok(token.to_string());
    }

    if let Ok(value) = std::env::var("BREL_GITLAB_TOKEN")
        && !value.trim().is_empty()
    {
        return Ok(value);
    }

    bail!(
        "Missing GitLab auth token. Set `BREL_GITLAB_TOKEN` before running GitLab provider commands."
    )
}

fn resolve_forgejo_token(override_token: Option<&str>) -> Result<String> {
    if let Some(token) = override_token {
        if token.trim().is_empty() {
            bail!(
                "Missing Forgejo auth token. Set `BREL_FORGEJO_TOKEN` (or `FORGEJO_TOKEN`) before running Forgejo provider commands."
            );
        }
        return Ok(token.to_string());
    }

    for name in ["BREL_FORGEJO_TOKEN", "FORGEJO_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(value) = std::env::var(name)
            && !value.trim().is_empty()
        {
            return Ok(value);
        }
    }

    bail!(
        "Missing Forgejo auth token. Set `BREL_FORGEJO_TOKEN` (or `FORGEJO_TOKEN`) before running Forgejo provider commands."
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

enum ForgeProvider<'client> {
    Github {
        env: Vec<(String, String)>,
    },
    Gitlab {
        env: GitlabEnv,
        token: String,
        client: &'client mut dyn GitlabHttpClient,
    },
    Forgejo {
        env: ForgejoEnv,
        token: String,
        client: &'client mut dyn ForgejoHttpClient,
    },
}

impl<'client> ForgeProvider<'client> {
    fn new(
        provider: Provider,
        auth_token_override: Option<&str>,
        gitlab_env_override: Option<GitlabEnv>,
        forgejo_env_override: Option<ForgejoEnv>,
        gitlab_client: &'client mut dyn GitlabHttpClient,
        forgejo_client: &'client mut dyn ForgejoHttpClient,
    ) -> Result<Self> {
        match provider {
            Provider::Github => {
                let token = resolve_gh_token(auth_token_override)?;
                Ok(Self::Github {
                    env: vec![("GH_TOKEN".to_string(), token)],
                })
            }
            Provider::Gitlab => {
                let token = resolve_gitlab_token(auth_token_override)?;
                let env = match gitlab_env_override {
                    Some(env) => env,
                    None => GitlabEnv::from_env_for_project()?,
                };
                Ok(Self::Gitlab {
                    env,
                    token,
                    client: gitlab_client,
                })
            }
            Provider::Forgejo => {
                let token = resolve_forgejo_token(auth_token_override)?;
                let env = match forgejo_env_override {
                    Some(env) => env,
                    None => ForgejoEnv::from_env_for_project()?,
                };
                Ok(Self::Forgejo {
                    env,
                    token,
                    client: forgejo_client,
                })
            }
            Provider::Gitea => bail!(
                "Provider `{}` is configured, but release PR creation currently supports only `github`, `gitlab`, or `forgejo`.",
                provider
            ),
        }
    }

    fn find_managed_open_prs(
        &mut self,
        runner: &mut dyn CommandRunner,
        repo_root: &Path,
        config: &ResolvedConfig,
    ) -> Result<Vec<ForgePullRequest>> {
        match self {
            Self::Github { env } => github_find_managed_open_prs(runner, repo_root, config, env),
            Self::Gitlab { env, token, client } => {
                gitlab_find_managed_open_merge_requests(*client, env, token, config)
            }
            Self::Forgejo { env, token, client } => {
                forgejo_find_managed_open_pull_requests(*client, env, token, config)
            }
        }
    }

    fn create_pr(
        &mut self,
        runner: &mut dyn CommandRunner,
        repo_root: &Path,
        base_branch: &str,
        release_branch: &str,
        title: &str,
        body: &str,
    ) -> Result<()> {
        match self {
            Self::Github { env } => gh_create_pr(
                runner,
                repo_root,
                base_branch,
                release_branch,
                title,
                body,
                env,
            ),
            Self::Gitlab { env, token, client } => gitlab_create_merge_request(
                *client,
                env,
                token,
                base_branch,
                release_branch,
                title,
                body,
            ),
            Self::Forgejo { env, token, client } => forgejo_create_pull_request(
                *client,
                env,
                token,
                base_branch,
                release_branch,
                title,
                body,
            ),
        }
    }

    fn edit_pr(
        &mut self,
        runner: &mut dyn CommandRunner,
        repo_root: &Path,
        number: u64,
        base_branch: &str,
        title: &str,
        body: &str,
    ) -> Result<()> {
        match self {
            Self::Github { env } => {
                gh_edit_pr(runner, repo_root, number, base_branch, title, body, env)
            }
            Self::Gitlab { env, token, client } => {
                gitlab_update_merge_request(*client, env, token, number, base_branch, title, body)
            }
            Self::Forgejo { env, token, client } => {
                forgejo_update_pull_request(*client, env, token, number, base_branch, title, body)
            }
        }
    }

    fn close_stale_release_pr(
        &mut self,
        runner: &mut dyn CommandRunner,
        repo_root: &Path,
        pr: &ForgePullRequest,
        release_branch: &str,
    ) -> Result<()> {
        match self {
            Self::Github { env } => {
                github_close_stale_release_pr(runner, repo_root, pr, release_branch, env)
            }
            Self::Gitlab { env, token, client } => gitlab_close_stale_release_pr(
                *client,
                env,
                token,
                runner,
                repo_root,
                pr,
                release_branch,
            ),
            Self::Forgejo { env, token, client } => forgejo_close_stale_release_pr(
                *client,
                env,
                token,
                runner,
                repo_root,
                pr,
                release_branch,
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ForgePullRequest {
    number: u64,
    head_ref_name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GhPullRequest {
    number: u64,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    body: Option<String>,
}

impl From<GhPullRequest> for ForgePullRequest {
    fn from(pr: GhPullRequest) -> Self {
        Self {
            number: pr.number,
            head_ref_name: pr.head_ref_name,
        }
    }
}

fn github_find_managed_open_prs(
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    config: &ResolvedConfig,
    gh_env: &[(String, String)],
) -> Result<Vec<ForgePullRequest>> {
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
    Ok(prs
        .into_iter()
        .filter(|pr| {
            pr.body
                .as_deref()
                .is_some_and(|body| body.contains(MANAGED_RELEASE_PR_MARKER))
        })
        .map(ForgePullRequest::from)
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ForgejoEnv {
    server_url: String,
    repository: String,
}

impl ForgejoEnv {
    fn from_env_for_project() -> Result<Self> {
        Ok(Self {
            server_url: required_env_with_alias("FORGEJO_SERVER_URL", "GITHUB_SERVER_URL")?,
            repository: required_env_with_alias("FORGEJO_REPOSITORY", "GITHUB_REPOSITORY")?,
        })
    }
}

fn required_env_with_alias(primary: &str, alias: &str) -> Result<String> {
    for name in [primary, alias] {
        if let Ok(value) = std::env::var(name) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }

    bail!("Missing Forgejo Actions variable `{primary}` (or `{alias}`).")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForgejoHttpMethod {
    Get,
    Post,
    Patch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ForgejoHttpResponse {
    status: u16,
    body: String,
}

trait ForgejoHttpClient {
    fn request(
        &mut self,
        method: ForgejoHttpMethod,
        url: &str,
        token: &str,
        body: Option<serde_json::Value>,
    ) -> Result<ForgejoHttpResponse>;
}

struct ReqwestForgejoHttpClient {
    client: reqwest::blocking::Client,
}

const FORGEJO_HTTP_TIMEOUT_SECS: u64 = 60;

impl ReqwestForgejoHttpClient {
    fn new() -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(FORGEJO_HTTP_TIMEOUT_SECS))
            .build()
            .context("Failed to build Forgejo HTTP client.")?;
        Ok(Self { client })
    }
}

impl ForgejoHttpClient for ReqwestForgejoHttpClient {
    fn request(
        &mut self,
        method: ForgejoHttpMethod,
        url: &str,
        token: &str,
        body: Option<serde_json::Value>,
    ) -> Result<ForgejoHttpResponse> {
        let request = match method {
            ForgejoHttpMethod::Get => self.client.get(url),
            ForgejoHttpMethod::Post => self.client.post(url),
            ForgejoHttpMethod::Patch => self.client.patch(url),
        }
        .header("Authorization", format!("token {token}"))
        .header("Accept", "application/json");

        let request = match body {
            Some(body) => request.json(&body),
            None => request,
        };

        let response = request
            .send()
            .with_context(|| format!("Failed to send Forgejo API request to `{url}`."))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .with_context(|| format!("Failed to read Forgejo API response from `{url}`."))?;

        Ok(ForgejoHttpResponse { status, body })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ForgejoPullRequest {
    number: u64,
    #[serde(default)]
    head_branch: String,
    head: Option<ForgejoPullRequestHead>,
    body: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ForgejoPullRequestHead {
    #[serde(default, rename = "ref")]
    ref_name: String,
    // Forgejo's `PRBranchInfo` exposes the branch name under the JSON key `label`
    // (the Go SDK field is `Name`, but its JSON tag is `label`). When the head
    // branch has been deleted, `ref` becomes `refs/pull/<n>/head` while `label`
    // still holds the original branch name, so it is the reliable fallback.
    #[serde(default, rename = "label")]
    label: String,
}

impl ForgejoPullRequest {
    fn head_ref_name(&self) -> String {
        if !self.head_branch.is_empty() {
            return self.head_branch.clone();
        }

        let Some(head) = &self.head else {
            return String::new();
        };
        if !head.ref_name.is_empty() {
            return head.ref_name.clone();
        }
        head.label.clone()
    }
}

impl From<ForgejoPullRequest> for ForgePullRequest {
    fn from(pull_request: ForgejoPullRequest) -> Self {
        Self {
            number: pull_request.number,
            head_ref_name: pull_request.head_ref_name(),
        }
    }
}

fn forgejo_find_managed_open_pull_requests(
    client: &mut dyn ForgejoHttpClient,
    env: &ForgejoEnv,
    token: &str,
    config: &ResolvedConfig,
) -> Result<Vec<ForgePullRequest>> {
    let url = format!(
        "{}?state=open&base={}",
        forgejo_api_url(env, "pulls"),
        url_encode(&config.default_branch)
    );
    let pull_requests: Vec<ForgejoPullRequest> = forgejo_request_json(
        client,
        ForgejoHttpMethod::Get,
        &url,
        token,
        None,
        "Failed to list open pull requests via Forgejo API.",
    )?;

    Ok(pull_requests
        .into_iter()
        .filter(|pull_request| {
            pull_request
                .body
                .as_deref()
                .is_some_and(|body| body.contains(MANAGED_RELEASE_PR_MARKER))
        })
        .map(ForgePullRequest::from)
        .collect())
}

fn forgejo_create_pull_request(
    client: &mut dyn ForgejoHttpClient,
    env: &ForgejoEnv,
    token: &str,
    base_branch: &str,
    release_branch: &str,
    title: &str,
    body: &str,
) -> Result<()> {
    let _: serde_json::Value = forgejo_request_json(
        client,
        ForgejoHttpMethod::Post,
        &forgejo_api_url(env, "pulls"),
        token,
        Some(serde_json::json!({
            "base": base_branch,
            "head": release_branch,
            "title": title,
            "body": body,
        })),
        "Failed to create release pull request via Forgejo API.",
    )?;
    Ok(())
}

fn forgejo_update_pull_request(
    client: &mut dyn ForgejoHttpClient,
    env: &ForgejoEnv,
    token: &str,
    number: u64,
    base_branch: &str,
    title: &str,
    body: &str,
) -> Result<()> {
    let _: serde_json::Value = forgejo_request_json(
        client,
        ForgejoHttpMethod::Patch,
        &forgejo_api_url(env, &format!("pulls/{number}")),
        token,
        Some(serde_json::json!({
            "base": base_branch,
            "title": title,
            "body": body,
        })),
        "Failed to update existing release pull request via Forgejo API.",
    )?;
    Ok(())
}

fn forgejo_close_stale_release_pr(
    client: &mut dyn ForgejoHttpClient,
    env: &ForgejoEnv,
    token: &str,
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    pr: &ForgePullRequest,
    release_branch: &str,
) -> Result<()> {
    let close_comment = format!(
        "Closing this managed release PR because the next release version changed and the release branch is now `{release_branch}`."
    );
    forgejo_close_pull_request(client, env, token, pr.number, &close_comment)?;
    git_delete_remote_branch_best_effort(runner, repo_root, &pr.head_ref_name);
    Ok(())
}

fn forgejo_close_pull_request(
    client: &mut dyn ForgejoHttpClient,
    env: &ForgejoEnv,
    token: &str,
    number: u64,
    close_comment: &str,
) -> Result<()> {
    let _: serde_json::Value = forgejo_request_json(
        client,
        ForgejoHttpMethod::Post,
        &forgejo_api_url(env, &format!("issues/{number}/comments")),
        token,
        Some(serde_json::json!({
            "body": close_comment,
        })),
        "Failed to comment on stale release pull request via Forgejo API.",
    )?;
    let _: serde_json::Value = forgejo_request_json(
        client,
        ForgejoHttpMethod::Patch,
        &forgejo_api_url(env, &format!("pulls/{number}")),
        token,
        Some(serde_json::json!({
            "state": "closed",
        })),
        "Failed to close stale release pull request via Forgejo API.",
    )?;
    Ok(())
}

fn forgejo_request_json<T: DeserializeOwned>(
    client: &mut dyn ForgejoHttpClient,
    method: ForgejoHttpMethod,
    url: &str,
    token: &str,
    body: Option<serde_json::Value>,
    context: &str,
) -> Result<T> {
    let response = client.request(method, url, token, body)?;
    if !(200..300).contains(&response.status) {
        let details = response.body.trim();
        let details = if details.is_empty() {
            "no response body"
        } else {
            details
        };
        bail!(
            "{context} Forgejo API returned HTTP {}: {details}",
            response.status
        );
    }

    serde_json::from_str(&response.body)
        .with_context(|| format!("{context} Failed to parse Forgejo API response."))
}

fn forgejo_api_url(env: &ForgejoEnv, path: &str) -> String {
    format!(
        "{}/api/v1/repos/{}/{}",
        env.server_url.trim_end_matches('/'),
        repo_path_url_encode(&env.repository),
        path.trim_start_matches('/')
    )
}

fn repo_path_url_encode(repository: &str) -> String {
    repository
        .split('/')
        .map(url_encode)
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitlabEnv {
    api_v4_url: String,
    project_id: String,
    commit_sha: Option<String>,
}

impl GitlabEnv {
    fn from_env_for_project() -> Result<Self> {
        Ok(Self {
            api_v4_url: required_env("CI_API_V4_URL")?,
            project_id: required_env("CI_PROJECT_ID")?,
            commit_sha: std::env::var("CI_COMMIT_SHA")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
        })
    }

    fn from_env_for_tag() -> Result<Self> {
        let mut env = Self::from_env_for_project()?;
        let commit_sha = required_env("CI_COMMIT_SHA")?;
        env.commit_sha = Some(commit_sha);
        Ok(env)
    }
}

fn required_env(name: &str) -> Result<String> {
    let value = std::env::var(name)
        .with_context(|| format!("Missing GitLab CI variable `{name}`."))?
        .trim()
        .to_string();
    if value.is_empty() {
        bail!("Missing GitLab CI variable `{name}`.");
    }
    Ok(value)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitlabHttpMethod {
    Get,
    Post,
    Put,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitlabHttpResponse {
    status: u16,
    body: String,
}

trait GitlabHttpClient {
    fn request(
        &mut self,
        method: GitlabHttpMethod,
        url: &str,
        token: &str,
        body: Option<serde_json::Value>,
    ) -> Result<GitlabHttpResponse>;
}

struct ReqwestGitlabHttpClient {
    client: reqwest::blocking::Client,
}

const GITLAB_HTTP_TIMEOUT_SECS: u64 = 60;

impl ReqwestGitlabHttpClient {
    fn new() -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(GITLAB_HTTP_TIMEOUT_SECS))
            .build()
            .context("Failed to build GitLab HTTP client.")?;
        Ok(Self { client })
    }
}

impl GitlabHttpClient for ReqwestGitlabHttpClient {
    fn request(
        &mut self,
        method: GitlabHttpMethod,
        url: &str,
        token: &str,
        body: Option<serde_json::Value>,
    ) -> Result<GitlabHttpResponse> {
        let request = match method {
            GitlabHttpMethod::Get => self.client.get(url),
            GitlabHttpMethod::Post => self.client.post(url),
            GitlabHttpMethod::Put => self.client.put(url),
        }
        .header("PRIVATE-TOKEN", token)
        .header("Accept", "application/json");

        let request = match body {
            Some(body) => request.json(&body),
            None => request,
        };

        let response = request
            .send()
            .with_context(|| format!("Failed to send GitLab API request to `{url}`."))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .with_context(|| format!("Failed to read GitLab API response from `{url}`."))?;

        Ok(GitlabHttpResponse { status, body })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GitlabMergeRequest {
    iid: u64,
    #[serde(default)]
    source_branch: String,
    title: Option<String>,
    description: Option<String>,
}

impl From<GitlabMergeRequest> for ForgePullRequest {
    fn from(merge_request: GitlabMergeRequest) -> Self {
        Self {
            number: merge_request.iid,
            head_ref_name: merge_request.source_branch,
        }
    }
}

fn gitlab_find_managed_open_merge_requests(
    client: &mut dyn GitlabHttpClient,
    env: &GitlabEnv,
    token: &str,
    config: &ResolvedConfig,
) -> Result<Vec<ForgePullRequest>> {
    let url = format!(
        "{}?state=opened&target_branch={}",
        gitlab_api_url(env, "merge_requests"),
        url_encode(&config.default_branch)
    );
    let merge_requests: Vec<GitlabMergeRequest> = gitlab_request_json(
        client,
        GitlabHttpMethod::Get,
        &url,
        token,
        None,
        "Failed to list open merge requests via GitLab API.",
    )?;

    Ok(merge_requests
        .into_iter()
        .filter(|merge_request| {
            merge_request
                .description
                .as_deref()
                .is_some_and(|body| body.contains(MANAGED_RELEASE_PR_MARKER))
        })
        .map(ForgePullRequest::from)
        .collect())
}

fn gitlab_create_merge_request(
    client: &mut dyn GitlabHttpClient,
    env: &GitlabEnv,
    token: &str,
    base_branch: &str,
    release_branch: &str,
    title: &str,
    body: &str,
) -> Result<()> {
    let _: serde_json::Value = gitlab_request_json(
        client,
        GitlabHttpMethod::Post,
        &gitlab_api_url(env, "merge_requests"),
        token,
        Some(serde_json::json!({
            "source_branch": release_branch,
            "target_branch": base_branch,
            "title": title,
            "description": body,
        })),
        "Failed to create release merge request via GitLab API.",
    )?;
    Ok(())
}

fn gitlab_update_merge_request(
    client: &mut dyn GitlabHttpClient,
    env: &GitlabEnv,
    token: &str,
    iid: u64,
    base_branch: &str,
    title: &str,
    body: &str,
) -> Result<()> {
    let _: serde_json::Value = gitlab_request_json(
        client,
        GitlabHttpMethod::Put,
        &gitlab_api_url(env, &format!("merge_requests/{iid}")),
        token,
        Some(serde_json::json!({
            "target_branch": base_branch,
            "title": title,
            "description": body,
        })),
        "Failed to update existing release merge request via GitLab API.",
    )?;
    Ok(())
}

fn gitlab_close_stale_release_pr(
    client: &mut dyn GitlabHttpClient,
    env: &GitlabEnv,
    token: &str,
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    pr: &ForgePullRequest,
    release_branch: &str,
) -> Result<()> {
    let close_comment = format!(
        "Closing this managed release PR because the next release version changed and the release branch is now `{release_branch}`."
    );
    gitlab_close_merge_request(client, env, token, pr.number, &close_comment)?;
    git_delete_remote_branch_best_effort(runner, repo_root, &pr.head_ref_name);
    Ok(())
}

fn gitlab_close_merge_request(
    client: &mut dyn GitlabHttpClient,
    env: &GitlabEnv,
    token: &str,
    iid: u64,
    close_comment: &str,
) -> Result<()> {
    let _: serde_json::Value = gitlab_request_json(
        client,
        GitlabHttpMethod::Post,
        &gitlab_api_url(env, &format!("merge_requests/{iid}/notes")),
        token,
        Some(serde_json::json!({
            "body": close_comment,
        })),
        "Failed to comment on stale release merge request via GitLab API.",
    )?;
    let _: serde_json::Value = gitlab_request_json(
        client,
        GitlabHttpMethod::Put,
        &gitlab_api_url(env, &format!("merge_requests/{iid}")),
        token,
        Some(serde_json::json!({
            "state_event": "close",
        })),
        "Failed to close stale release merge request via GitLab API.",
    )?;
    Ok(())
}

fn gitlab_find_merged_merge_requests_for_commit(
    client: &mut dyn GitlabHttpClient,
    env: &GitlabEnv,
    token: &str,
) -> Result<Vec<GitlabMergeRequest>> {
    let commit_sha = env
        .commit_sha
        .as_deref()
        .context("Missing GitLab CI variable `CI_COMMIT_SHA`.")?;
    let url = format!(
        "{}?state=merged",
        gitlab_api_url(
            env,
            &format!(
                "repository/commits/{}/merge_requests",
                url_encode(commit_sha)
            )
        )
    );
    gitlab_request_json(
        client,
        GitlabHttpMethod::Get,
        &url,
        token,
        None,
        "Failed to list merged merge requests for GitLab commit.",
    )
}

fn gitlab_request_json<T: DeserializeOwned>(
    client: &mut dyn GitlabHttpClient,
    method: GitlabHttpMethod,
    url: &str,
    token: &str,
    body: Option<serde_json::Value>,
    context: &str,
) -> Result<T> {
    let response = client.request(method, url, token, body)?;
    if !(200..300).contains(&response.status) {
        let details = response.body.trim();
        let details = if details.is_empty() {
            "no response body"
        } else {
            details
        };
        bail!(
            "{context} GitLab API returned HTTP {}: {details}",
            response.status
        );
    }

    serde_json::from_str(&response.body)
        .with_context(|| format!("{context} Failed to parse GitLab API response."))
}

fn gitlab_api_url(env: &GitlabEnv, path: &str) -> String {
    format!(
        "{}/projects/{}/{}",
        env.api_v4_url.trim_end_matches('/'),
        url_encode(&env.project_id),
        path.trim_start_matches('/')
    )
}

fn url_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    encoded
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

fn github_close_stale_release_pr(
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    pr: &ForgePullRequest,
    release_branch: &str,
    gh_env: &[(String, String)],
) -> Result<()> {
    let close_comment = format!(
        "Closing this managed release PR because the next release version changed and the release branch is now `{release_branch}`."
    );
    gh_close_pr(runner, repo_root, pr.number, &close_comment, gh_env)?;
    git_delete_remote_branch_best_effort(runner, repo_root, &pr.head_ref_name);
    Ok(())
}

fn gh_close_pr(
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    number: u64,
    comment: &str,
    gh_env: &[(String, String)],
) -> Result<()> {
    run_checked(
        runner,
        repo_root,
        "gh",
        vec![
            "pr".to_string(),
            "close".to_string(),
            number.to_string(),
            "--comment".to_string(),
            comment.to_string(),
        ],
        gh_env,
        "Failed to close stale release pull request.",
    )?;
    Ok(())
}

fn git_delete_remote_branch_best_effort(
    runner: &mut dyn CommandRunner,
    repo_root: &Path,
    branch: &str,
) {
    let args = vec![
        "push".to_string(),
        "origin".to_string(),
        "--delete".to_string(),
        branch.to_string(),
    ];
    match runner.run(repo_root, "git", &args, &[]) {
        Ok(output) if output.status == 0 => {}
        Ok(output) => {
            let stderr = output.stderr.trim();
            let details = if stderr.is_empty() {
                "no stderr output"
            } else {
                stderr
            };
            eprintln!(
                "warning: failed to delete stale release branch `{branch}` from origin: {details}"
            );
        }
        Err(err) => {
            eprintln!(
                "warning: failed to delete stale release branch `{branch}` from origin: {err:#}"
            );
        }
    }
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

    #[derive(Debug, Clone)]
    struct RecordedGitlabRequest {
        method: GitlabHttpMethod,
        url: String,
        token: String,
        body: Option<serde_json::Value>,
    }

    struct ScriptedGitlabClient {
        responses: VecDeque<GitlabHttpResponse>,
        requests: Vec<RecordedGitlabRequest>,
    }

    impl ScriptedGitlabClient {
        fn new(responses: Vec<GitlabHttpResponse>) -> Self {
            Self {
                responses: responses.into(),
                requests: Vec::new(),
            }
        }
    }

    impl GitlabHttpClient for ScriptedGitlabClient {
        fn request(
            &mut self,
            method: GitlabHttpMethod,
            url: &str,
            token: &str,
            body: Option<serde_json::Value>,
        ) -> Result<GitlabHttpResponse> {
            self.requests.push(RecordedGitlabRequest {
                method,
                url: url.to_string(),
                token: token.to_string(),
                body,
            });
            self.responses
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("Missing scripted GitLab response for `{url}`"))
        }
    }

    #[derive(Debug, Clone)]
    struct RecordedForgejoRequest {
        method: ForgejoHttpMethod,
        url: String,
        token: String,
        body: Option<serde_json::Value>,
    }

    struct ScriptedForgejoClient {
        responses: VecDeque<ForgejoHttpResponse>,
        requests: Vec<RecordedForgejoRequest>,
    }

    impl ScriptedForgejoClient {
        fn new(responses: Vec<ForgejoHttpResponse>) -> Self {
            Self {
                responses: responses.into(),
                requests: Vec::new(),
            }
        }
    }

    impl ForgejoHttpClient for ScriptedForgejoClient {
        fn request(
            &mut self,
            method: ForgejoHttpMethod,
            url: &str,
            token: &str,
            body: Option<serde_json::Value>,
        ) -> Result<ForgejoHttpResponse> {
            self.requests.push(RecordedForgejoRequest {
                method,
                url: url.to_string(),
                token: token.to_string(),
                body,
            });
            self.responses
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("Missing scripted Forgejo response for `{url}`"))
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

    fn http_ok(body: &str) -> GitlabHttpResponse {
        GitlabHttpResponse {
            status: 200,
            body: body.to_string(),
        }
    }

    fn http_created(body: &str) -> GitlabHttpResponse {
        GitlabHttpResponse {
            status: 201,
            body: body.to_string(),
        }
    }

    fn forgejo_http_ok(body: &str) -> ForgejoHttpResponse {
        ForgejoHttpResponse {
            status: 200,
            body: body.to_string(),
        }
    }

    fn forgejo_http_created(body: &str) -> ForgejoHttpResponse {
        ForgejoHttpResponse {
            status: 201,
            body: body.to_string(),
        }
    }

    fn forgejo_http_error(status: u16, body: &str) -> ForgejoHttpResponse {
        ForgejoHttpResponse {
            status,
            body: body.to_string(),
        }
    }

    fn gitlab_env() -> GitlabEnv {
        GitlabEnv {
            api_v4_url: "https://gitlab.example.com/api/v4".to_string(),
            project_id: "123".to_string(),
            commit_sha: Some("merge123".to_string()),
        }
    }

    fn forgejo_env() -> ForgejoEnv {
        ForgejoEnv {
            server_url: "https://forgejo.example.com".to_string(),
            repository: "owner/demo repo".to_string(),
        }
    }

    fn log_entry(sha: &str, subject: &str, body: &str) -> String {
        format!("{sha}\u{1f}{subject}\u{1f}{body}\u{1e}")
    }

    fn write_pull_request_event(
        repo_root: &Path,
        merged: bool,
        title: &str,
        body: &str,
        merge_commit_sha: &str,
    ) -> PathBuf {
        let path = repo_root.join("event.json");
        let event = serde_json::json!({
            "pull_request": {
                "merged": merged,
                "title": title,
                "body": body,
                "merge_commit_sha": merge_commit_sha,
            }
        });
        fs::write(&path, serde_json::to_string(&event).unwrap()).unwrap();
        path
    }

    fn write_forgejo_pull_request_event(
        repo_root: &Path,
        merged: bool,
        title: &str,
        body: &str,
        merge_commit_sha: &str,
        merged_commit_id: &str,
    ) -> PathBuf {
        let path = repo_root.join("forgejo-event.json");
        let event = serde_json::json!({
            "pull_request": {
                "merged": merged,
                "title": title,
                "body": body,
                "merge_commit_sha": merge_commit_sha,
                "merged_commit_id": merged_commit_id,
            }
        });
        fs::write(&path, serde_json::to_string(&event).unwrap()).unwrap();
        path
    }

    fn args_start_with(args: &[String], expected: &[&str]) -> bool {
        args.len() >= expected.len()
            && args
                .iter()
                .zip(expected)
                .all(|(actual, expected)| actual.as_str() == *expected)
    }

    fn args_equal(args: &[String], expected: &[&str]) -> bool {
        args.len() == expected.len()
            && args
                .iter()
                .zip(expected)
                .all(|(actual, expected)| actual.as_str() == *expected)
    }

    fn call_position(
        calls: &[RecordedCall],
        predicate: impl FnMut(&RecordedCall) -> bool,
    ) -> usize {
        calls
            .iter()
            .position(predicate)
            .expect("expected recorded command call")
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
            r#"[{{"number":7,"headRefName":"brel/release/v1.3.0","body":"{}\nold body"}}]"#,
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
                    "brel/release/v1.3.0".to_string()
                ]));
        assert!(runner.calls.iter().any(|call| {
            call.program == "gh"
                && call
                    .args
                    .starts_with(&["pr".to_string(), "edit".to_string(), "7".to_string()])
        }));
    }

    #[test]
    fn stale_release_pr_branch_is_recreated() {
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
            r#"[{{"number":7,"headRefName":"brel/release/v1.2.4","body":"{}\nold body"}}]"#,
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
            ok(""),
            ok(""),
        ]);

        run_with_runner(temp_dir.path(), None, &mut runner, Some("token")).unwrap();

        assert!(runner.calls.iter().any(|call| call.program == "git"
            && call.args
                == vec![
                    "checkout".to_string(),
                    "-B".to_string(),
                    "brel/release/v1.3.0".to_string()
                ]));
        assert!(runner.calls.iter().any(|call| {
            call.program == "git"
                && call
                    .args
                    .starts_with(&["push".to_string(), "--force-with-lease".to_string()])
                && call.args.contains(&"brel/release/v1.3.0".to_string())
        }));
        let create_idx = call_position(&runner.calls, |call| {
            call.program == "gh"
                && args_start_with(&call.args, &["pr", "create"])
                && call.args.contains(&"--head".to_string())
                && call.args.contains(&"brel/release/v1.3.0".to_string())
        });
        let close_idx = call_position(&runner.calls, |call| {
            call.program == "gh" && args_start_with(&call.args, &["pr", "close", "7"])
        });
        let delete_idx = call_position(&runner.calls, |call| {
            call.program == "git"
                && args_equal(
                    &call.args,
                    &["push", "origin", "--delete", "brel/release/v1.2.4"],
                )
        });
        assert!(create_idx < close_idx);
        assert!(close_idx < delete_idx);
        assert!(
            runner.calls[close_idx]
                .args
                .iter()
                .any(|arg| arg == "--comment")
        );
        assert!(
            !runner.calls[close_idx]
                .args
                .iter()
                .any(|arg| arg == "--delete-branch")
        );
        assert!(!runner.calls.iter().any(|call| {
            call.program == "gh"
                && call
                    .args
                    .starts_with(&["pr".to_string(), "edit".to_string(), "7".to_string()])
        }));
    }

    #[test]
    fn stale_release_pr_is_not_closed_when_replacement_create_fails() {
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
            r#"[{{"number":7,"headRefName":"brel/release/v1.2.4","body":"{}\nold body"}}]"#,
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
            err_status(1, "create failed"),
        ]);

        let err = run_with_runner(temp_dir.path(), None, &mut runner, Some("token")).unwrap_err();
        let err_text = format!("{err:#}");
        assert!(err_text.contains("Failed to create release pull request."));
        assert!(
            runner
                .calls
                .iter()
                .any(|call| call.program == "gh" && args_start_with(&call.args, &["pr", "create"]))
        );
        assert!(
            !runner
                .calls
                .iter()
                .any(|call| call.program == "gh" && args_start_with(&call.args, &["pr", "close"]))
        );
        assert!(!runner.calls.iter().any(|call| {
            call.program == "git" && args_start_with(&call.args, &["push", "origin", "--delete"])
        }));
    }

    #[test]
    fn stale_release_pr_branch_delete_failure_is_tolerated() {
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
            r#"[{{"number":7,"headRefName":"brel/release/v1.2.4","body":"{}\nold body"}}]"#,
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
            ok(""),
            err_status(1, "remote ref does not exist"),
        ]);

        run_with_runner(temp_dir.path(), None, &mut runner, Some("token")).unwrap();

        assert!(
            runner
                .calls
                .iter()
                .any(|call| call.program == "gh" && args_start_with(&call.args, &["pr", "close"]))
        );
        assert!(runner.calls.iter().any(|call| {
            call.program == "git"
                && args_equal(
                    &call.args,
                    &["push", "origin", "--delete", "brel/release/v1.2.4"],
                )
        }));
    }

    #[test]
    fn stale_release_pr_close_failure_does_not_skip_remaining_stale_prs() {
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
            r#"[{{"number":7,"headRefName":"brel/release/v1.2.4","body":"{}\nstale body"}},{{"number":8,"headRefName":"brel/release/v1.2.5","body":"{}\nnewer stale body"}}]"#,
            MANAGED_RELEASE_PR_MARKER, MANAGED_RELEASE_PR_MARKER
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
            err_status(1, "close failed"),
            ok(""),
            ok(""),
        ]);

        let err = run_with_runner(temp_dir.path(), None, &mut runner, Some("token")).unwrap_err();
        let err_text = format!("{err:#}");
        assert!(err_text.contains("Failed to close 1 stale release pull request"));
        assert!(err_text.contains("brel/release/v1.2.4"));

        let first_close_idx = call_position(&runner.calls, |call| {
            call.program == "gh" && args_start_with(&call.args, &["pr", "close", "7"])
        });
        let second_close_idx = call_position(&runner.calls, |call| {
            call.program == "gh" && args_start_with(&call.args, &["pr", "close", "8"])
        });
        let second_delete_idx = call_position(&runner.calls, |call| {
            call.program == "git"
                && args_equal(
                    &call.args,
                    &["push", "origin", "--delete", "brel/release/v1.2.5"],
                )
        });

        assert!(first_close_idx < second_close_idx);
        assert!(second_close_idx < second_delete_idx);
        assert!(!runner.calls.iter().any(|call| {
            call.program == "git"
                && args_equal(
                    &call.args,
                    &["push", "origin", "--delete", "brel/release/v1.2.4"],
                )
        }));
    }

    #[test]
    fn current_release_pr_is_updated_when_stale_managed_pr_appears_first() {
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
            r#"[{{"number":7,"headRefName":"brel/release/v1.2.4","body":"{}\nstale body"}},{{"number":8,"headRefName":"brel/release/v1.3.0","body":"{}\ncurrent body"}}]"#,
            MANAGED_RELEASE_PR_MARKER, MANAGED_RELEASE_PR_MARKER
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
            ok(""),
            ok(""),
        ]);

        run_with_runner(temp_dir.path(), None, &mut runner, Some("token")).unwrap();

        let edit_idx = call_position(&runner.calls, |call| {
            call.program == "gh" && args_start_with(&call.args, &["pr", "edit", "8"])
        });
        let close_idx = call_position(&runner.calls, |call| {
            call.program == "gh" && args_start_with(&call.args, &["pr", "close", "7"])
        });
        assert!(edit_idx < close_idx);
        assert!(
            !runner
                .calls
                .iter()
                .any(|call| call.program == "gh" && args_start_with(&call.args, &["pr", "create"]))
        );
        assert!(runner.calls.iter().any(|call| {
            call.program == "git"
                && args_equal(
                    &call.args,
                    &["push", "origin", "--delete", "brel/release/v1.2.4"],
                )
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
    fn tag_manual_mode_creates_and_pushes_tag() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
[release_pr.tagging]
tag_template = "release-{version}"
"#,
        )
        .unwrap();
        let mut runner = ScriptedRunner::new(vec![ok(""), status(1), ok(""), ok("")]);

        run_tag_with_runner(
            temp_dir.path(),
            None,
            Some("release-1.2.3"),
            Some("abc123"),
            &mut runner,
            None,
        )
        .unwrap();

        assert!(runner.calls.iter().any(|call| {
            call.program == "git" && args_equal(&call.args, &["fetch", "--tags", "origin"])
        }));
        assert!(runner.calls.iter().any(|call| {
            call.program == "git" && args_equal(&call.args, &["tag", "release-1.2.3", "abc123"])
        }));
        assert!(runner.calls.iter().any(|call| {
            call.program == "git"
                && args_equal(&call.args, &["push", "origin", "refs/tags/release-1.2.3"])
        }));
    }

    #[test]
    fn tag_event_mode_skips_unmanaged_pr() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
[release_pr.tagging]
enabled = true
"#,
        )
        .unwrap();
        let event_path = write_pull_request_event(
            temp_dir.path(),
            true,
            "Release v1.2.3",
            "ordinary pull request",
            "merge123",
        );
        let mut runner = ScriptedRunner::new(vec![]);

        run_tag_with_runner(
            temp_dir.path(),
            None,
            None,
            None,
            &mut runner,
            Some(&event_path),
        )
        .unwrap();

        assert!(runner.calls.is_empty());
    }

    #[test]
    fn tag_event_mode_skips_invalid_release_title() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
[release_pr.tagging]
enabled = true
"#,
        )
        .unwrap();
        let event_path = write_pull_request_event(
            temp_dir.path(),
            true,
            "Ship v1.2.3",
            MANAGED_RELEASE_PR_MARKER,
            "merge123",
        );
        let mut runner = ScriptedRunner::new(vec![]);

        run_tag_with_runner(
            temp_dir.path(),
            None,
            None,
            None,
            &mut runner,
            Some(&event_path),
        )
        .unwrap();

        assert!(runner.calls.is_empty());
    }

    #[test]
    fn tag_event_mode_skips_existing_tag() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
[release_pr.tagging]
enabled = true
"#,
        )
        .unwrap();
        let event_path = write_pull_request_event(
            temp_dir.path(),
            true,
            "Release v1.2.3",
            MANAGED_RELEASE_PR_MARKER,
            "merge123",
        );
        let mut runner = ScriptedRunner::new(vec![ok(""), ok("refs/tags/v1.2.3\n")]);

        run_tag_with_runner(
            temp_dir.path(),
            None,
            None,
            None,
            &mut runner,
            Some(&event_path),
        )
        .unwrap();

        assert!(
            !runner
                .calls
                .iter()
                .any(|call| call.program == "git" && args_start_with(&call.args, &["tag"]))
        );
        assert!(!runner.calls.iter().any(|call| {
            call.program == "git" && args_equal(&call.args, &["push", "origin", "refs/tags/v1.2.3"])
        }));
    }

    #[test]
    fn tag_event_mode_creates_and_pushes_tag_for_managed_pr() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
[release_pr.tagging]
enabled = true
"#,
        )
        .unwrap();
        let event_path = write_pull_request_event(
            temp_dir.path(),
            true,
            "Release v1.2.3",
            MANAGED_RELEASE_PR_MARKER,
            "merge123",
        );
        let mut runner = ScriptedRunner::new(vec![ok(""), status(1), ok(""), ok("")]);

        run_tag_with_runner(
            temp_dir.path(),
            None,
            None,
            None,
            &mut runner,
            Some(&event_path),
        )
        .unwrap();

        assert!(runner.calls.iter().any(|call| {
            call.program == "git" && args_equal(&call.args, &["tag", "v1.2.3", "merge123"])
        }));
        assert!(runner.calls.iter().any(|call| {
            call.program == "git" && args_equal(&call.args, &["push", "origin", "refs/tags/v1.2.3"])
        }));
    }

    #[test]
    fn gitlab_tag_event_mode_skips_unmanaged_merge_request() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
provider = "gitlab"

[release_pr.tagging]
enabled = true
"#,
        )
        .unwrap();
        let mut runner = ScriptedRunner::new(vec![]);
        let mut gitlab = ScriptedGitlabClient::new(vec![http_ok(
            r#"[{"iid":7,"title":"Release v1.2.3","description":"ordinary merge request"}]"#,
        )]);
        let mut event_context = TagEventContext {
            github_event_path_override: None,
            forgejo_event_path_override: None,
            gitlab_token_override: Some("token"),
            gitlab_env_override: Some(gitlab_env()),
            gitlab_client: &mut gitlab,
        };

        run_tag_with_runner_with_event_context(
            temp_dir.path(),
            None,
            None,
            None,
            &mut runner,
            &mut event_context,
        )
        .unwrap();

        assert_eq!(gitlab.requests.len(), 1);
        assert_eq!(gitlab.requests[0].method, GitlabHttpMethod::Get);
        assert_eq!(
            gitlab.requests[0].url,
            "https://gitlab.example.com/api/v4/projects/123/repository/commits/merge123/merge_requests?state=merged"
        );
        assert!(runner.calls.is_empty());
    }

    #[test]
    fn gitlab_tag_event_mode_creates_and_pushes_tag_for_managed_merge_request() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
provider = "gitlab"

[release_pr.tagging]
enabled = true
"#,
        )
        .unwrap();
        let managed_mr_json = format!(
            r#"[{{"iid":7,"title":"Release v1.2.3","description":"{}\nrelease body"}}]"#,
            MANAGED_RELEASE_PR_MARKER
        );
        let mut runner = ScriptedRunner::new(vec![ok(""), status(1), ok(""), ok("")]);
        let mut gitlab = ScriptedGitlabClient::new(vec![http_ok(&managed_mr_json)]);
        let mut event_context = TagEventContext {
            github_event_path_override: None,
            forgejo_event_path_override: None,
            gitlab_token_override: Some("token"),
            gitlab_env_override: Some(gitlab_env()),
            gitlab_client: &mut gitlab,
        };

        run_tag_with_runner_with_event_context(
            temp_dir.path(),
            None,
            None,
            None,
            &mut runner,
            &mut event_context,
        )
        .unwrap();

        assert_eq!(gitlab.requests[0].token, "token");
        assert!(runner.calls.iter().any(|call| {
            call.program == "git" && args_equal(&call.args, &["tag", "v1.2.3", "merge123"])
        }));
        assert!(runner.calls.iter().any(|call| {
            call.program == "git" && args_equal(&call.args, &["push", "origin", "refs/tags/v1.2.3"])
        }));
    }

    #[test]
    fn forgejo_tag_event_mode_skips_unmanaged_pull_request() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
provider = "forgejo"

[release_pr.tagging]
enabled = true
"#,
        )
        .unwrap();
        let event_path = write_forgejo_pull_request_event(
            temp_dir.path(),
            true,
            "Release v1.2.3",
            "ordinary pull request",
            "merge123",
            "",
        );
        let mut runner = ScriptedRunner::new(vec![]);

        run_tag_with_runner(
            temp_dir.path(),
            None,
            None,
            None,
            &mut runner,
            Some(&event_path),
        )
        .unwrap();

        assert!(runner.calls.is_empty());
    }

    #[test]
    fn forgejo_tag_event_mode_skips_unmerged_pull_request() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
provider = "forgejo"

[release_pr.tagging]
enabled = true
"#,
        )
        .unwrap();
        let event_path = write_forgejo_pull_request_event(
            temp_dir.path(),
            false,
            "Release v1.2.3",
            MANAGED_RELEASE_PR_MARKER,
            "merge123",
            "",
        );
        let mut runner = ScriptedRunner::new(vec![]);

        run_tag_with_runner(
            temp_dir.path(),
            None,
            None,
            None,
            &mut runner,
            Some(&event_path),
        )
        .unwrap();

        assert!(runner.calls.is_empty());
    }

    #[test]
    fn forgejo_tag_event_mode_creates_tag_with_merged_commit_id_fallback() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
provider = "forgejo"

[release_pr.tagging]
enabled = true
"#,
        )
        .unwrap();
        let event_path = write_forgejo_pull_request_event(
            temp_dir.path(),
            true,
            "Release v1.2.3",
            MANAGED_RELEASE_PR_MARKER,
            "",
            "forgejo-merge123",
        );
        let mut runner = ScriptedRunner::new(vec![ok(""), status(1), ok(""), ok("")]);

        run_tag_with_runner(
            temp_dir.path(),
            None,
            None,
            None,
            &mut runner,
            Some(&event_path),
        )
        .unwrap();

        assert!(runner.calls.iter().any(|call| {
            call.program == "git" && args_equal(&call.args, &["tag", "v1.2.3", "forgejo-merge123"])
        }));
        assert!(runner.calls.iter().any(|call| {
            call.program == "git" && args_equal(&call.args, &["push", "origin", "refs/tags/v1.2.3"])
        }));
    }

    #[test]
    fn tag_event_mode_rejects_unsupported_provider() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
provider = "gitea"

[release_pr.tagging]
enabled = true
"#,
        )
        .unwrap();
        let mut runner = ScriptedRunner::new(vec![]);

        let err =
            run_tag_with_runner(temp_dir.path(), None, None, None, &mut runner, None).unwrap_err();

        assert!(
            err.to_string()
                .contains("event mode currently supports only `github`, `gitlab`, or `forgejo`")
        );
        assert!(runner.calls.is_empty());
    }

    #[test]
    fn release_pr_updates_cargo_lock_with_selector_and_format_override() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
[release_pr.version_updates]
"Cargo.lock" = ["package[name=brel].version"]

[release_pr.format_overrides]
"Cargo.lock" = "toml"
"#,
        )
        .unwrap();
        fs::write(
            temp_dir.path().join("Cargo.lock"),
            r#"
version = 4

[[package]]
name = "dep"
version = "0.9.0"

[[package]]
name = "brel"
version = "1.2.3"
"#,
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

        let lock_contents = fs::read_to_string(temp_dir.path().join("Cargo.lock")).unwrap();
        assert!(lock_contents.contains("name = \"dep\"\nversion = \"0.9.0\""));
        assert!(lock_contents.contains("name = \"brel\"\nversion = \"1.3.0\""));

        let add_call = runner
            .calls
            .iter()
            .find(|call| call.program == "git" && call.args.first() == Some(&"add".to_string()))
            .expect("missing git add call");
        assert!(add_call.args.contains(&"Cargo.lock".to_string()));
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
    fn gitlab_release_pr_creates_merge_request() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
provider = "gitlab"
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

        let mut runner = ScriptedRunner::new(vec![
            ok("v1.2.3\n"),
            ok(&log_entry("abc123456789", "feat: add feature", "")),
            ok(""),
            ok(""),
            status(1),
            ok(""),
            ok(""),
        ]);
        let mut gitlab = ScriptedGitlabClient::new(vec![http_ok("[]"), http_created("{}")]);

        run_with_runner_and_gitlab_client(
            temp_dir.path(),
            None,
            &mut runner,
            Some("token"),
            Some(gitlab_env()),
            &mut gitlab,
        )
        .unwrap();

        assert!(runner.calls.iter().all(|call| call.program != "gh"));
        assert_eq!(gitlab.requests.len(), 2);
        assert_eq!(gitlab.requests[0].method, GitlabHttpMethod::Get);
        assert_eq!(
            gitlab.requests[0].url,
            "https://gitlab.example.com/api/v4/projects/123/merge_requests?state=opened&target_branch=main"
        );
        assert_eq!(gitlab.requests[0].token, "token");
        assert_eq!(gitlab.requests[1].method, GitlabHttpMethod::Post);
        assert_eq!(
            gitlab.requests[1].url,
            "https://gitlab.example.com/api/v4/projects/123/merge_requests"
        );
        let body = gitlab.requests[1].body.as_ref().unwrap();
        assert_eq!(body["source_branch"], "brel/release/v1.3.0");
        assert_eq!(body["target_branch"], "main");
        assert_eq!(body["title"], "Release v1.3.0");
        assert!(
            body["description"]
                .as_str()
                .unwrap()
                .contains(MANAGED_RELEASE_PR_MARKER)
        );
    }

    #[test]
    fn gitlab_release_pr_updates_existing_merge_request() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
provider = "gitlab"
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

        let existing_mr_json = format!(
            r#"[{{"iid":7,"source_branch":"brel/release/v1.3.0","description":"{}\nold body"}}]"#,
            MANAGED_RELEASE_PR_MARKER
        );
        let mut runner = ScriptedRunner::new(vec![
            ok("v1.2.3\n"),
            ok(&log_entry("abc123456789", "feat: add feature", "")),
            ok(""),
            ok(""),
            status(1),
            ok(""),
            ok(""),
        ]);
        let mut gitlab = ScriptedGitlabClient::new(vec![http_ok(&existing_mr_json), http_ok("{}")]);

        run_with_runner_and_gitlab_client(
            temp_dir.path(),
            None,
            &mut runner,
            Some("token"),
            Some(gitlab_env()),
            &mut gitlab,
        )
        .unwrap();

        assert_eq!(gitlab.requests.len(), 2);
        assert_eq!(gitlab.requests[1].method, GitlabHttpMethod::Put);
        assert_eq!(
            gitlab.requests[1].url,
            "https://gitlab.example.com/api/v4/projects/123/merge_requests/7"
        );
        assert_eq!(
            gitlab.requests[1].body.as_ref().unwrap()["title"],
            "Release v1.3.0"
        );
        assert!(!gitlab.requests.iter().any(|request| {
            request.method == GitlabHttpMethod::Post && request.url.ends_with("/merge_requests")
        }));
    }

    #[test]
    fn gitlab_stale_release_pr_is_closed_and_branch_deleted() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
provider = "gitlab"
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

        let stale_mr_json = format!(
            r#"[{{"iid":7,"source_branch":"brel/release/v1.2.4","description":"{}\nstale body"}}]"#,
            MANAGED_RELEASE_PR_MARKER
        );
        let mut runner = ScriptedRunner::new(vec![
            ok("v1.2.3\n"),
            ok(&log_entry("abc123456789", "feat: add feature", "")),
            ok(""),
            ok(""),
            status(1),
            ok(""),
            ok(""),
            ok(""),
        ]);
        let mut gitlab = ScriptedGitlabClient::new(vec![
            http_ok(&stale_mr_json),
            http_created("{}"),
            http_created("{}"),
            http_ok("{}"),
        ]);

        run_with_runner_and_gitlab_client(
            temp_dir.path(),
            None,
            &mut runner,
            Some("token"),
            Some(gitlab_env()),
            &mut gitlab,
        )
        .unwrap();

        assert_eq!(gitlab.requests.len(), 4);
        assert_eq!(gitlab.requests[1].method, GitlabHttpMethod::Post);
        assert_eq!(
            gitlab.requests[1].url,
            "https://gitlab.example.com/api/v4/projects/123/merge_requests"
        );
        assert_eq!(gitlab.requests[2].method, GitlabHttpMethod::Post);
        assert_eq!(
            gitlab.requests[2].url,
            "https://gitlab.example.com/api/v4/projects/123/merge_requests/7/notes"
        );
        assert!(
            gitlab.requests[2].body.as_ref().unwrap()["body"]
                .as_str()
                .unwrap()
                .contains("release branch is now `brel/release/v1.3.0`")
        );
        assert_eq!(gitlab.requests[3].method, GitlabHttpMethod::Put);
        assert_eq!(
            gitlab.requests[3].body.as_ref().unwrap()["state_event"],
            "close"
        );
        assert!(runner.calls.iter().any(|call| {
            call.program == "git"
                && args_equal(
                    &call.args,
                    &["push", "origin", "--delete", "brel/release/v1.2.4"],
                )
        }));
    }

    #[test]
    fn forgejo_release_pr_creates_pull_request() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
provider = "forgejo"
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

        let mut runner = ScriptedRunner::new(vec![
            ok("v1.2.3\n"),
            ok(&log_entry("abc123456789", "feat: add feature", "")),
            ok(""),
            ok(""),
            status(1),
            ok(""),
            ok(""),
        ]);
        let mut gitlab = ScriptedGitlabClient::new(vec![]);
        let mut forgejo =
            ScriptedForgejoClient::new(vec![forgejo_http_ok("[]"), forgejo_http_created("{}")]);

        run_with_runner_and_clients(
            temp_dir.path(),
            None,
            &mut runner,
            ProviderRuntime {
                auth_token_override: Some("token"),
                gitlab_env_override: None,
                forgejo_env_override: Some(forgejo_env()),
                gitlab_client: &mut gitlab,
                forgejo_client: &mut forgejo,
            },
        )
        .unwrap();

        assert!(runner.calls.iter().all(|call| call.program != "gh"));
        assert_eq!(forgejo.requests.len(), 2);
        assert_eq!(forgejo.requests[0].method, ForgejoHttpMethod::Get);
        assert_eq!(
            forgejo.requests[0].url,
            "https://forgejo.example.com/api/v1/repos/owner/demo%20repo/pulls?state=open&base=main"
        );
        assert_eq!(forgejo.requests[0].token, "token");
        assert_eq!(forgejo.requests[1].method, ForgejoHttpMethod::Post);
        assert_eq!(
            forgejo.requests[1].url,
            "https://forgejo.example.com/api/v1/repos/owner/demo%20repo/pulls"
        );
        let body = forgejo.requests[1].body.as_ref().unwrap();
        assert_eq!(body["head"], "brel/release/v1.3.0");
        assert_eq!(body["base"], "main");
        assert_eq!(body["title"], "Release v1.3.0");
        assert!(
            body["body"]
                .as_str()
                .unwrap()
                .contains(MANAGED_RELEASE_PR_MARKER)
        );
    }

    #[test]
    fn forgejo_release_pr_updates_existing_pull_request() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
provider = "forgejo"
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
            r#"[{{"number":7,"head":{{"ref":"brel/release/v1.3.0"}},"body":"{}\nold body"}}]"#,
            MANAGED_RELEASE_PR_MARKER
        );
        let mut runner = ScriptedRunner::new(vec![
            ok("v1.2.3\n"),
            ok(&log_entry("abc123456789", "feat: add feature", "")),
            ok(""),
            ok(""),
            status(1),
            ok(""),
            ok(""),
        ]);
        let mut gitlab = ScriptedGitlabClient::new(vec![]);
        let mut forgejo = ScriptedForgejoClient::new(vec![
            forgejo_http_ok(&existing_pr_json),
            forgejo_http_ok("{}"),
        ]);

        run_with_runner_and_clients(
            temp_dir.path(),
            None,
            &mut runner,
            ProviderRuntime {
                auth_token_override: Some("token"),
                gitlab_env_override: None,
                forgejo_env_override: Some(forgejo_env()),
                gitlab_client: &mut gitlab,
                forgejo_client: &mut forgejo,
            },
        )
        .unwrap();

        assert_eq!(forgejo.requests.len(), 2);
        assert_eq!(forgejo.requests[1].method, ForgejoHttpMethod::Patch);
        assert_eq!(
            forgejo.requests[1].url,
            "https://forgejo.example.com/api/v1/repos/owner/demo%20repo/pulls/7"
        );
        assert_eq!(
            forgejo.requests[1].body.as_ref().unwrap()["title"],
            "Release v1.3.0"
        );
        assert!(!forgejo.requests.iter().any(|request| {
            request.method == ForgejoHttpMethod::Post && request.url.ends_with("/pulls")
        }));
    }

    #[test]
    fn forgejo_stale_release_pr_is_closed_and_branch_deleted() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
provider = "forgejo"
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

        let stale_pr_json = format!(
            r#"[{{"number":7,"head_branch":"brel/release/v1.2.4","body":"{}\nstale body"}}]"#,
            MANAGED_RELEASE_PR_MARKER
        );
        let mut runner = ScriptedRunner::new(vec![
            ok("v1.2.3\n"),
            ok(&log_entry("abc123456789", "feat: add feature", "")),
            ok(""),
            ok(""),
            status(1),
            ok(""),
            ok(""),
            ok(""),
        ]);
        let mut gitlab = ScriptedGitlabClient::new(vec![]);
        let mut forgejo = ScriptedForgejoClient::new(vec![
            forgejo_http_ok(&stale_pr_json),
            forgejo_http_created("{}"),
            forgejo_http_created("{}"),
            forgejo_http_ok("{}"),
        ]);

        run_with_runner_and_clients(
            temp_dir.path(),
            None,
            &mut runner,
            ProviderRuntime {
                auth_token_override: Some("token"),
                gitlab_env_override: None,
                forgejo_env_override: Some(forgejo_env()),
                gitlab_client: &mut gitlab,
                forgejo_client: &mut forgejo,
            },
        )
        .unwrap();

        assert_eq!(forgejo.requests.len(), 4);
        assert_eq!(forgejo.requests[1].method, ForgejoHttpMethod::Post);
        assert_eq!(
            forgejo.requests[1].url,
            "https://forgejo.example.com/api/v1/repos/owner/demo%20repo/pulls"
        );
        assert_eq!(forgejo.requests[2].method, ForgejoHttpMethod::Post);
        assert_eq!(
            forgejo.requests[2].url,
            "https://forgejo.example.com/api/v1/repos/owner/demo%20repo/issues/7/comments"
        );
        assert!(
            forgejo.requests[2].body.as_ref().unwrap()["body"]
                .as_str()
                .unwrap()
                .contains("release branch is now `brel/release/v1.3.0`")
        );
        assert_eq!(forgejo.requests[3].method, ForgejoHttpMethod::Patch);
        assert_eq!(
            forgejo.requests[3].body.as_ref().unwrap()["state"],
            "closed"
        );
        assert!(runner.calls.iter().any(|call| {
            call.program == "git"
                && args_equal(
                    &call.args,
                    &["push", "origin", "--delete", "brel/release/v1.2.4"],
                )
        }));
    }

    #[test]
    fn forgejo_head_ref_falls_back_to_label_when_ref_is_empty() {
        // Real Forgejo `PRBranchInfo` exposes the branch under the `label` JSON
        // key and can leave `ref` empty (or set to `refs/pull/<n>/head`) once the
        // head branch has been deleted. The managed branch name must still parse.
        let json = r#"{
            "number": 7,
            "head": {
                "label": "brel/release/v1.2.4",
                "ref": "",
                "sha": "abc123",
                "repo_id": 1
            },
            "body": "stale body"
        }"#;
        let pull_request: ForgejoPullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pull_request.head_ref_name(), "brel/release/v1.2.4");

        let forge_pr = ForgePullRequest::from(pull_request);
        assert_eq!(forge_pr.head_ref_name, "brel/release/v1.2.4");
    }

    #[test]
    fn forgejo_head_ref_prefers_ref_over_label() {
        let json = r#"{
            "number": 7,
            "head": { "label": "owner:branch", "ref": "brel/release/v1.2.4" }
        }"#;
        let pull_request: ForgejoPullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pull_request.head_ref_name(), "brel/release/v1.2.4");
    }

    #[test]
    fn missing_forgejo_token_is_an_error() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
provider = "forgejo"

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
        let mut gitlab = ScriptedGitlabClient::new(vec![]);
        let mut forgejo = ScriptedForgejoClient::new(vec![]);

        let err = run_with_runner_and_clients(
            temp_dir.path(),
            None,
            &mut runner,
            ProviderRuntime {
                auth_token_override: Some(""),
                gitlab_env_override: None,
                forgejo_env_override: Some(forgejo_env()),
                gitlab_client: &mut gitlab,
                forgejo_client: &mut forgejo,
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("Missing Forgejo auth token"));
        assert!(forgejo.requests.is_empty());
    }

    #[test]
    fn forgejo_api_failure_is_actionable() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
provider = "forgejo"

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
        let mut gitlab = ScriptedGitlabClient::new(vec![]);
        let mut forgejo =
            ScriptedForgejoClient::new(vec![forgejo_http_error(500, r#"{"message":"boom"}"#)]);

        let err = run_with_runner_and_clients(
            temp_dir.path(),
            None,
            &mut runner,
            ProviderRuntime {
                auth_token_override: Some("token"),
                gitlab_env_override: None,
                forgejo_env_override: Some(forgejo_env()),
                gitlab_client: &mut gitlab,
                forgejo_client: &mut forgejo,
            },
        )
        .unwrap_err();

        let err_text = format!("{err:#}");
        assert!(err_text.contains("Failed to list open pull requests via Forgejo API."));
        assert!(err_text.contains("Forgejo API returned HTTP 500"));
        assert!(err_text.contains("boom"));
    }

    #[test]
    fn missing_gitlab_token_is_an_error() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
provider = "gitlab"

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
        let mut gitlab = ScriptedGitlabClient::new(vec![]);

        let err = run_with_runner_and_gitlab_client(
            temp_dir.path(),
            None,
            &mut runner,
            Some(""),
            Some(gitlab_env()),
            &mut gitlab,
        )
        .unwrap_err();

        assert!(err.to_string().contains("Missing GitLab auth token"));
        assert!(gitlab.requests.is_empty());
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
