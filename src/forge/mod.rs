pub(crate) mod forgejo;
pub(crate) mod github;
pub(crate) mod gitlab;

use crate::config::{Provider, ResolvedConfig};
use crate::release_pr::CommandRunner;
use anyhow::{Result, bail};
use std::fmt::Write as _;
use std::path::Path;

use forgejo::{
    forgejo_close_stale_release_pr, forgejo_create_pull_request,
    forgejo_find_managed_open_pull_requests, forgejo_update_pull_request, resolve_forgejo_token,
};
use github::{
    gh_create_pr, gh_edit_pr, github_close_stale_release_pr, github_find_managed_open_prs,
    resolve_gh_token,
};
use gitlab::{
    gitlab_close_stale_release_pr, gitlab_create_merge_request,
    gitlab_find_managed_open_merge_requests, gitlab_update_merge_request, resolve_gitlab_token,
};

pub(crate) use forgejo::{ForgejoEnv, ForgejoHttpClient, ReqwestForgejoHttpClient};
pub(crate) use gitlab::{GitlabEnv, GitlabHttpClient, ReqwestGitlabHttpClient};

pub(crate) trait Forge {
    fn find_managed_open_prs(
        &mut self,
        runner: &mut dyn CommandRunner,
        repo_root: &Path,
        config: &ResolvedConfig,
    ) -> Result<Vec<ForgePullRequest>>;

    fn create_pr(
        &mut self,
        runner: &mut dyn CommandRunner,
        repo_root: &Path,
        base_branch: &str,
        release_branch: &str,
        title: &str,
        body: &str,
    ) -> Result<()>;

    fn edit_pr(
        &mut self,
        runner: &mut dyn CommandRunner,
        repo_root: &Path,
        number: u64,
        base_branch: &str,
        title: &str,
        body: &str,
    ) -> Result<()>;

    fn close_stale_release_pr(
        &mut self,
        runner: &mut dyn CommandRunner,
        repo_root: &Path,
        pr: &ForgePullRequest,
        release_branch: &str,
    ) -> Result<()>;
}

pub(crate) fn build_forge<'client>(
    provider: Provider,
    auth_token_override: Option<&str>,
    gitlab_env_override: Option<GitlabEnv>,
    forgejo_env_override: Option<ForgejoEnv>,
    gitlab_client: &'client mut dyn GitlabHttpClient,
    forgejo_client: &'client mut dyn ForgejoHttpClient,
) -> Result<Box<dyn Forge + 'client>> {
    match provider {
        Provider::Github => {
            let token = resolve_gh_token(auth_token_override)?;
            Ok(Box::new(GithubForge {
                env: vec![("GH_TOKEN".to_string(), token)],
            }))
        }
        Provider::Gitlab => {
            let token = resolve_gitlab_token(auth_token_override)?;
            let env = match gitlab_env_override {
                Some(env) => env,
                None => GitlabEnv::from_env_for_project()?,
            };
            Ok(Box::new(GitlabForge {
                env,
                token,
                client: gitlab_client,
            }))
        }
        Provider::Forgejo => {
            let token = resolve_forgejo_token(auth_token_override)?;
            let env = match forgejo_env_override {
                Some(env) => env,
                None => ForgejoEnv::from_env_for_project()?,
            };
            Ok(Box::new(ForgejoForge {
                env,
                token,
                client: forgejo_client,
            }))
        }
        Provider::Gitea => bail!(
            "Provider `{}` is configured, but release PR creation currently supports only `github`, `gitlab`, or `forgejo`.",
            provider
        ),
    }
}

pub(crate) struct GithubForge {
    env: Vec<(String, String)>,
}

impl Forge for GithubForge {
    fn find_managed_open_prs(
        &mut self,
        runner: &mut dyn CommandRunner,
        repo_root: &Path,
        config: &ResolvedConfig,
    ) -> Result<Vec<ForgePullRequest>> {
        github_find_managed_open_prs(runner, repo_root, config, &self.env)
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
        gh_create_pr(
            runner,
            repo_root,
            base_branch,
            release_branch,
            title,
            body,
            &self.env,
        )
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
        gh_edit_pr(
            runner,
            repo_root,
            number,
            base_branch,
            title,
            body,
            &self.env,
        )
    }

    fn close_stale_release_pr(
        &mut self,
        runner: &mut dyn CommandRunner,
        repo_root: &Path,
        pr: &ForgePullRequest,
        release_branch: &str,
    ) -> Result<()> {
        github_close_stale_release_pr(runner, repo_root, pr, release_branch, &self.env)
    }
}

pub(crate) struct GitlabForge<'client> {
    env: GitlabEnv,
    token: String,
    client: &'client mut dyn GitlabHttpClient,
}

impl Forge for GitlabForge<'_> {
    fn find_managed_open_prs(
        &mut self,
        _runner: &mut dyn CommandRunner,
        _repo_root: &Path,
        config: &ResolvedConfig,
    ) -> Result<Vec<ForgePullRequest>> {
        gitlab_find_managed_open_merge_requests(self.client, &self.env, &self.token, config)
    }

    fn create_pr(
        &mut self,
        _runner: &mut dyn CommandRunner,
        _repo_root: &Path,
        base_branch: &str,
        release_branch: &str,
        title: &str,
        body: &str,
    ) -> Result<()> {
        gitlab_create_merge_request(
            self.client,
            &self.env,
            &self.token,
            base_branch,
            release_branch,
            title,
            body,
        )
    }

    fn edit_pr(
        &mut self,
        _runner: &mut dyn CommandRunner,
        _repo_root: &Path,
        number: u64,
        base_branch: &str,
        title: &str,
        body: &str,
    ) -> Result<()> {
        gitlab_update_merge_request(
            self.client,
            &self.env,
            &self.token,
            number,
            base_branch,
            title,
            body,
        )
    }

    fn close_stale_release_pr(
        &mut self,
        runner: &mut dyn CommandRunner,
        repo_root: &Path,
        pr: &ForgePullRequest,
        release_branch: &str,
    ) -> Result<()> {
        gitlab_close_stale_release_pr(
            self.client,
            &self.env,
            &self.token,
            runner,
            repo_root,
            pr,
            release_branch,
        )
    }
}

pub(crate) struct ForgejoForge<'client> {
    env: ForgejoEnv,
    token: String,
    client: &'client mut dyn ForgejoHttpClient,
}

impl Forge for ForgejoForge<'_> {
    fn find_managed_open_prs(
        &mut self,
        _runner: &mut dyn CommandRunner,
        _repo_root: &Path,
        config: &ResolvedConfig,
    ) -> Result<Vec<ForgePullRequest>> {
        forgejo_find_managed_open_pull_requests(self.client, &self.env, &self.token, config)
    }

    fn create_pr(
        &mut self,
        _runner: &mut dyn CommandRunner,
        _repo_root: &Path,
        base_branch: &str,
        release_branch: &str,
        title: &str,
        body: &str,
    ) -> Result<()> {
        forgejo_create_pull_request(
            self.client,
            &self.env,
            &self.token,
            base_branch,
            release_branch,
            title,
            body,
        )
    }

    fn edit_pr(
        &mut self,
        _runner: &mut dyn CommandRunner,
        _repo_root: &Path,
        number: u64,
        base_branch: &str,
        title: &str,
        body: &str,
    ) -> Result<()> {
        forgejo_update_pull_request(
            self.client,
            &self.env,
            &self.token,
            number,
            base_branch,
            title,
            body,
        )
    }

    fn close_stale_release_pr(
        &mut self,
        runner: &mut dyn CommandRunner,
        repo_root: &Path,
        pr: &ForgePullRequest,
        release_branch: &str,
    ) -> Result<()> {
        forgejo_close_stale_release_pr(
            self.client,
            &self.env,
            &self.token,
            runner,
            repo_root,
            pr,
            release_branch,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForgePullRequest {
    pub(crate) number: u64,
    pub(crate) head_ref_name: String,
}

pub(crate) fn url_encode(value: &str) -> String {
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
