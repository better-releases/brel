use crate::cli::InitArgs;
use crate::config::{self, ConfigSource, Provider};
use crate::tag_template::{self, TagTemplate};
use crate::template::{self, WorkflowRenderContext, WorkflowTemplate};
use crate::workflow;
use anyhow::{Context, Result, bail};
use dialoguer::{Confirm, Select};
use similar::TextDiff;
use std::fs;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, value};

#[derive(Debug, Clone)]
pub struct InitOptions {
    pub config_path: Option<std::path::PathBuf>,
    pub yes: bool,
    pub dry_run: bool,
}

pub trait Interactor {
    fn confirm_overwrite(&mut self, workflow_path: &Path) -> Result<bool>;
    fn choose_branch_for_mismatch(
        &mut self,
        configured_branch: &str,
        repo_default_branch: &str,
    ) -> Result<String>;
}

struct CliInteractor;

impl Interactor for CliInteractor {
    fn confirm_overwrite(&mut self, workflow_path: &Path) -> Result<bool> {
        Confirm::new()
            .with_prompt(format!(
                "`{}` is managed by brel. Overwrite?",
                workflow_path.display()
            ))
            .default(false)
            .interact()
            .context("Failed to read overwrite confirmation.")
    }

    fn choose_branch_for_mismatch(
        &mut self,
        configured_branch: &str,
        repo_default_branch: &str,
    ) -> Result<String> {
        let options = [
            format!("Keep config branch `{configured_branch}`"),
            format!("Use repository default `{repo_default_branch}`"),
        ];
        let selection = Select::new()
            .with_prompt(
                "Config default_branch does not match repository default branch. Choose branch to use",
            )
            .items(&options)
            .default(0)
            .interact()
            .context("Failed to read branch selection.")?;

        match selection {
            0 => Ok(configured_branch.to_string()),
            1 => Ok(repo_default_branch.to_string()),
            _ => bail!("Invalid branch selection."),
        }
    }
}

pub fn run(args: InitArgs) -> Result<()> {
    let options = InitOptions {
        config_path: args.config,
        yes: args.yes,
        dry_run: args.dry_run,
    };

    let cwd = std::env::current_dir().context("Failed to determine current directory.")?;
    let mut interactor = CliInteractor;
    run_with_interactor(&cwd, &options, &mut interactor)
}

pub(crate) fn run_with_interactor(
    repo_root: &Path,
    options: &InitOptions,
    interactor: &mut dyn Interactor,
) -> Result<()> {
    let config = config::load(options.config_path.as_deref(), repo_root)?;
    for warning in &config.warnings {
        eprintln!("warning: {warning}");
    }

    if matches!(config.source, ConfigSource::Defaulted) {
        print_defaults_summary();
    } else if let Some(path) = config.source.path() {
        println!("Loaded config from `{}`", path.display());
    }

    if config.provider != Provider::Github {
        bail!(
            "Provider `{}` is configured, but `brel init` currently supports only `github`.",
            config.provider
        );
    }

    let repo_default_branch = workflow::detect_origin_default_branch(repo_root)?;
    let selected_branch = resolve_default_branch(
        &config.default_branch,
        repo_default_branch.as_deref(),
        options.yes,
        interactor,
    )?;
    persist_selected_default_branch(repo_root, options, &config.source, &selected_branch)?;

    let workflow_path = workflow::resolve_workflow_path(&config.workflow_file)?;
    let workflow_absolute_path = repo_root.join(&workflow_path);
    let release_pr_command = build_release_pr_command(options.config_path.as_deref());
    let next_version_command = build_next_version_command(options.config_path.as_deref());
    let next_version_output_expr = "${{ steps.next-version.outputs.version }}";
    let tag_template = TagTemplate::parse(&config.release_pr.tagging.tag_template)
        .context("Invalid normalized release tag template.")?;
    let next_version_tag_output_expr = tag_template.render(next_version_output_expr);
    let tagging_template_prefix_shell = tag_template::shell_escape_single(tag_template.prefix());
    let tagging_template_suffix_shell = tag_template::shell_escape_single(tag_template.suffix());
    let rendered = template::render_workflow(
        config.provider,
        WorkflowTemplate::ReleasePr,
        &WorkflowRenderContext {
            default_branch: &selected_branch.branch,
            release_pr_command: &release_pr_command,
            next_version_command: &next_version_command,
            github_token_expr: "${{ github.token }}",
            tagging_push_token_expr: "${{ secrets.BREL_TAG_PUSH_TOKEN }}",
            next_version_non_empty_expr: "${{ steps.next-version.outputs.version != '' }}",
            next_version_output_expr,
            next_version_tag_output_expr: &next_version_tag_output_expr,
            changelog_enabled: config.release_pr.changelog.enabled,
            changelog_output_file: &config.release_pr.changelog.output_file,
            tagging_enabled: config.release_pr.tagging.enabled,
            tagging_template_prefix_shell: &tagging_template_prefix_shell,
            tagging_template_suffix_shell: &tagging_template_suffix_shell,
        },
    )?;

    let existing = if workflow_absolute_path.exists() {
        Some(
            fs::read_to_string(&workflow_absolute_path)
                .with_context(|| format!("Failed to read `{}`.", workflow_path.display()))?,
        )
    } else {
        None
    };

    let action = plan_file_action(
        &workflow_path,
        existing.as_deref(),
        &rendered,
        options.yes,
        interactor,
    )?;

    match action {
        FileAction::Skip(reason) => {
            println!("Skipped `{}` ({reason}).", workflow_path.display());
        }
        FileAction::Create => {
            if options.dry_run {
                println!("Dry run: would create `{}`", workflow_path.display());
                print_diff("", &rendered);
            } else {
                if let Some(parent) = workflow_absolute_path.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!(
                            "Failed to create workflow directory `{}`.",
                            parent.display()
                        )
                    })?;
                }
                fs::write(&workflow_absolute_path, rendered)
                    .with_context(|| format!("Failed to write `{}`.", workflow_path.display()))?;
                println!("Created `{}`", workflow_path.display());
            }
        }
        FileAction::Overwrite => {
            let before = existing.as_deref().unwrap_or_default();
            if options.dry_run {
                println!("Dry run: would overwrite `{}`", workflow_path.display());
                print_diff(before, &rendered);
            } else {
                fs::write(&workflow_absolute_path, rendered)
                    .with_context(|| format!("Failed to write `{}`.", workflow_path.display()))?;
                println!("Updated `{}`", workflow_path.display());
            }
        }
    };

    if config.release_pr.tagging.enabled {
        print_tagging_token_notice();
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileAction {
    Create,
    Overwrite,
    Skip(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedDefaultBranch {
    branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigPersistencePlan {
    Create(PathBuf),
    Update(PathBuf),
}

impl ConfigPersistencePlan {
    fn path(&self) -> &Path {
        match self {
            Self::Create(path) | Self::Update(path) => path.as_path(),
        }
    }
}

fn plan_file_action(
    workflow_path: &Path,
    existing: Option<&str>,
    rendered: &str,
    yes: bool,
    interactor: &mut dyn Interactor,
) -> Result<FileAction> {
    let Some(existing_content) = existing else {
        return Ok(FileAction::Create);
    };

    if !workflow::is_managed(existing_content) {
        bail!(
            "Refusing to overwrite unmanaged workflow `{}`. \
             Move/remove the file or set `workflow_file` to a different filename.",
            workflow_path.display()
        );
    }

    if existing_content == rendered {
        return Ok(FileAction::Skip("already up to date"));
    }

    if yes {
        return Ok(FileAction::Overwrite);
    }

    if interactor.confirm_overwrite(workflow_path)? {
        Ok(FileAction::Overwrite)
    } else {
        Ok(FileAction::Skip("overwrite declined"))
    }
}

pub(crate) fn resolve_default_branch(
    configured_branch: &str,
    repo_default_branch: Option<&str>,
    yes: bool,
    interactor: &mut dyn Interactor,
) -> Result<ResolvedDefaultBranch> {
    let Some(repo_branch) = repo_default_branch else {
        return Ok(ResolvedDefaultBranch {
            branch: configured_branch.to_string(),
        });
    };

    if repo_branch == configured_branch {
        return Ok(ResolvedDefaultBranch {
            branch: configured_branch.to_string(),
        });
    }

    if yes {
        bail!(
            "Configured default_branch `{configured_branch}` does not match repository default \
             branch `{repo_branch}` (origin/HEAD). Update config or rerun without --yes \
             to choose interactively."
        );
    }

    let selected = interactor.choose_branch_for_mismatch(configured_branch, repo_branch)?;
    if selected != configured_branch && selected != repo_branch {
        bail!(
            "Selected branch `{selected}` is invalid. \
             Choose either `{configured_branch}` or `{repo_branch}`."
        );
    }

    println!("Using branch `{selected}` for generated workflow triggers.");
    Ok(ResolvedDefaultBranch { branch: selected })
}

fn persist_selected_default_branch(
    repo_root: &Path,
    options: &InitOptions,
    config_source: &ConfigSource,
    selected_branch: &ResolvedDefaultBranch,
) -> Result<()> {
    let Some(plan) = plan_config_persistence(repo_root, config_source, &selected_branch.branch)?
    else {
        return Ok(());
    };

    let display_path = display_repo_path(repo_root, plan.path());
    if options.dry_run {
        match plan {
            ConfigPersistencePlan::Create(_) => {
                println!(
                    "Dry run: would create `{display_path}` with `default_branch = \"{}\"`",
                    selected_branch.branch
                );
            }
            ConfigPersistencePlan::Update(_) => {
                println!(
                    "Dry run: would update `{display_path}` to `default_branch = \"{}\"`",
                    selected_branch.branch
                );
            }
        }
        return Ok(());
    }

    match plan {
        ConfigPersistencePlan::Create(path) => {
            write_new_default_branch_config(&path, &selected_branch.branch)?;
            let display_path = display_repo_path(repo_root, &path);
            println!(
                "Created `{display_path}` with `default_branch = \"{}\"`.",
                selected_branch.branch
            );
        }
        ConfigPersistencePlan::Update(path) => {
            update_default_branch_config(&path, &selected_branch.branch)?;
            let display_path = display_repo_path(repo_root, &path);
            println!(
                "Updated `{display_path}` with `default_branch = \"{}\"`.",
                selected_branch.branch
            );
        }
    }

    Ok(())
}

fn plan_config_persistence(
    repo_root: &Path,
    config_source: &ConfigSource,
    branch: &str,
) -> Result<Option<ConfigPersistencePlan>> {
    match config_source {
        ConfigSource::Defaulted => Ok(Some(ConfigPersistencePlan::Create(
            repo_root.join("brel.toml"),
        ))),
        ConfigSource::Explicit(path) | ConfigSource::Discovered(path) => {
            let current = fs::read_to_string(path)
                .with_context(|| format!("Failed to read config file `{}`.", path.display()))?;
            let document = current.parse::<DocumentMut>().with_context(|| {
                format!(
                    "Failed to update config file `{}` as editable TOML.",
                    path.display()
                )
            })?;
            let existing = document
                .get("default_branch")
                .and_then(|item| item.as_str());

            if existing == Some(branch) {
                Ok(None)
            } else {
                Ok(Some(ConfigPersistencePlan::Update(path.clone())))
            }
        }
    }
}

fn write_new_default_branch_config(path: &Path, branch: &str) -> Result<()> {
    let mut document = DocumentMut::new();
    document["default_branch"] = value(branch);
    fs::write(path, document.to_string())
        .with_context(|| format!("Failed to write config file `{}`.", path.display()))
}

fn update_default_branch_config(path: &Path, branch: &str) -> Result<()> {
    let current = fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file `{}`.", path.display()))?;
    let mut document = current.parse::<DocumentMut>().with_context(|| {
        format!(
            "Failed to update config file `{}` as editable TOML.",
            path.display()
        )
    })?;
    document["default_branch"] = value(branch);
    fs::write(path, document.to_string())
        .with_context(|| format!("Failed to write config file `{}`.", path.display()))
}

fn display_repo_path(repo_root: &Path, path: &Path) -> String {
    path.strip_prefix(repo_root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn print_defaults_summary() {
    println!("No config file found. Using defaults:");
    println!("  provider: github");
    println!("  default_branch: main");
    println!("  workflow_file: release-pr.yml");
}

fn print_tagging_token_notice() {
    println!(
        "Tagging is enabled. Add repository secret `BREL_TAG_PUSH_TOKEN` \
         (PAT with Contents: Read and write)."
    );
    println!(
        "Without this token, tags pushed by the workflow will not trigger \
         downstream tag-push workflows."
    );
}

fn build_release_pr_command(explicit_config_path: Option<&Path>) -> String {
    let Some(path) = explicit_config_path else {
        return "brel release-pr".to_string();
    };

    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if file_name == "brel.toml" || file_name == ".brel.toml" {
        return "brel release-pr".to_string();
    }

    format!(
        "brel release-pr --config {}",
        tag_template::shell_escape_single(path.to_string_lossy().as_ref())
    )
}

fn build_next_version_command(explicit_config_path: Option<&Path>) -> String {
    let Some(path) = explicit_config_path else {
        return "brel next-version".to_string();
    };

    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if file_name == "brel.toml" || file_name == ".brel.toml" {
        return "brel next-version".to_string();
    }

    format!(
        "brel next-version --config {}",
        tag_template::shell_escape_single(path.to_string_lossy().as_ref())
    )
}

fn print_diff(before: &str, after: &str) {
    let diff = TextDiff::from_lines(before, after);
    let unified = diff
        .unified_diff()
        .context_radius(3)
        .header("current", "proposed")
        .to_string();

    if unified.trim().is_empty() {
        println!("No textual diff.");
    } else {
        println!("{unified}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::process::Command;
    use tempfile::tempdir;

    #[derive(Default)]
    struct MockInteractor {
        overwrite_answer: bool,
        selected_branch: RefCell<Option<String>>,
        overwrite_calls: usize,
        branch_select_calls: usize,
    }

    impl Interactor for MockInteractor {
        fn confirm_overwrite(&mut self, _workflow_path: &Path) -> Result<bool> {
            self.overwrite_calls += 1;
            Ok(self.overwrite_answer)
        }

        fn choose_branch_for_mismatch(
            &mut self,
            configured_branch: &str,
            _repo_default_branch: &str,
        ) -> Result<String> {
            self.branch_select_calls += 1;
            Ok(self
                .selected_branch
                .borrow()
                .clone()
                .unwrap_or_else(|| configured_branch.to_string()))
        }
    }

    fn init_options(yes: bool, dry_run: bool) -> InitOptions {
        InitOptions {
            config_path: None,
            yes,
            dry_run,
        }
    }

    fn init_options_with_config(config_path: PathBuf, yes: bool, dry_run: bool) -> InitOptions {
        InitOptions {
            config_path: Some(config_path),
            yes,
            dry_run,
        }
    }

    fn run_git(repo_root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo_root)
            .status()
            .unwrap();
        assert!(status.success(), "git {:?} failed", args);
    }

    fn set_origin_head(repo_root: &Path, branch: &str) {
        run_git(repo_root, &["init"]);
        let target = format!("refs/remotes/origin/{branch}");
        run_git(
            repo_root,
            &["symbolic-ref", "refs/remotes/origin/HEAD", &target],
        );
    }

    #[test]
    fn no_config_creates_default_workflow() {
        let temp_dir = tempdir().unwrap();
        let mut interactor = MockInteractor::default();

        run_with_interactor(temp_dir.path(), &init_options(true, false), &mut interactor).unwrap();

        let config = fs::read_to_string(temp_dir.path().join("brel.toml")).unwrap();
        assert!(config.contains("default_branch = \"main\""));

        let workflow = temp_dir.path().join(".github/workflows/release-pr.yml");
        let content = fs::read_to_string(workflow).unwrap();
        assert!(content.contains("# managed-by: brel"));
        assert!(content.contains("- main"));
        assert!(content.contains("fetch-depth: 0"));
        assert!(content.contains("uses: better-releases/setup-brel@v1"));
        assert!(!content.contains("BREL_RELEASE_REPO"));
        assert!(content.contains("id: next-version"));
        assert!(content.contains("next_version=\"$(brel next-version)\""));
        assert!(content.contains("if: ${{ steps.next-version.outputs.version != '' }}"));
        assert!(
            content.contains("args: --unreleased --tag v${{ steps.next-version.outputs.version }}")
        );
        assert!(content.contains("--prepend CHANGELOG.md"));
        assert!(!content.contains("--output CHANGELOG.md"));
        assert!(content.contains("uses: orhun/git-cliff-action@v4"));
        assert!(!content.contains("pull_request:"));
    }

    #[test]
    fn changelog_step_can_be_disabled() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
[release_pr.changelog]
enabled = false
"#,
        )
        .unwrap();
        let mut interactor = MockInteractor::default();

        run_with_interactor(temp_dir.path(), &init_options(true, false), &mut interactor).unwrap();

        let workflow = temp_dir.path().join(".github/workflows/release-pr.yml");
        let content = fs::read_to_string(workflow).unwrap();
        assert!(!content.contains("uses: orhun/git-cliff-action@v4"));
    }

    #[test]
    fn tagging_step_can_be_enabled() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
[release_pr.tagging]
enabled = true
"#,
        )
        .unwrap();
        let mut interactor = MockInteractor::default();

        run_with_interactor(temp_dir.path(), &init_options(true, false), &mut interactor).unwrap();

        let workflow = temp_dir.path().join(".github/workflows/release-pr.yml");
        let content = fs::read_to_string(workflow).unwrap();
        assert!(content.contains("pull_request:"));
        assert!(content.contains("Create release tag"));
        assert!(content.contains("if: github.event_name == 'pull_request'"));
        assert!(content.contains("Validate tag push token"));
        assert!(content.contains("BREL_TAG_PUSH_TOKEN"));
        assert!(content.contains("token: ${{ secrets.BREL_TAG_PUSH_TOKEN }}"));
    }

    #[test]
    fn tagging_template_updates_workflow_tag_generation() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
[release_pr.tagging]
enabled = true
tag_template = "release-{version}-prod"
"#,
        )
        .unwrap();
        let mut interactor = MockInteractor::default();

        run_with_interactor(temp_dir.path(), &init_options(true, false), &mut interactor).unwrap();

        let workflow = temp_dir.path().join(".github/workflows/release-pr.yml");
        let content = fs::read_to_string(workflow).unwrap();
        assert!(content.contains(
            "args: --unreleased --tag release-${{ steps.next-version.outputs.version }}-prod"
        ));
        assert!(content.contains("--prepend CHANGELOG.md"));
        assert!(!content.contains("--output CHANGELOG.md"));
        assert!(content.contains("prefix=release-"));
        assert!(content.contains("suffix=-prod"));
    }

    #[test]
    fn managed_file_decline_keeps_existing_content() {
        let temp_dir = tempdir().unwrap();
        let workflow = temp_dir.path().join(".github/workflows/release-pr.yml");
        fs::create_dir_all(workflow.parent().unwrap()).unwrap();
        fs::write(&workflow, "# managed-by: brel\nname: old\n").unwrap();

        let mut interactor = MockInteractor {
            overwrite_answer: false,
            ..Default::default()
        };

        run_with_interactor(
            temp_dir.path(),
            &init_options(false, false),
            &mut interactor,
        )
        .unwrap();

        let content = fs::read_to_string(workflow).unwrap();
        assert_eq!(content, "# managed-by: brel\nname: old\n");
        assert_eq!(interactor.overwrite_calls, 1);
    }

    #[test]
    fn managed_file_accept_overwrites_content() {
        let temp_dir = tempdir().unwrap();
        let workflow = temp_dir.path().join(".github/workflows/release-pr.yml");
        fs::create_dir_all(workflow.parent().unwrap()).unwrap();
        fs::write(&workflow, "# managed-by: brel\nname: old\n").unwrap();

        let mut interactor = MockInteractor {
            overwrite_answer: true,
            ..Default::default()
        };

        run_with_interactor(
            temp_dir.path(),
            &init_options(false, false),
            &mut interactor,
        )
        .unwrap();

        let content = fs::read_to_string(workflow).unwrap();
        assert!(content.contains("workflow_dispatch"));
        assert_eq!(interactor.overwrite_calls, 1);
    }

    #[test]
    fn yes_flag_overwrites_without_prompt() {
        let temp_dir = tempdir().unwrap();
        let workflow = temp_dir.path().join(".github/workflows/release-pr.yml");
        fs::create_dir_all(workflow.parent().unwrap()).unwrap();
        fs::write(&workflow, "# managed-by: brel\nname: old\n").unwrap();

        let mut interactor = MockInteractor::default();
        run_with_interactor(temp_dir.path(), &init_options(true, false), &mut interactor).unwrap();
        assert_eq!(interactor.overwrite_calls, 0);
    }

    #[test]
    fn unmanaged_workflow_is_rejected() {
        let temp_dir = tempdir().unwrap();
        let workflow = temp_dir.path().join(".github/workflows/release-pr.yml");
        fs::create_dir_all(workflow.parent().unwrap()).unwrap();
        fs::write(&workflow, "name: user workflow\n").unwrap();

        let mut interactor = MockInteractor::default();
        let err = run_with_interactor(temp_dir.path(), &init_options(true, false), &mut interactor)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Refusing to overwrite unmanaged workflow")
        );
    }

    #[test]
    fn dry_run_does_not_mutate_existing_file() {
        let temp_dir = tempdir().unwrap();
        let workflow = temp_dir.path().join(".github/workflows/release-pr.yml");
        fs::create_dir_all(workflow.parent().unwrap()).unwrap();
        fs::write(&workflow, "# managed-by: brel\nname: old\n").unwrap();

        let mut interactor = MockInteractor::default();
        run_with_interactor(temp_dir.path(), &init_options(true, true), &mut interactor).unwrap();

        let content = fs::read_to_string(workflow).unwrap();
        assert_eq!(content, "# managed-by: brel\nname: old\n");
    }

    #[test]
    fn branch_choice_creates_brel_toml_when_config_is_defaulted() {
        let temp_dir = tempdir().unwrap();
        set_origin_head(temp_dir.path(), "develop");
        let mut interactor = MockInteractor {
            selected_branch: RefCell::new(Some("develop".to_string())),
            ..Default::default()
        };

        run_with_interactor(
            temp_dir.path(),
            &init_options(false, false),
            &mut interactor,
        )
        .unwrap();

        let config = fs::read_to_string(temp_dir.path().join("brel.toml")).unwrap();
        assert!(config.contains("default_branch = \"develop\""));

        let workflow =
            fs::read_to_string(temp_dir.path().join(".github/workflows/release-pr.yml")).unwrap();
        assert!(workflow.contains("- develop"));
        assert_eq!(interactor.branch_select_calls, 1);
    }

    #[test]
    fn branch_choice_updates_discovered_config() {
        let temp_dir = tempdir().unwrap();
        set_origin_head(temp_dir.path(), "develop");
        fs::write(
            temp_dir.path().join("brel.toml"),
            "# keep me\nprovider = \"github\"\ndefault_branch = \"main\"\n[release_pr.changelog]\nenabled = false\n",
        )
        .unwrap();
        let mut interactor = MockInteractor {
            selected_branch: RefCell::new(Some("develop".to_string())),
            ..Default::default()
        };

        run_with_interactor(
            temp_dir.path(),
            &init_options(false, false),
            &mut interactor,
        )
        .unwrap();

        let config = fs::read_to_string(temp_dir.path().join("brel.toml")).unwrap();
        assert!(config.contains("# keep me"));
        assert!(config.contains("provider = \"github\""));
        assert!(config.contains("default_branch = \"develop\""));
        assert!(config.contains("[release_pr.changelog]"));
        assert!(config.contains("enabled = false"));
        assert!(!config.contains("default_branch = \"main\""));
    }

    #[test]
    fn branch_choice_updates_explicit_config() {
        let temp_dir = tempdir().unwrap();
        set_origin_head(temp_dir.path(), "develop");
        let config_path = temp_dir.path().join("custom.toml");
        fs::write(
            &config_path,
            "workflow_file = \"custom.yml\"\ndefault_branch = \"main\"\n",
        )
        .unwrap();
        let mut interactor = MockInteractor {
            selected_branch: RefCell::new(Some("develop".to_string())),
            ..Default::default()
        };

        run_with_interactor(
            temp_dir.path(),
            &init_options_with_config(config_path.clone(), false, false),
            &mut interactor,
        )
        .unwrap();

        let config = fs::read_to_string(&config_path).unwrap();
        assert!(config.contains("workflow_file = \"custom.yml\""));
        assert!(config.contains("default_branch = \"develop\""));
        assert!(!config.contains("default_branch = \"main\""));
    }

    #[test]
    fn init_adds_default_branch_to_existing_config_when_it_was_implicit() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            "[release_pr.changelog]\nenabled = false\n",
        )
        .unwrap();
        let mut interactor = MockInteractor::default();

        run_with_interactor(temp_dir.path(), &init_options(true, false), &mut interactor).unwrap();

        let config = fs::read_to_string(temp_dir.path().join("brel.toml")).unwrap();
        assert!(config.contains("default_branch = \"main\""));
        assert!(config.contains("[release_pr.changelog]"));
        assert!(config.contains("enabled = false"));
    }

    #[test]
    fn dry_run_does_not_mutate_config_when_branch_choice_would_persist() {
        let temp_dir = tempdir().unwrap();
        set_origin_head(temp_dir.path(), "develop");
        let config_path = temp_dir.path().join("brel.toml");
        let original = "default_branch = \"main\"\n";
        fs::write(&config_path, original).unwrap();
        let mut interactor = MockInteractor {
            selected_branch: RefCell::new(Some("develop".to_string())),
            ..Default::default()
        };

        run_with_interactor(temp_dir.path(), &init_options(false, true), &mut interactor).unwrap();

        let config = fs::read_to_string(&config_path).unwrap();
        assert_eq!(config, original);
        assert!(
            !temp_dir
                .path()
                .join(".github/workflows/release-pr.yml")
                .exists()
        );
    }

    #[test]
    fn choosing_configured_branch_does_not_rewrite_config() {
        let temp_dir = tempdir().unwrap();
        set_origin_head(temp_dir.path(), "develop");
        let config_path = temp_dir.path().join("brel.toml");
        let original = "# keep me\ndefault_branch = \"main\"\n";
        fs::write(&config_path, original).unwrap();
        let mut interactor = MockInteractor {
            selected_branch: RefCell::new(Some("main".to_string())),
            ..Default::default()
        };

        run_with_interactor(
            temp_dir.path(),
            &init_options(false, false),
            &mut interactor,
        )
        .unwrap();

        let config = fs::read_to_string(&config_path).unwrap();
        assert_eq!(config, original);
    }

    #[test]
    fn branch_mismatch_can_be_resolved_interactively() {
        let mut interactor = MockInteractor {
            selected_branch: RefCell::new(Some("main".to_string())),
            ..Default::default()
        };

        let branch =
            resolve_default_branch("develop", Some("main"), false, &mut interactor).unwrap();
        assert_eq!(
            branch,
            ResolvedDefaultBranch {
                branch: "main".to_string(),
            }
        );
        assert_eq!(interactor.branch_select_calls, 1);
    }

    #[test]
    fn branch_mismatch_fails_with_yes() {
        let mut interactor = MockInteractor::default();
        let err =
            resolve_default_branch("develop", Some("main"), true, &mut interactor).unwrap_err();
        assert!(
            err.to_string()
                .contains("does not match repository default branch")
        );
    }
}
