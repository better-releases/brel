use crate::config::{ChangelogProvider, Provider};
use anyhow::{Context, Result, bail};
use handlebars::{Handlebars, no_escape};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowTemplate {
    ReleasePr,
}

#[derive(Debug)]
pub struct WorkflowRenderContext<'a> {
    pub default_branch: &'a str,
    pub release_pr_command: &'a str,
    pub next_version_command: &'a str,
    pub github_token_expr: &'a str,
    pub tagging_push_token_expr: &'a str,
    pub next_version_non_empty_expr: &'a str,
    pub next_version_output_expr: &'a str,
    pub next_version_tag_output_expr: &'a str,
    pub changelog_enabled: bool,
    pub changelog_provider: ChangelogProvider,
    pub changelog_output_file: &'a str,
    pub changelog_output_file_shell: &'a str,
    pub changelogen_package_shell: &'a str,
    pub tagging_enabled: bool,
    pub tagging_template_prefix_shell: &'a str,
    pub tagging_template_suffix_shell: &'a str,
}

#[derive(Debug, Serialize)]
struct GithubWorkflowRenderContext<'a> {
    pub default_branch: &'a str,
    pub release_pr_command: &'a str,
    pub next_version_command: &'a str,
    pub github_token_expr: &'a str,
    pub tagging_push_token_expr: &'a str,
    pub next_version_non_empty_expr: &'a str,
    pub next_version_output_expr: &'a str,
    pub next_version_tag_output_expr: &'a str,
    pub changelog_enabled: bool,
    pub changelog_provider_git_cliff: bool,
    pub changelog_provider_changelogen: bool,
    pub changelog_output_file: &'a str,
    pub changelog_output_file_shell: &'a str,
    pub changelogen_package_shell: &'a str,
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
            let github_context = github_workflow_render_context(context);
            render_template(
                "github-release-pr",
                GITHUB_RELEASE_PR_TEMPLATE,
                &github_context,
            )
        }
        (provider, _) => bail!(
            "Provider `{}` is not supported by workflow renderer in v1.",
            provider.as_str()
        ),
    }
}

fn github_workflow_render_context<'a>(
    context: &WorkflowRenderContext<'a>,
) -> GithubWorkflowRenderContext<'a> {
    GithubWorkflowRenderContext {
        default_branch: context.default_branch,
        release_pr_command: context.release_pr_command,
        next_version_command: context.next_version_command,
        github_token_expr: context.github_token_expr,
        tagging_push_token_expr: context.tagging_push_token_expr,
        next_version_non_empty_expr: context.next_version_non_empty_expr,
        next_version_output_expr: context.next_version_output_expr,
        next_version_tag_output_expr: context.next_version_tag_output_expr,
        changelog_enabled: context.changelog_enabled,
        changelog_provider_git_cliff: matches!(
            context.changelog_provider,
            ChangelogProvider::GitCliff
        ),
        changelog_provider_changelogen: matches!(
            context.changelog_provider,
            ChangelogProvider::Changelogen
        ),
        changelog_output_file: context.changelog_output_file,
        changelog_output_file_shell: context.changelog_output_file_shell,
        changelogen_package_shell: context.changelogen_package_shell,
        tagging_enabled: context.tagging_enabled,
        tagging_template_prefix_shell: context.tagging_template_prefix_shell,
        tagging_template_suffix_shell: context.tagging_template_suffix_shell,
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
                tagging_push_token_expr: "${{ secrets.BREL_TAG_PUSH_TOKEN }}",
                next_version_non_empty_expr: "${{ steps.next-version.outputs.version != '' }}",
                next_version_output_expr: "${{ steps.next-version.outputs.version }}",
                next_version_tag_output_expr: "v${{ steps.next-version.outputs.version }}",
                changelog_enabled: true,
                changelog_provider: ChangelogProvider::GitCliff,
                changelog_output_file: "CHANGELOG.md",
                changelog_output_file_shell: "CHANGELOG.md",
                changelogen_package_shell: "'changelogen@0.6.2'",
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
        assert!(!rendered.contains("uses: actions/setup-node@v6"));
        assert!(!rendered.contains("changelogen@"));
        assert!(rendered.contains("uses: better-releases/setup-brel@v1"));
        assert!(!rendered.contains("BREL_RELEASE_REPO"));
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
                tagging_push_token_expr: "${{ secrets.BREL_TAG_PUSH_TOKEN }}",
                next_version_non_empty_expr: "${{ steps.next-version.outputs.version != '' }}",
                next_version_output_expr: "${{ steps.next-version.outputs.version }}",
                next_version_tag_output_expr: "v${{ steps.next-version.outputs.version }}",
                changelog_enabled: false,
                changelog_provider: ChangelogProvider::GitCliff,
                changelog_output_file: "CHANGELOG.md",
                changelog_output_file_shell: "CHANGELOG.md",
                changelogen_package_shell: "'changelogen@0.6.2'",
                tagging_enabled: false,
                tagging_template_prefix_shell: "'v'",
                tagging_template_suffix_shell: "''",
            },
        )
        .unwrap();

        assert!(!rendered.contains("uses: orhun/git-cliff-action@v4"));
        assert!(!rendered.contains("uses: actions/setup-node@v6"));
        assert!(!rendered.contains("changelogen@"));
    }

    #[test]
    fn renders_changelogen_changelog_steps() {
        let rendered = render_workflow(
            Provider::Github,
            WorkflowTemplate::ReleasePr,
            &WorkflowRenderContext {
                default_branch: "main",
                release_pr_command: "brel release-pr",
                next_version_command: "brel next-version",
                github_token_expr: "${{ github.token }}",
                tagging_push_token_expr: "${{ secrets.BREL_TAG_PUSH_TOKEN }}",
                next_version_non_empty_expr: "${{ steps.next-version.outputs.version != '' }}",
                next_version_output_expr: "${{ steps.next-version.outputs.version }}",
                next_version_tag_output_expr: "v${{ steps.next-version.outputs.version }}",
                changelog_enabled: true,
                changelog_provider: ChangelogProvider::Changelogen,
                changelog_output_file: "docs/changelog.md",
                changelog_output_file_shell: "docs/changelog.md",
                changelogen_package_shell: "'changelogen@0.6.2'",
                tagging_enabled: false,
                tagging_template_prefix_shell: "'v'",
                tagging_template_suffix_shell: "''",
            },
        )
        .unwrap();

        assert!(rendered.contains("uses: actions/setup-node@v6"));
        assert!(rendered.contains("node-version: 24"));
        assert!(rendered.contains("package-manager-cache: false"));
        assert!(rendered.contains(
            "run: npx --yes 'changelogen@0.6.2' --to HEAD -r \"${{ steps.next-version.outputs.version }}\" --output docs/changelog.md"
        ));
        assert!(!rendered.contains("uses: orhun/git-cliff-action@v4"));
        assert!(!rendered.contains("--bump"));
        assert!(!rendered.contains("--release"));
        assert!(!rendered.contains("--commit"));
        assert!(!rendered.contains("--tag"));
        assert!(!rendered.contains("--push"));
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
                tagging_push_token_expr: "${{ secrets.BREL_TAG_PUSH_TOKEN }}",
                next_version_non_empty_expr: "${{ steps.next-version.outputs.version != '' }}",
                next_version_output_expr: "${{ steps.next-version.outputs.version }}",
                next_version_tag_output_expr: "v${{ steps.next-version.outputs.version }}",
                changelog_enabled: true,
                changelog_provider: ChangelogProvider::GitCliff,
                changelog_output_file: "CHANGELOG.md",
                changelog_output_file_shell: "CHANGELOG.md",
                changelogen_package_shell: "'changelogen@0.6.2'",
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
        assert!(rendered.contains("Validate tag push token"));
        assert!(rendered.contains("BREL_TAG_PUSH_TOKEN"));
        assert!(rendered.contains("token: ${{ secrets.BREL_TAG_PUSH_TOKEN }}"));
        assert!(
            rendered
                .contains("GITHUB_TOKEN tag pushes do not trigger downstream tag-push workflows.")
        );
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
                tagging_push_token_expr: "${{ secrets.BREL_TAG_PUSH_TOKEN }}",
                next_version_non_empty_expr: "${{ steps.next-version.outputs.version != '' }}",
                next_version_output_expr: "${{ steps.next-version.outputs.version }}",
                next_version_tag_output_expr: "release-${{ steps.next-version.outputs.version }}",
                changelog_enabled: true,
                changelog_provider: ChangelogProvider::GitCliff,
                changelog_output_file: "CHANGELOG.md",
                changelog_output_file_shell: "CHANGELOG.md",
                changelogen_package_shell: "'changelogen@0.6.2'",
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
