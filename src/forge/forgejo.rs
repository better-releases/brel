use super::{ForgePullRequest, url_encode};
use crate::config::ResolvedConfig;
use crate::release_pr::{
    CommandRunner, TagRequest, TagRequestMode, git_delete_remote_branch_best_effort,
    parse_release_tag_from_title,
};
use crate::tag_template::TagTemplate;
use crate::template::MANAGED_RELEASE_PR_MARKER;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForgejoEnv {
    pub(crate) server_url: String,
    pub(crate) repository: String,
}

impl ForgejoEnv {
    pub(crate) fn from_env_for_project() -> Result<Self> {
        Ok(Self {
            server_url: required_env_with_alias("FORGEJO_SERVER_URL", "GITHUB_SERVER_URL")?,
            repository: required_env_with_alias("FORGEJO_REPOSITORY", "GITHUB_REPOSITORY")?,
        })
    }
}

pub(crate) fn required_env_with_alias(primary: &str, alias: &str) -> Result<String> {
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
pub(crate) enum ForgejoHttpMethod {
    Get,
    Post,
    Patch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForgejoHttpResponse {
    pub(crate) status: u16,
    pub(crate) body: String,
}

pub(crate) trait ForgejoHttpClient {
    fn request(
        &mut self,
        method: ForgejoHttpMethod,
        url: &str,
        token: &str,
        body: Option<serde_json::Value>,
    ) -> Result<ForgejoHttpResponse>;
}

pub(crate) struct ReqwestForgejoHttpClient {
    client: reqwest::blocking::Client,
}

pub(crate) const FORGEJO_HTTP_TIMEOUT_SECS: u64 = 60;

impl ReqwestForgejoHttpClient {
    pub(crate) fn new() -> Result<Self> {
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
pub(crate) struct ForgejoPullRequest {
    number: u64,
    #[serde(default)]
    head_branch: String,
    head: Option<ForgejoPullRequestHead>,
    body: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ForgejoPullRequestHead {
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

pub(crate) fn forgejo_find_managed_open_pull_requests(
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

pub(crate) fn forgejo_create_pull_request(
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

pub(crate) fn forgejo_update_pull_request(
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

pub(crate) fn forgejo_close_stale_release_pr(
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

pub(crate) fn forgejo_close_pull_request(
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

pub(crate) fn forgejo_request_json<T: DeserializeOwned>(
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

pub(crate) fn forgejo_api_url(env: &ForgejoEnv, path: &str) -> String {
    format!(
        "{}/api/v1/repos/{}/{}",
        env.server_url.trim_end_matches('/'),
        repo_path_url_encode(&env.repository),
        path.trim_start_matches('/')
    )
}

pub(crate) fn repo_path_url_encode(repository: &str) -> String {
    repository
        .split('/')
        .map(url_encode)
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn resolve_forgejo_token(override_token: Option<&str>) -> Result<String> {
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

#[derive(Debug, Deserialize)]
pub(crate) struct ForgejoPullRequestEvent {
    pull_request: Option<ForgejoEventPullRequest>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ForgejoEventPullRequest {
    merged: Option<bool>,
    title: Option<String>,
    body: Option<String>,
    merge_commit_sha: Option<String>,
    merged_commit_id: Option<String>,
}

pub(crate) fn read_forgejo_pull_request_event(
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

pub(crate) fn non_empty_string(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    })
}

pub(crate) fn resolve_forgejo_tag_request(
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
