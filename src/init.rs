use crate::cli::InitArgs;
use crate::config::{self, ChangelogProvider, ConfigSource, Provider, ProviderChoice};
use crate::tag_template::{self, TagTemplate};
use crate::template::{self, WorkflowRenderContext, WorkflowTemplate};
use crate::workflow;
use anyhow::{Context, Result, bail};
use dialoguer::{Confirm, Input, Select};
use similar::TextDiff;
use std::fs;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table, value};

#[derive(Debug, Clone)]
pub struct InitOptions {
    pub config_path: Option<std::path::PathBuf>,
    pub yes: bool,
    pub dry_run: bool,
}

pub trait Interactor {
    fn confirm_overwrite(&mut self, workflow_path: &Path) -> Result<bool>;
    fn choose_provider(
        &mut self,
        current_provider: Provider,
        choices: &[ProviderChoice],
    ) -> Result<Provider>;
    fn choose_branch_for_mismatch(
        &mut self,
        configured_branch: &str,
        repo_default_branch: &str,
    ) -> Result<String>;
    fn input_default_branch(&mut self, default_branch: &str) -> Result<String>;
    fn confirm_changelog(&mut self, current: bool) -> Result<bool>;
    fn choose_changelog_provider(
        &mut self,
        current_provider: ChangelogProvider,
    ) -> Result<ChangelogProvider>;
    fn confirm_tagging(&mut self, current: bool) -> Result<bool>;
    fn input_tag_template(&mut self, default_template: &str) -> Result<String>;
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

    fn choose_provider(
        &mut self,
        current_provider: Provider,
        choices: &[ProviderChoice],
    ) -> Result<Provider> {
        let options = choices
            .iter()
            .map(|choice| choice.provider.prompt_label())
            .collect::<Vec<_>>();
        let default = choices
            .iter()
            .position(|choice| choice.provider == current_provider)
            .unwrap_or(0);
        let selection = Select::new()
            .with_prompt("Forge")
            .items(&options)
            .default(default)
            .interact()
            .context("Failed to read forge selection.")?;

        choices
            .get(selection)
            .map(|choice| choice.provider)
            .context("Invalid forge selection.")
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

    fn input_default_branch(&mut self, default_branch: &str) -> Result<String> {
        Input::<String>::new()
            .with_prompt("Main branch")
            .default(default_branch.to_string())
            .interact_text()
            .context("Failed to read main branch.")
    }

    fn confirm_changelog(&mut self, current: bool) -> Result<bool> {
        Confirm::new()
            .with_prompt("Generate changelog?")
            .default(current)
            .interact()
            .context("Failed to read changelog confirmation.")
    }

    fn choose_changelog_provider(
        &mut self,
        current_provider: ChangelogProvider,
    ) -> Result<ChangelogProvider> {
        let choices = ChangelogProvider::choices();
        let options = choices
            .iter()
            .map(|choice| choice.provider.prompt_label())
            .collect::<Vec<_>>();
        let default = choices
            .iter()
            .position(|choice| choice.provider == current_provider)
            .unwrap_or(0);
        let selection = Select::new()
            .with_prompt("Changelog provider")
            .items(&options)
            .default(default)
            .interact()
            .context("Failed to read changelog provider selection.")?;

        choices
            .get(selection)
            .map(|choice| choice.provider)
            .context("Invalid changelog provider selection.")
    }

    fn confirm_tagging(&mut self, current: bool) -> Result<bool> {
        Confirm::new()
            .with_prompt("Tag release on merge?")
            .default(current)
            .interact()
            .context("Failed to read tagging confirmation.")
    }

    fn input_tag_template(&mut self, default_template: &str) -> Result<String> {
        Input::<String>::new()
            .with_prompt("Tag template")
            .default(default_template.to_string())
            .interact_text()
            .context("Failed to read tag template.")
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
    let mut config = config::load(options.config_path.as_deref(), repo_root)?;
    for warning in &config.warnings {
        eprintln!("warning: {warning}");
    }

    if matches!(config.source, ConfigSource::Defaulted) {
        print_defaults_summary();
    } else if let Some(path) = config.source.path() {
        println!("Loaded config from `{}`", path.display());
    }

    if options.yes && !config.provider.is_init_supported() {
        bail_unsupported_init_provider(config.provider)?;
    }

    let repo_default_branch = workflow::detect_origin_default_branch(repo_root)?;
    if options.yes {
        let selected_branch =
            resolve_default_branch(&config.default_branch, repo_default_branch.as_deref())?;
        config.default_branch = selected_branch.branch;
        persist_init_values(
            repo_root,
            options,
            &config.source,
            &InitPersistenceValues::default_branch(&config.default_branch),
        )?;
    } else {
        let selections =
            prompt_init_selections(&config, repo_default_branch.as_deref(), interactor)?;
        apply_init_selections(&mut config, &selections);
        persist_init_values(
            repo_root,
            options,
            &config.source,
            &InitPersistenceValues::from_selections(&selections),
        )?;
    }

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
    let changelog_output_file_shell =
        tag_template::shell_escape_single(&config.release_pr.changelog.output_file);
    let changelogen_package = format!(
        "changelogen@{}",
        config.release_pr.changelog.changelogen.version
    );
    let changelogen_package_shell = tag_template::shell_escape_single(&changelogen_package);
    let rendered = template::render_workflow(
        config.provider,
        WorkflowTemplate::ReleasePr,
        &WorkflowRenderContext {
            default_branch: &config.default_branch,
            release_pr_command: &release_pr_command,
            next_version_command: &next_version_command,
            github_token_expr: "${{ github.token }}",
            tagging_push_token_expr: "${{ secrets.BREL_TAG_PUSH_TOKEN }}",
            next_version_non_empty_expr: "${{ steps.next-version.outputs.version != '' }}",
            next_version_output_expr,
            next_version_tag_output_expr: &next_version_tag_output_expr,
            changelog_enabled: config.release_pr.changelog.enabled,
            changelog_provider: config.release_pr.changelog.provider,
            changelog_output_file: &config.release_pr.changelog.output_file,
            changelog_output_file_shell: &changelog_output_file_shell,
            changelogen_package_shell: &changelogen_package_shell,
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
struct InitSelections {
    provider: Provider,
    default_branch: String,
    changelog_enabled: bool,
    changelog_provider: ChangelogProvider,
    tagging_enabled: bool,
    tag_template: String,
}

#[derive(Debug, Clone, Copy)]
struct InitPersistenceValues<'a> {
    provider: Option<Provider>,
    default_branch: Option<&'a str>,
    changelog_enabled: Option<bool>,
    changelog_provider: Option<ChangelogProvider>,
    tagging_enabled: Option<bool>,
    tag_template: Option<&'a str>,
}

impl<'a> InitPersistenceValues<'a> {
    fn default_branch(default_branch: &'a str) -> Self {
        Self {
            provider: None,
            default_branch: Some(default_branch),
            changelog_enabled: None,
            changelog_provider: None,
            tagging_enabled: None,
            tag_template: None,
        }
    }

    fn from_selections(selections: &'a InitSelections) -> Self {
        Self {
            provider: Some(selections.provider),
            default_branch: Some(&selections.default_branch),
            changelog_enabled: Some(selections.changelog_enabled),
            changelog_provider: selections
                .changelog_enabled
                .then_some(selections.changelog_provider),
            tagging_enabled: Some(selections.tagging_enabled),
            tag_template: selections
                .tagging_enabled
                .then_some(selections.tag_template.as_str()),
        }
    }
}

fn prompt_init_selections(
    config: &config::ResolvedConfig,
    repo_default_branch: Option<&str>,
    interactor: &mut dyn Interactor,
) -> Result<InitSelections> {
    let provider_choices = Provider::choices()
        .iter()
        .copied()
        .filter(|choice| choice.init_supported)
        .collect::<Vec<_>>();
    let provider = interactor.choose_provider(config.provider, &provider_choices)?;

    let default_branch =
        resolve_interactive_default_branch(config, repo_default_branch, interactor)?;

    let changelog_enabled = interactor.confirm_changelog(config.release_pr.changelog.enabled)?;
    let changelog_provider = if changelog_enabled {
        interactor.choose_changelog_provider(config.release_pr.changelog.provider)?
    } else {
        config.release_pr.changelog.provider
    };

    let tagging_enabled = interactor.confirm_tagging(config.release_pr.tagging.enabled)?;
    let tag_template = if tagging_enabled {
        tag_template::normalize_tag_template(
            &interactor.input_tag_template(&config.release_pr.tagging.tag_template)?,
        )
        .context("Invalid `release_pr.tagging.tag_template`.")?
    } else {
        config.release_pr.tagging.tag_template.clone()
    };

    Ok(InitSelections {
        provider,
        default_branch,
        changelog_enabled,
        changelog_provider,
        tagging_enabled,
        tag_template,
    })
}

fn resolve_interactive_default_branch(
    config: &config::ResolvedConfig,
    repo_default_branch: Option<&str>,
    interactor: &mut dyn Interactor,
) -> Result<String> {
    if let Some(repo_branch) = repo_default_branch
        && repo_branch != config.default_branch
        && !matches!(config.source, ConfigSource::Defaulted)
    {
        let selected =
            interactor.choose_branch_for_mismatch(&config.default_branch, repo_branch)?;
        if selected != config.default_branch && selected != repo_branch {
            bail!(
                "Selected branch `{selected}` is invalid. \
                 Choose either `{}` or `{repo_branch}`.",
                config.default_branch
            );
        }

        Ok(selected)
    } else {
        let default_branch = if matches!(config.source, ConfigSource::Defaulted) {
            repo_default_branch.unwrap_or(&config.default_branch)
        } else {
            &config.default_branch
        };
        normalize_default_branch(&interactor.input_default_branch(default_branch)?)
    }
}

fn normalize_default_branch(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("Main branch cannot be empty.");
    }

    Ok(trimmed.to_string())
}

fn apply_init_selections(config: &mut config::ResolvedConfig, selections: &InitSelections) {
    config.provider = selections.provider;
    config.default_branch = selections.default_branch.clone();
    config.release_pr.changelog.enabled = selections.changelog_enabled;
    config.release_pr.changelog.provider = selections.changelog_provider;
    config.release_pr.tagging.enabled = selections.tagging_enabled;
    config.release_pr.tagging.tag_template = selections.tag_template.clone();
}

fn bail_unsupported_init_provider(provider: Provider) -> Result<()> {
    bail!(
        "Provider `{}` is configured, but `brel init` currently supports only `github`.",
        provider
    )
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

    bail!(
        "Configured default_branch `{configured_branch}` does not match repository default \
         branch `{repo_branch}` (origin/HEAD). Update config or rerun without --yes \
         to choose interactively."
    );
}

fn persist_init_values(
    repo_root: &Path,
    options: &InitOptions,
    config_source: &ConfigSource,
    values: &InitPersistenceValues<'_>,
) -> Result<()> {
    let Some(plan) = plan_config_persistence(repo_root, config_source, values)? else {
        return Ok(());
    };

    let display_path = display_repo_path(repo_root, plan.path());
    if options.dry_run {
        match plan {
            ConfigPersistencePlan::Create(_) => {
                println!("Dry run: would create `{display_path}` with init config selections");
            }
            ConfigPersistencePlan::Update(_) => {
                println!("Dry run: would update `{display_path}` with init config selections");
            }
        }
        return Ok(());
    }

    match plan {
        ConfigPersistencePlan::Create(path) => {
            write_new_init_config(&path, values)?;
            let display_path = display_repo_path(repo_root, &path);
            println!("Created `{display_path}` with init config selections.");
        }
        ConfigPersistencePlan::Update(path) => {
            update_init_config(&path, values)?;
            let display_path = display_repo_path(repo_root, &path);
            println!("Updated `{display_path}` with init config selections.");
        }
    }

    Ok(())
}

fn plan_config_persistence(
    repo_root: &Path,
    config_source: &ConfigSource,
    values: &InitPersistenceValues<'_>,
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
            let mut proposed = document.clone();
            apply_init_values_to_document(&mut proposed, values);

            if proposed.to_string() == current {
                Ok(None)
            } else {
                Ok(Some(ConfigPersistencePlan::Update(path.clone())))
            }
        }
    }
}

fn write_new_init_config(path: &Path, values: &InitPersistenceValues<'_>) -> Result<()> {
    let mut document = DocumentMut::new();
    apply_init_values_to_document(&mut document, values);
    fs::write(path, document.to_string())
        .with_context(|| format!("Failed to write config file `{}`.", path.display()))
}

fn update_init_config(path: &Path, values: &InitPersistenceValues<'_>) -> Result<()> {
    let current = fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file `{}`.", path.display()))?;
    let mut document = current.parse::<DocumentMut>().with_context(|| {
        format!(
            "Failed to update config file `{}` as editable TOML.",
            path.display()
        )
    })?;
    apply_init_values_to_document(&mut document, values);
    fs::write(path, document.to_string())
        .with_context(|| format!("Failed to write config file `{}`.", path.display()))
}

fn apply_init_values_to_document(document: &mut DocumentMut, values: &InitPersistenceValues<'_>) {
    if let Some(provider) = values.provider {
        document["provider"] = value(provider.as_str());
    }

    if let Some(default_branch) = values.default_branch {
        document["default_branch"] = value(default_branch);
    }

    if values.changelog_enabled.is_some() || values.changelog_provider.is_some() {
        let release_pr = ensure_init_table(document.as_table_mut(), "release_pr");
        let changelog = ensure_init_table(release_pr, "changelog");

        if let Some(enabled) = values.changelog_enabled {
            changelog["enabled"] = value(enabled);
        }

        if let Some(provider) = values.changelog_provider {
            changelog["provider"] = value(provider.as_str());
        }
    }

    if values.tagging_enabled.is_some() || values.tag_template.is_some() {
        let release_pr = ensure_init_table(document.as_table_mut(), "release_pr");
        let tagging = ensure_init_table(release_pr, "tagging");

        if let Some(enabled) = values.tagging_enabled {
            tagging["enabled"] = value(enabled);
        }

        if let Some(tag_template) = values.tag_template {
            tagging["tag_template"] = value(tag_template);
        }
    }
}

fn ensure_init_table<'a>(table: &'a mut Table, key: &str) -> &'a mut Table {
    let item = table[key].or_insert(Item::Table(Table::new()));
    if item.as_table().is_none() {
        let table = std::mem::take(item)
            .into_table()
            .expect("init config table key must be table-like");
        *item = Item::Table(table);
    }

    item.as_table_mut()
        .expect("init config table key must be a table")
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
        selected_provider: RefCell<Option<Provider>>,
        selected_branch_for_mismatch: RefCell<Option<String>>,
        default_branch_answer: RefCell<Option<String>>,
        changelog_enabled_answer: RefCell<Option<bool>>,
        changelog_provider_answer: RefCell<Option<ChangelogProvider>>,
        tagging_enabled_answer: RefCell<Option<bool>>,
        tag_template_answer: RefCell<Option<String>>,
        provider_choices: RefCell<Vec<ProviderChoice>>,
        overwrite_calls: usize,
        provider_select_calls: usize,
        branch_mismatch_select_calls: usize,
        default_branch_input_calls: usize,
        changelog_confirm_calls: usize,
        changelog_provider_select_calls: usize,
        tagging_confirm_calls: usize,
        tag_template_input_calls: usize,
    }

    impl Interactor for MockInteractor {
        fn confirm_overwrite(&mut self, _workflow_path: &Path) -> Result<bool> {
            self.overwrite_calls += 1;
            Ok(self.overwrite_answer)
        }

        fn choose_provider(
            &mut self,
            current_provider: Provider,
            choices: &[ProviderChoice],
        ) -> Result<Provider> {
            self.provider_select_calls += 1;
            self.provider_choices.replace(choices.to_vec());
            let default_provider = choices
                .iter()
                .find(|choice| choice.provider == current_provider)
                .or_else(|| choices.first())
                .map(|choice| choice.provider)
                .context("mock provider choices cannot be empty")?;
            Ok(self
                .selected_provider
                .borrow()
                .as_ref()
                .copied()
                .unwrap_or(default_provider))
        }

        fn choose_branch_for_mismatch(
            &mut self,
            configured_branch: &str,
            _repo_default_branch: &str,
        ) -> Result<String> {
            self.branch_mismatch_select_calls += 1;
            Ok(self
                .selected_branch_for_mismatch
                .borrow()
                .clone()
                .unwrap_or_else(|| configured_branch.to_string()))
        }

        fn input_default_branch(&mut self, default_branch: &str) -> Result<String> {
            self.default_branch_input_calls += 1;
            Ok(self
                .default_branch_answer
                .borrow()
                .clone()
                .unwrap_or_else(|| default_branch.to_string()))
        }

        fn confirm_changelog(&mut self, current: bool) -> Result<bool> {
            self.changelog_confirm_calls += 1;
            Ok((*self.changelog_enabled_answer.borrow()).unwrap_or(current))
        }

        fn choose_changelog_provider(
            &mut self,
            current_provider: ChangelogProvider,
        ) -> Result<ChangelogProvider> {
            self.changelog_provider_select_calls += 1;
            Ok(self
                .changelog_provider_answer
                .borrow()
                .as_ref()
                .copied()
                .unwrap_or(current_provider))
        }

        fn confirm_tagging(&mut self, current: bool) -> Result<bool> {
            self.tagging_confirm_calls += 1;
            Ok((*self.tagging_enabled_answer.borrow()).unwrap_or(current))
        }

        fn input_tag_template(&mut self, default_template: &str) -> Result<String> {
            self.tag_template_input_calls += 1;
            Ok(self
                .tag_template_answer
                .borrow()
                .clone()
                .unwrap_or_else(|| default_template.to_string()))
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

    fn read_config_toml(path: &Path) -> toml::Value {
        fs::read_to_string(path)
            .unwrap()
            .parse::<toml::Value>()
            .unwrap()
    }

    fn full_init_persistence_values<'a>(
        default_branch: &'a str,
        tag_template: &'a str,
    ) -> InitPersistenceValues<'a> {
        InitPersistenceValues {
            provider: Some(Provider::Github),
            default_branch: Some(default_branch),
            changelog_enabled: Some(true),
            changelog_provider: Some(ChangelogProvider::GitCliff),
            tagging_enabled: Some(true),
            tag_template: Some(tag_template),
        }
    }

    #[test]
    fn init_values_create_nested_tables_in_fresh_document() {
        let mut document = DocumentMut::new();

        apply_init_values_to_document(
            &mut document,
            &full_init_persistence_values("main", "release-{version}"),
        );

        let content = document.to_string();
        assert!(content.contains("[release_pr.changelog]"));
        assert!(content.contains("[release_pr.tagging]"));
        let config = content.parse::<toml::Value>().unwrap();
        assert_eq!(config["provider"].as_str(), Some("github"));
        assert_eq!(config["default_branch"].as_str(), Some("main"));
        assert_eq!(
            config["release_pr"]["changelog"]["enabled"].as_bool(),
            Some(true)
        );
        assert_eq!(
            config["release_pr"]["changelog"]["provider"].as_str(),
            Some("git-cliff")
        );
        assert_eq!(
            config["release_pr"]["tagging"]["enabled"].as_bool(),
            Some(true)
        );
        assert_eq!(
            config["release_pr"]["tagging"]["tag_template"].as_str(),
            Some("release-{version}")
        );
    }

    #[test]
    fn init_values_create_nested_tables_in_minimal_existing_document() {
        let mut document = "default_branch = \"main\"\n"
            .parse::<DocumentMut>()
            .unwrap();

        apply_init_values_to_document(
            &mut document,
            &InitPersistenceValues {
                provider: None,
                default_branch: Some("develop"),
                changelog_enabled: Some(false),
                changelog_provider: None,
                tagging_enabled: Some(false),
                tag_template: None,
            },
        );

        let content = document.to_string();
        assert!(content.contains("[release_pr.changelog]"));
        assert!(content.contains("[release_pr.tagging]"));
        let config = content.parse::<toml::Value>().unwrap();
        assert_eq!(config["default_branch"].as_str(), Some("develop"));
        assert_eq!(
            config["release_pr"]["changelog"]["enabled"].as_bool(),
            Some(false)
        );
        assert_eq!(
            config["release_pr"]["tagging"]["enabled"].as_bool(),
            Some(false)
        );
    }

    #[test]
    fn init_values_preserve_partial_release_pr_tables() {
        let mut document = r#"
[release_pr]
release_branch_pattern = "brel/release/v{{version}}"
changelog = { output_file = "docs/CHANGELOG.md" }
"#
        .parse::<DocumentMut>()
        .unwrap();

        apply_init_values_to_document(
            &mut document,
            &InitPersistenceValues {
                provider: None,
                default_branch: None,
                changelog_enabled: Some(true),
                changelog_provider: Some(ChangelogProvider::Changelogen),
                tagging_enabled: Some(true),
                tag_template: Some("v{version}"),
            },
        );

        let config = document.to_string().parse::<toml::Value>().unwrap();
        assert_eq!(
            config["release_pr"]["release_branch_pattern"].as_str(),
            Some("brel/release/v{{version}}")
        );
        assert_eq!(
            config["release_pr"]["changelog"]["output_file"].as_str(),
            Some("docs/CHANGELOG.md")
        );
        assert_eq!(
            config["release_pr"]["changelog"]["enabled"].as_bool(),
            Some(true)
        );
        assert_eq!(
            config["release_pr"]["changelog"]["provider"].as_str(),
            Some("changelogen")
        );
        assert_eq!(
            config["release_pr"]["tagging"]["enabled"].as_bool(),
            Some(true)
        );
        assert_eq!(
            config["release_pr"]["tagging"]["tag_template"].as_str(),
            Some("v{version}")
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
        assert!(!content.contains("uses: actions/setup-node@v6"));
        assert!(!content.contains("changelogen@"));
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
        assert!(!content.contains("uses: actions/setup-node@v6"));
        assert!(!content.contains("changelogen@"));
    }

    #[test]
    fn changelogen_changelog_provider_updates_workflow() {
        let temp_dir = tempdir().unwrap();
        fs::write(
            temp_dir.path().join("brel.toml"),
            r#"
[release_pr.changelog]
provider = "changelogen"
output_file = "docs/changelog.md"
"#,
        )
        .unwrap();
        let mut interactor = MockInteractor::default();

        run_with_interactor(temp_dir.path(), &init_options(true, false), &mut interactor).unwrap();

        let workflow = temp_dir.path().join(".github/workflows/release-pr.yml");
        let content = fs::read_to_string(workflow).unwrap();
        assert!(content.contains("uses: actions/setup-node@v6"));
        assert!(content.contains("node-version: 24"));
        assert!(content.contains("package-manager-cache: false"));
        assert!(content.contains(
            "run: npx --yes 'changelogen@0.6.2' --to HEAD -r \"${{ steps.next-version.outputs.version }}\" --output docs/changelog.md"
        ));
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
    fn interactive_init_can_disable_changelog() {
        let temp_dir = tempdir().unwrap();
        let mut interactor = MockInteractor {
            changelog_enabled_answer: RefCell::new(Some(false)),
            ..Default::default()
        };

        run_with_interactor(
            temp_dir.path(),
            &init_options(false, false),
            &mut interactor,
        )
        .unwrap();

        let config = read_config_toml(&temp_dir.path().join("brel.toml"));
        assert_eq!(
            config["release_pr"]["changelog"]["enabled"].as_bool(),
            Some(false)
        );
        assert_eq!(interactor.changelog_provider_select_calls, 0);

        let workflow =
            fs::read_to_string(temp_dir.path().join(".github/workflows/release-pr.yml")).unwrap();
        assert!(!workflow.contains("uses: orhun/git-cliff-action@v4"));
        assert!(!workflow.contains("uses: actions/setup-node@v6"));
    }

    #[test]
    fn interactive_init_can_choose_changelogen() {
        let temp_dir = tempdir().unwrap();
        let mut interactor = MockInteractor {
            changelog_provider_answer: RefCell::new(Some(ChangelogProvider::Changelogen)),
            ..Default::default()
        };

        run_with_interactor(
            temp_dir.path(),
            &init_options(false, false),
            &mut interactor,
        )
        .unwrap();

        let config = read_config_toml(&temp_dir.path().join("brel.toml"));
        assert_eq!(
            config["release_pr"]["changelog"]["provider"].as_str(),
            Some("changelogen")
        );
        assert_eq!(interactor.changelog_provider_select_calls, 1);

        let workflow =
            fs::read_to_string(temp_dir.path().join(".github/workflows/release-pr.yml")).unwrap();
        assert!(workflow.contains("uses: actions/setup-node@v6"));
        assert!(workflow.contains("changelogen@0.6.2"));
        assert!(!workflow.contains("uses: orhun/git-cliff-action@v4"));
    }

    #[test]
    fn interactive_init_can_enable_tagging_with_template() {
        let temp_dir = tempdir().unwrap();
        let mut interactor = MockInteractor {
            tagging_enabled_answer: RefCell::new(Some(true)),
            tag_template_answer: RefCell::new(Some("release-{{version}}".to_string())),
            ..Default::default()
        };

        run_with_interactor(
            temp_dir.path(),
            &init_options(false, false),
            &mut interactor,
        )
        .unwrap();

        let config = read_config_toml(&temp_dir.path().join("brel.toml"));
        assert_eq!(
            config["release_pr"]["tagging"]["enabled"].as_bool(),
            Some(true)
        );
        assert_eq!(
            config["release_pr"]["tagging"]["tag_template"].as_str(),
            Some("release-{version}")
        );
        assert_eq!(interactor.tag_template_input_calls, 1);

        let workflow =
            fs::read_to_string(temp_dir.path().join(".github/workflows/release-pr.yml")).unwrap();
        assert!(workflow.contains("pull_request:"));
        assert!(workflow.contains(
            "args: --unreleased --tag release-${{ steps.next-version.outputs.version }}"
        ));
    }

    #[test]
    fn interactive_init_does_not_ask_tag_template_when_tagging_disabled() {
        let temp_dir = tempdir().unwrap();
        let mut interactor = MockInteractor {
            tagging_enabled_answer: RefCell::new(Some(false)),
            tag_template_answer: RefCell::new(Some("release-{version}".to_string())),
            ..Default::default()
        };

        run_with_interactor(
            temp_dir.path(),
            &init_options(false, false),
            &mut interactor,
        )
        .unwrap();

        let config = read_config_toml(&temp_dir.path().join("brel.toml"));
        assert_eq!(
            config["release_pr"]["tagging"]["enabled"].as_bool(),
            Some(false)
        );
        assert!(
            config["release_pr"]["tagging"]
                .as_table()
                .unwrap()
                .get("tag_template")
                .is_none()
        );
        assert_eq!(interactor.tag_template_input_calls, 0);
    }

    #[test]
    fn interactive_init_rewrites_unsupported_configured_provider_to_github() {
        let temp_dir = tempdir().unwrap();
        fs::write(temp_dir.path().join("brel.toml"), "provider = \"gitlab\"\n").unwrap();
        let mut interactor = MockInteractor::default();

        run_with_interactor(
            temp_dir.path(),
            &init_options(false, false),
            &mut interactor,
        )
        .unwrap();

        let config = fs::read_to_string(temp_dir.path().join("brel.toml")).unwrap();
        assert!(config.contains("provider = \"github\""));
        assert!(!config.contains("provider = \"gitlab\""));
        assert_eq!(
            interactor
                .provider_choices
                .borrow()
                .iter()
                .map(|choice| choice.provider)
                .collect::<Vec<_>>(),
            vec![Provider::Github]
        );
    }

    #[test]
    fn yes_init_rejects_unsupported_configured_provider() {
        let temp_dir = tempdir().unwrap();
        fs::write(temp_dir.path().join("brel.toml"), "provider = \"gitlab\"\n").unwrap();
        let mut interactor = MockInteractor::default();

        let err = run_with_interactor(temp_dir.path(), &init_options(true, false), &mut interactor)
            .unwrap_err();

        assert!(err.to_string().contains("currently supports only `github`"));
        assert_eq!(interactor.provider_select_calls, 0);
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
            default_branch_answer: RefCell::new(Some("develop".to_string())),
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
        assert_eq!(interactor.default_branch_input_calls, 1);
        assert_eq!(interactor.branch_mismatch_select_calls, 0);
        assert_eq!(interactor.provider_select_calls, 1);
        assert_eq!(
            interactor
                .provider_choices
                .borrow()
                .iter()
                .map(|choice| choice.provider)
                .collect::<Vec<_>>(),
            vec![Provider::Github]
        );
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
            selected_branch_for_mismatch: RefCell::new(Some("develop".to_string())),
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
        assert_eq!(interactor.branch_mismatch_select_calls, 1);
        assert_eq!(interactor.default_branch_input_calls, 0);
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
            selected_branch_for_mismatch: RefCell::new(Some("develop".to_string())),
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
        assert_eq!(interactor.branch_mismatch_select_calls, 1);
        assert_eq!(interactor.default_branch_input_calls, 0);
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
            selected_branch_for_mismatch: RefCell::new(Some("develop".to_string())),
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
        assert_eq!(interactor.branch_mismatch_select_calls, 1);
        assert_eq!(interactor.default_branch_input_calls, 0);
    }

    #[test]
    fn interactive_init_persists_prompt_answers() {
        let temp_dir = tempdir().unwrap();
        set_origin_head(temp_dir.path(), "develop");
        let config_path = temp_dir.path().join("brel.toml");
        let original = "# keep me\ndefault_branch = \"main\"\n";
        fs::write(&config_path, original).unwrap();
        let mut interactor = MockInteractor::default();

        run_with_interactor(
            temp_dir.path(),
            &init_options(false, false),
            &mut interactor,
        )
        .unwrap();

        let config_content = fs::read_to_string(&config_path).unwrap();
        assert!(config_content.contains("# keep me"));
        let config = config_content.parse::<toml::Value>().unwrap();
        assert_eq!(config["provider"].as_str(), Some("github"));
        assert_eq!(config["default_branch"].as_str(), Some("main"));
        assert_eq!(
            config["release_pr"]["changelog"]["enabled"].as_bool(),
            Some(true)
        );
        assert_eq!(
            config["release_pr"]["changelog"]["provider"].as_str(),
            Some("git-cliff")
        );
        assert_eq!(
            config["release_pr"]["tagging"]["enabled"].as_bool(),
            Some(false)
        );
        assert_eq!(interactor.branch_mismatch_select_calls, 1);
        assert_eq!(interactor.default_branch_input_calls, 0);
    }

    #[test]
    fn defaulted_config_prefers_detected_repo_branch_prompt_default() {
        let temp_dir = tempdir().unwrap();
        set_origin_head(temp_dir.path(), "develop");
        let mut interactor = MockInteractor {
            default_branch_answer: RefCell::new(Some("develop".to_string())),
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
        assert_eq!(interactor.default_branch_input_calls, 1);
        assert_eq!(interactor.branch_mismatch_select_calls, 0);
    }

    #[test]
    fn branch_mismatch_fails_with_yes() {
        let err = resolve_default_branch("develop", Some("main")).unwrap_err();
        assert!(
            err.to_string()
                .contains("does not match repository default branch")
        );
    }
}
