use crate::config::Provider;
use anyhow::{Context, Result, bail};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const MANAGED_MARKER: &str = "# managed-by: brel";
pub const GITHUB_WORKFLOW_DIR: &str = ".github/workflows";
pub const FORGEJO_WORKFLOW_DIR: &str = ".forgejo/workflows";

pub fn resolve_workflow_path(provider: Provider, workflow_file: &str) -> Result<PathBuf> {
    let normalized = workflow_file.trim();
    if normalized.is_empty() {
        bail!("`workflow_file` cannot be empty.");
    }
    if normalized.contains('/') || normalized.contains('\\') {
        bail!(
            "`workflow_file` must be a filename only. \
             Example: `release-pr.yml`."
        );
    }

    match provider {
        Provider::Github => Ok(PathBuf::from(GITHUB_WORKFLOW_DIR).join(normalized)),
        Provider::Gitlab => Ok(PathBuf::from(normalized)),
        Provider::Forgejo => Ok(PathBuf::from(FORGEJO_WORKFLOW_DIR).join(normalized)),
        Provider::Gitea => bail!(
            "Provider `{}` is not supported by workflow path resolver in v1.",
            provider.as_str()
        ),
    }
}

pub fn is_managed(contents: &str) -> bool {
    contents
        .lines()
        .next()
        .is_some_and(|line| line.trim() == MANAGED_MARKER)
}

pub fn detect_origin_default_branch(repo_root: &Path) -> Result<Option<String>> {
    let output = match Command::new("git")
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .current_dir(repo_root)
        .output()
    {
        Ok(output) => output,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).context("Failed to run git while checking origin/HEAD.");
        }
    };

    if !output.status.success() {
        return Ok(None);
    }

    let raw = String::from_utf8(output.stdout)
        .context("Git output for origin/HEAD was not valid UTF-8.")?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    Ok(Some(
        trimmed
            .strip_prefix("origin/")
            .unwrap_or(trimmed)
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn managed_marker_must_be_first_line() {
        assert!(is_managed("# managed-by: brel\nname: Test"));
        assert!(!is_managed("name: Test\n# managed-by: brel"));
        assert!(!is_managed(""));
    }

    #[test]
    fn workflow_file_must_be_filename_only() {
        let path = resolve_workflow_path(Provider::Github, "release-pr.yml").unwrap();
        assert_eq!(path, PathBuf::from(".github/workflows/release-pr.yml"));

        assert!(resolve_workflow_path(Provider::Github, "workflows/release-pr.yml").is_err());
        assert!(resolve_workflow_path(Provider::Github, "../release-pr.yml").is_err());
    }

    #[test]
    fn gitlab_workflow_file_lives_at_repo_root() {
        let path = resolve_workflow_path(Provider::Gitlab, ".gitlab-ci.yml").unwrap();
        assert_eq!(path, PathBuf::from(".gitlab-ci.yml"));
    }

    #[test]
    fn forgejo_workflow_file_lives_under_forgejo_workflows() {
        let path = resolve_workflow_path(Provider::Forgejo, "release-pr.yml").unwrap();
        assert_eq!(path, PathBuf::from(".forgejo/workflows/release-pr.yml"));
    }

    #[test]
    fn branch_detection_skips_when_origin_head_missing() {
        let temp_dir = tempdir().unwrap();
        let branch = detect_origin_default_branch(temp_dir.path()).unwrap();
        assert!(branch.is_none());
    }
}
