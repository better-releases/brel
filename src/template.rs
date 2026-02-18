use crate::config::Provider;
use anyhow::{Context, Result, bail};
use handlebars::{Handlebars, no_escape};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowTemplate {
    ReleasePr,
}

#[derive(Debug, Serialize)]
pub struct WorkflowRenderContext<'a> {
    pub default_branch: &'a str,
    pub release_pr_command: &'a str,
    pub next_version_command: &'a str,
    pub github_token_expr: &'a str,
    pub next_version_non_empty_expr: &'a str,
    pub next_version_output_expr: &'a str,
    pub next_version_tag_output_expr: &'a str,
    pub changelog_enabled: bool,
    pub changelog_output_file: &'a str,
    pub tagging_enabled: bool,
    pub tagging_template_prefix_shell: &'a str,
    pub tagging_template_suffix_shell: &'a str,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
pub struct ReleasePrCommitContext<'a> {
    pub sha_short: &'a str,
    pub subject: &'a str,
}

#[derive(Debug, Serialize)]
pub struct ReleasePrBodyContext<'a> {
    pub version: &'a str,
    pub tag: &'a str,
    pub base_branch: &'a str,
    pub release_branch: &'a str,
    pub commits: &'a [ReleasePrCommitContext<'a>],
}

pub const MANAGED_RELEASE_PR_MARKER: &str = "<!-- managed-by: brel -->";

const GITHUB_RELEASE_PR_TEMPLATE: &str =
    include_str!("../templates/workflows/github/release-pr.yml.hbs");
const DEFAULT_RELEASE_PR_BODY_TEMPLATE: &str = r#"<!-- managed-by: brel -->
## Release {{tag}}

Base branch: `{{base_branch}}`
Release branch: `{{release_branch}}`

### Included commits
{{#if commits}}
{{#each commits}}
- {{subject}} ({{sha_short}})
{{/each}}
{{else}}
- No commit summaries available.
{{/if}}
"#;

pub fn render_workflow(
    provider: Provider,
    template: WorkflowTemplate,
    context: &WorkflowRenderContext<'_>,
) -> Result<String> {
    match (provider, template) {
        (Provider::Github, WorkflowTemplate::ReleasePr) => {
            render_template("github-release-pr", GITHUB_RELEASE_PR_TEMPLATE, context)
        }
        (provider, _) => bail!(
            "Provider `{}` is not supported by workflow renderer in v1.",
            provider.as_str()
        ),
    }
}

pub fn render_release_pr_body(
    context: &ReleasePrBodyContext<'_>,
    template_override: Option<&str>,
) -> Result<String> {
    let template = template_override.unwrap_or(DEFAULT_RELEASE_PR_BODY_TEMPLATE);
    render_template("release-pr-body", template, context)
}

fn render_template<T: Serialize>(name: &str, template_source: &str, context: &T) -> Result<String> {
    let mut handlebars = Handlebars::new();
    handlebars.register_escape_fn(no_escape);
    handlebars
        .register_template_string(name, template_source)
        .with_context(|| format!("Failed to register template `{name}`."))?;

    handlebars
        .render(name, context)
        .with_context(|| format!("Failed to render template `{name}`."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_github_template_with_branch_and_release_command() {
        let rendered = render_workflow(
            Provider::Github,
            WorkflowTemplate::ReleasePr,
            &WorkflowRenderContext {
                default_branch: "main",
                release_pr_command: "brel release-pr --config custom.toml",
                next_version_command: "brel next-version --config custom.toml",
                github_token_expr: "${{ github.token }}",
                next_version_non_empty_expr: "${{ steps.next-version.outputs.version != '' }}",
                next_version_output_expr: "${{ steps.next-version.outputs.version }}",
                next_version_tag_output_expr: "v${{ steps.next-version.outputs.version }}",
                changelog_enabled: true,
                changelog_output_file: "CHANGELOG.md",
                tagging_enabled: false,
                tagging_template_prefix_shell: "'v'",
                tagging_template_suffix_shell: "''",
            },
        )
        .unwrap();

        assert!(rendered.contains("# managed-by: brel"));
        assert!(rendered.contains("- main"));
        assert!(rendered.contains("run: brel release-pr --config custom.toml"));
        assert!(rendered.contains("id: next-version"));
        assert!(rendered.contains("next_version=\"$(brel next-version --config custom.toml)\""));
        assert!(rendered.contains("GH_TOKEN: ${{ github.token }}"));
        assert!(rendered.contains("if: ${{ steps.next-version.outputs.version != '' }}"));
        assert!(rendered.contains(
            "args: --unreleased --tag v${{ steps.next-version.outputs.version }} --prepend CHANGELOG.md"
        ));
        assert!(!rendered.contains("--output CHANGELOG.md"));
        assert!(rendered.contains("uses: orhun/git-cliff-action@v4"));
        assert!(rendered.contains("archive asset (.tar.gz, .tar.xz, .zip)"));
        assert!(rendered.contains("tar -xaf"));
        assert!(!rendered.contains("tar -xzf"));
        assert!(!rendered.contains("Create release tag"));
        assert!(!rendered.contains("pull_request:"));
    }

    #[test]
    fn can_disable_github_changelog_step() {
        let rendered = render_workflow(
            Provider::Github,
            WorkflowTemplate::ReleasePr,
            &WorkflowRenderContext {
                default_branch: "main",
                release_pr_command: "brel release-pr",
                next_version_command: "brel next-version",
                github_token_expr: "${{ github.token }}",
                next_version_non_empty_expr: "${{ steps.next-version.outputs.version != '' }}",
                next_version_output_expr: "${{ steps.next-version.outputs.version }}",
                next_version_tag_output_expr: "v${{ steps.next-version.outputs.version }}",
                changelog_enabled: false,
                changelog_output_file: "CHANGELOG.md",
                tagging_enabled: false,
                tagging_template_prefix_shell: "'v'",
                tagging_template_suffix_shell: "''",
            },
        )
        .unwrap();

        assert!(!rendered.contains("uses: orhun/git-cliff-action@v4"));
    }

    #[test]
    fn can_enable_github_tagging_step() {
        let rendered = render_workflow(
            Provider::Github,
            WorkflowTemplate::ReleasePr,
            &WorkflowRenderContext {
                default_branch: "main",
                release_pr_command: "brel release-pr",
                next_version_command: "brel next-version",
                github_token_expr: "${{ github.token }}",
                next_version_non_empty_expr: "${{ steps.next-version.outputs.version != '' }}",
                next_version_output_expr: "${{ steps.next-version.outputs.version }}",
                next_version_tag_output_expr: "v${{ steps.next-version.outputs.version }}",
                changelog_enabled: true,
                changelog_output_file: "CHANGELOG.md",
                tagging_enabled: true,
                tagging_template_prefix_shell: "'v'",
                tagging_template_suffix_shell: "''",
            },
        )
        .unwrap();

        assert!(rendered.contains("Create release tag"));
        assert!(rendered.contains("if: github.event_name == 'pull_request'"));
        assert!(rendered.contains("types:"));
        assert!(rendered.contains("- closed"));
    }

    #[test]
    fn renders_custom_tag_template_expressions() {
        let rendered = render_workflow(
            Provider::Github,
            WorkflowTemplate::ReleasePr,
            &WorkflowRenderContext {
                default_branch: "main",
                release_pr_command: "brel release-pr",
                next_version_command: "brel next-version",
                github_token_expr: "${{ github.token }}",
                next_version_non_empty_expr: "${{ steps.next-version.outputs.version != '' }}",
                next_version_output_expr: "${{ steps.next-version.outputs.version }}",
                next_version_tag_output_expr: "release-${{ steps.next-version.outputs.version }}",
                changelog_enabled: true,
                changelog_output_file: "CHANGELOG.md",
                tagging_enabled: true,
                tagging_template_prefix_shell: "release-",
                tagging_template_suffix_shell: "''",
            },
        )
        .unwrap();

        assert!(rendered.contains(
            "args: --unreleased --tag release-${{ steps.next-version.outputs.version }} --prepend CHANGELOG.md"
        ));
        assert!(!rendered.contains("--output CHANGELOG.md"));
        assert!(rendered.contains("prefix=release-"));
        assert!(rendered.contains("suffix=''"));
    }

    #[test]
    fn renders_default_release_pr_body_template() {
        let commits = [ReleasePrCommitContext {
            sha_short: "abc1234",
            subject: "feat: add feature",
        }];
        let rendered = render_release_pr_body(
            &ReleasePrBodyContext {
                version: "1.2.3",
                tag: "v1.2.3",
                base_branch: "main",
                release_branch: "brel/release/v1.2.3",
                commits: &commits,
            },
            None,
        )
        .unwrap();

        assert!(rendered.contains(MANAGED_RELEASE_PR_MARKER));
        assert!(rendered.contains("Release v1.2.3"));
        assert!(rendered.contains("feat: add feature"));
    }
}
