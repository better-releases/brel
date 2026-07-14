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
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitlabEnv {
    pub(crate) api_v4_url: String,
    pub(crate) project_id: String,
    pub(crate) commit_sha: Option<String>,
}

impl GitlabEnv {
    pub(crate) fn from_env_for_project() -> Result<Self> {
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

pub(crate) fn required_env(name: &str) -> Result<String> {
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
pub(crate) enum GitlabHttpMethod {
    Get,
    Post,
    Put,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitlabHttpResponse {
    pub(crate) status: u16,
    pub(crate) body: String,
}

pub(crate) trait GitlabHttpClient {
    fn request(
        &mut self,
        method: GitlabHttpMethod,
        url: &str,
        token: &str,
        body: Option<serde_json::Value>,
    ) -> Result<GitlabHttpResponse>;
}

pub(crate) struct ReqwestGitlabHttpClient {
    client: reqwest::blocking::Client,
}

pub(crate) const GITLAB_HTTP_TIMEOUT_SECS: u64 = 60;

impl ReqwestGitlabHttpClient {
    pub(crate) fn new() -> Result<Self> {
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
pub(crate) struct GitlabMergeRequest {
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

pub(crate) fn gitlab_find_managed_open_merge_requests(
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

pub(crate) fn gitlab_create_merge_request(
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

pub(crate) fn gitlab_update_merge_request(
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

pub(crate) fn gitlab_close_stale_release_pr(
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

pub(crate) fn gitlab_close_merge_request(
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

pub(crate) fn gitlab_find_merged_merge_requests_for_commit(
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

pub(crate) fn gitlab_request_json<T: DeserializeOwned>(
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

pub(crate) fn gitlab_api_url(env: &GitlabEnv, path: &str) -> String {
    format!(
        "{}/projects/{}/{}",
        env.api_v4_url.trim_end_matches('/'),
        url_encode(&env.project_id),
        path.trim_start_matches('/')
    )
}

pub(crate) fn resolve_gitlab_token(override_token: Option<&str>) -> Result<String> {
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

pub(crate) fn resolve_gitlab_tag_request(
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
            .map_or_else(
                || env.commit_sha.clone().expect("tag env includes commit sha"),
                str::to_string,
            );

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
