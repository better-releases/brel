use super::ForgePullRequest;
use crate::config::ResolvedConfig;
use crate::release_pr::{
    CommandRunner, TagRequest, TagRequestMode, git_delete_remote_branch_best_effort,
    parse_release_tag_from_title, run_checked,
};
use crate::tag_template::TagTemplate;
use crate::template::MANAGED_RELEASE_PR_MARKER;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GhPullRequest {
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

pub(crate) fn github_find_managed_open_prs(
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

pub(crate) fn gh_create_pr(
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

pub(crate) fn gh_edit_pr(
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

pub(crate) fn github_close_stale_release_pr(
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

pub(crate) fn gh_close_pr(
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

pub(crate) fn resolve_gh_token(override_token: Option<&str>) -> Result<String> {
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

#[derive(Debug, Deserialize)]
pub(crate) struct GithubPullRequestEvent {
    pull_request: Option<GithubEventPullRequest>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubEventPullRequest {
    merged: Option<bool>,
    title: Option<String>,
    body: Option<String>,
    merge_commit_sha: Option<String>,
}

pub(crate) fn read_github_pull_request_event(
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

pub(crate) fn resolve_github_tag_request(
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
