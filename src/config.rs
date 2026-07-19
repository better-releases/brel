use crate::tag_template;
use crate::version_selector;
use anyhow::{Context, Result, bail};
use semver::Version;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

pub const DEFAULT_BRANCH: &str = "main";
pub const DEFAULT_WORKFLOW_FILE: &str = "release-pr.yml";
pub const DEFAULT_GITLAB_WORKFLOW_FILE: &str = ".gitlab-ci.yml";
pub const DEFAULT_RELEASE_BRANCH_PATTERN: &str = "brel/release/v{{version}}";
pub const DEFAULT_COMMIT_AUTHOR_NAME: &str = "brel[bot]";
pub const DEFAULT_COMMIT_AUTHOR_EMAIL: &str = "brel[bot]@users.noreply.github.com";
pub const DEFAULT_CHANGELOG_OUTPUT_FILE: &str = "CHANGELOG.md";
pub const DEFAULT_CHANGELOGEN_VERSION: &str = "0.6.2";
pub const DEFAULT_TAGGING_ENABLED: bool = false;
pub const DEFAULT_PREVIEW_COMMENT_ENABLED: bool = false;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Github,
    Gitlab,
    Forgejo,
    Gitea,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderChoice {
    pub provider: Provider,
    pub config_value: &'static str,
    pub prompt_label: &'static str,
    pub init_supported: bool,
}

const PROVIDER_CHOICES: &[ProviderChoice] = &[
    ProviderChoice {
        provider: Provider::Github,
        config_value: "github",
        prompt_label: "GitHub",
        init_supported: true,
    },
    ProviderChoice {
        provider: Provider::Gitlab,
        config_value: "gitlab",
        prompt_label: "GitLab",
        init_supported: true,
    },
    ProviderChoice {
        provider: Provider::Forgejo,
        config_value: "forgejo",
        prompt_label: "Forgejo",
        init_supported: true,
    },
    ProviderChoice {
        provider: Provider::Gitea,
        config_value: "gitea",
        prompt_label: "Gitea",
        init_supported: false,
    },
];

impl Provider {
    pub fn choices() -> &'static [ProviderChoice] {
        PROVIDER_CHOICES
    }

    pub fn choice(self) -> &'static ProviderChoice {
        Self::choices()
            .iter()
            .find(|choice| choice.provider == self)
            .expect("every Provider variant must have metadata")
    }

    pub fn as_str(self) -> &'static str {
        self.choice().config_value
    }

    pub fn prompt_label(self) -> &'static str {
        self.choice().prompt_label
    }

    pub fn is_init_supported(self) -> bool {
        self.choice().init_supported
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str((*self).as_str())
    }
}

impl FromStr for Provider {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.trim().to_ascii_lowercase();
        if let Some(choice) = Self::choices()
            .iter()
            .find(|choice| choice.config_value == normalized)
        {
            Ok(choice.provider)
        } else {
            bail!("Unsupported provider `{normalized}` in config.")
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangelogProvider {
    GitCliff,
    Changelogen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangelogProviderChoice {
    pub provider: ChangelogProvider,
    pub config_value: &'static str,
    pub prompt_label: &'static str,
}

const CHANGELOG_PROVIDER_CHOICES: &[ChangelogProviderChoice] = &[
    ChangelogProviderChoice {
        provider: ChangelogProvider::GitCliff,
        config_value: "git-cliff",
        prompt_label: "git-cliff",
    },
    ChangelogProviderChoice {
        provider: ChangelogProvider::Changelogen,
        config_value: "changelogen",
        prompt_label: "changelogen",
    },
];

impl ChangelogProvider {
    pub fn choices() -> &'static [ChangelogProviderChoice] {
        CHANGELOG_PROVIDER_CHOICES
    }

    pub fn choice(self) -> &'static ChangelogProviderChoice {
        Self::choices()
            .iter()
            .find(|choice| choice.provider == self)
            .expect("every ChangelogProvider variant must have metadata")
    }

    pub fn as_str(self) -> &'static str {
        self.choice().config_value
    }

    pub fn prompt_label(self) -> &'static str {
        self.choice().prompt_label
    }
}

impl fmt::Display for ChangelogProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str((*self).as_str())
    }
}

impl FromStr for ChangelogProvider {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.trim().to_ascii_lowercase();
        if let Some(choice) = Self::choices()
            .iter()
            .find(|choice| choice.config_value == normalized)
        {
            Ok(choice.provider)
        } else {
            let expected = Self::choices()
                .iter()
                .map(|choice| format!("`{}`", choice.config_value))
                .collect::<Vec<_>>()
                .join(" or ");
            bail!(
                "Unsupported `release_pr.changelog.provider` value `{normalized}`. Expected {expected}."
            )
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionFileFormat {
    Json,
    Toml,
    Yaml,
}

impl VersionFileFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Toml => "toml",
            Self::Yaml => "yaml",
        }
    }
}

impl fmt::Display for VersionFileFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str((*self).as_str())
    }
}

impl FromStr for VersionFileFormat {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "json" => Ok(Self::Json),
            "toml" => Ok(Self::Toml),
            "yaml" | "yml" => Ok(Self::Yaml),
            other => {
                bail!("Unsupported format override `{other}`. Expected `json`, `toml`, or `yaml`.")
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum ConfigSource {
    Explicit(PathBuf),
    Discovered(PathBuf),
    Defaulted,
}

impl ConfigSource {
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Explicit(path) | Self::Discovered(path) => Some(path.as_path()),
            Self::Defaulted => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitAuthorConfig {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogConfig {
    pub enabled: bool,
    pub provider: ChangelogProvider,
    pub output_file: String,
    pub changelogen: ChangelogenConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogenConfig {
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaggingConfig {
    pub enabled: bool,
    pub tag_template: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewCommentConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasePrConfig {
    pub version_updates: BTreeMap<String, Vec<String>>,
    pub format_overrides: BTreeMap<String, VersionFileFormat>,
    pub release_branch_pattern: String,
    pub pr_template_file: Option<String>,
    pub bump_minor_pre_major: bool,
    pub bump_patch_for_minor_pre_major: bool,
    pub commit_author: CommitAuthorConfig,
    pub changelog: ChangelogConfig,
    pub tagging: TaggingConfig,
    pub preview_comment: PreviewCommentConfig,
}

impl Default for ReleasePrConfig {
    fn default() -> Self {
        Self {
            version_updates: BTreeMap::new(),
            format_overrides: BTreeMap::new(),
            release_branch_pattern: DEFAULT_RELEASE_BRANCH_PATTERN.to_string(),
            pr_template_file: None,
            bump_minor_pre_major: false,
            bump_patch_for_minor_pre_major: false,
            commit_author: CommitAuthorConfig {
                name: DEFAULT_COMMIT_AUTHOR_NAME.to_string(),
                email: DEFAULT_COMMIT_AUTHOR_EMAIL.to_string(),
            },
            changelog: ChangelogConfig {
                enabled: true,
                provider: ChangelogProvider::GitCliff,
                output_file: DEFAULT_CHANGELOG_OUTPUT_FILE.to_string(),
                changelogen: ChangelogenConfig {
                    version: DEFAULT_CHANGELOGEN_VERSION.to_string(),
                },
            },
            tagging: TaggingConfig {
                enabled: DEFAULT_TAGGING_ENABLED,
                tag_template: tag_template::DEFAULT_TAG_TEMPLATE.to_string(),
            },
            preview_comment: PreviewCommentConfig {
                enabled: DEFAULT_PREVIEW_COMMENT_ENABLED,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub provider: Provider,
    pub default_branch: String,
    pub workflow_file: String,
    pub release_pr: ReleasePrConfig,
    pub source: ConfigSource,
    pub warnings: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    provider: Option<String>,
    default_branch: Option<String>,
    workflow_file: Option<String>,
    release_pr: Option<RawReleasePrConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct RawReleasePrConfig {
    version_updates: Option<BTreeMap<String, Vec<String>>>,
    format_overrides: Option<BTreeMap<String, String>>,
    release_branch_pattern: Option<String>,
    pr_template_file: Option<String>,
    bump_minor_pre_major: Option<bool>,
    bump_patch_for_minor_pre_major: Option<bool>,
    commit_author: Option<RawCommitAuthorConfig>,
    changelog: Option<RawChangelogConfig>,
    tagging: Option<RawTaggingConfig>,
    preview_comment: Option<RawPreviewCommentConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct RawCommitAuthorConfig {
    name: Option<String>,
    email: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawChangelogConfig {
    enabled: Option<bool>,
    provider: Option<String>,
    output_file: Option<String>,
    changelogen: Option<RawChangelogenConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct RawChangelogenConfig {
    version: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawTaggingConfig {
    enabled: Option<bool>,
    tag_template: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPreviewCommentConfig {
    enabled: Option<bool>,
}

pub fn load(explicit_path: Option<&Path>, cwd: &Path) -> Result<ResolvedConfig> {
    let config_location = resolve_config_location(explicit_path, cwd)?;

    let (source, raw_contents) = match config_location {
        Some((path, true)) => (
            ConfigSource::Explicit(path.clone()),
            fs::read_to_string(&path)
                .with_context(|| format!("Failed to read config file `{}`.", path.display()))?,
        ),
        Some((path, false)) => (
            ConfigSource::Discovered(path.clone()),
            fs::read_to_string(&path)
                .with_context(|| format!("Failed to read config file `{}`.", path.display()))?,
        ),
        None => {
            return Ok(ResolvedConfig {
                provider: Provider::Github,
                default_branch: DEFAULT_BRANCH.to_string(),
                workflow_file: DEFAULT_WORKFLOW_FILE.to_string(),
                release_pr: ReleasePrConfig::default(),
                source: ConfigSource::Defaulted,
                warnings: Vec::new(),
            });
        }
    };

    let parsed_toml = raw_contents.parse::<toml::Value>().with_context(|| {
        let path = source.path().expect("config source always has path");
        format!("Config file `{}` is not valid TOML.", path.display())
    })?;
    let mut warnings = collect_warnings(&parsed_toml);

    let raw: RawConfig = toml::from_str(&raw_contents).with_context(|| {
        let path = source.path().expect("config source always has path");
        format!(
            "Config file `{}` has unsupported value types.",
            path.display()
        )
    })?;

    let provider = match raw.provider {
        Some(value) => Provider::from_str(&value)?,
        None => Provider::Github,
    };

    let default_branch = raw
        .default_branch
        .unwrap_or_else(|| DEFAULT_BRANCH.to_string())
        .trim()
        .to_string();
    if default_branch.is_empty() {
        bail!("`default_branch` cannot be empty.");
    }

    let workflow_file = raw
        .workflow_file
        .unwrap_or_else(|| default_workflow_file(provider).to_string())
        .trim()
        .to_string();
    if workflow_file.is_empty() {
        bail!("`workflow_file` cannot be empty.");
    }

    let release_pr = resolve_release_pr_config(raw.release_pr)?;

    if release_pr.preview_comment.enabled && provider != Provider::Github {
        warnings.push(
            "`release_pr.preview_comment` is only supported for provider `github` and will be \
             ignored."
                .to_string(),
        );
    }

    Ok(ResolvedConfig {
        provider,
        default_branch,
        workflow_file,
        release_pr,
        source,
        warnings,
    })
}

pub fn default_workflow_file(provider: Provider) -> &'static str {
    match provider {
        Provider::Github | Provider::Forgejo | Provider::Gitea => DEFAULT_WORKFLOW_FILE,
        Provider::Gitlab => DEFAULT_GITLAB_WORKFLOW_FILE,
    }
}

fn resolve_release_pr_config(raw: Option<RawReleasePrConfig>) -> Result<ReleasePrConfig> {
    let Some(raw_release_pr) = raw else {
        return Ok(ReleasePrConfig::default());
    };

    let RawReleasePrConfig {
        version_updates,
        format_overrides,
        release_branch_pattern,
        pr_template_file,
        bump_minor_pre_major,
        bump_patch_for_minor_pre_major,
        commit_author,
        changelog,
        tagging,
        preview_comment,
    } = raw_release_pr;
    let version_updates = resolve_version_updates(version_updates)?;
    let format_overrides = resolve_format_overrides(format_overrides, &version_updates)?;
    let release_branch_pattern = resolve_release_branch_pattern(release_branch_pattern)?;
    let pr_template_file = pr_template_file
        .map(|path| normalize_repo_relative_path(&path, "`release_pr.pr_template_file` path"))
        .transpose()?;

    Ok(ReleasePrConfig {
        version_updates,
        format_overrides,
        release_branch_pattern,
        pr_template_file,
        bump_minor_pre_major: bump_minor_pre_major.unwrap_or(false),
        bump_patch_for_minor_pre_major: bump_patch_for_minor_pre_major.unwrap_or(false),
        commit_author: resolve_commit_author(commit_author)?,
        changelog: resolve_changelog_config(changelog)?,
        tagging: resolve_tagging_config(tagging)?,
        preview_comment: resolve_preview_comment_config(preview_comment),
    })
}

fn resolve_version_updates(
    raw: Option<BTreeMap<String, Vec<String>>>,
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut version_updates = BTreeMap::new();
    for (path, keys) in raw.unwrap_or_default() {
        let normalized_path =
            normalize_repo_relative_path(&path, "`release_pr.version_updates` path")?;
        if keys.is_empty() {
            bail!("`release_pr.version_updates[\"{normalized_path}\"]` cannot be empty.");
        }

        let mut normalized_keys = Vec::with_capacity(keys.len());
        for key in keys {
            normalized_keys.push(normalize_version_selector(&key)?);
        }

        if version_updates
            .insert(normalized_path.clone(), normalized_keys)
            .is_some()
        {
            bail!("Duplicate `release_pr.version_updates` path `{normalized_path}`.");
        }
    }

    Ok(version_updates)
}

fn resolve_format_overrides(
    raw: Option<BTreeMap<String, String>>,
    version_updates: &BTreeMap<String, Vec<String>>,
) -> Result<BTreeMap<String, VersionFileFormat>> {
    let mut format_overrides = BTreeMap::new();
    for (path, format_value) in raw.unwrap_or_default() {
        let normalized_path =
            normalize_repo_relative_path(&path, "`release_pr.format_overrides` path")?;
        if !version_updates.contains_key(&normalized_path) {
            bail!(
                "`release_pr.format_overrides` includes `{normalized_path}`, but no matching \
                 `release_pr.version_updates` entry exists."
            );
        }

        let format = VersionFileFormat::from_str(&format_value)?;
        if format_overrides
            .insert(normalized_path.clone(), format)
            .is_some()
        {
            bail!("Duplicate `release_pr.format_overrides` path `{normalized_path}`.");
        }
    }

    Ok(format_overrides)
}

fn resolve_release_branch_pattern(raw: Option<String>) -> Result<String> {
    let release_branch_pattern = raw
        .unwrap_or_else(|| DEFAULT_RELEASE_BRANCH_PATTERN.to_string())
        .trim()
        .to_string();
    if release_branch_pattern.is_empty() {
        bail!("`release_pr.release_branch_pattern` cannot be empty.");
    }
    validate_branch_pattern(&release_branch_pattern)?;

    Ok(release_branch_pattern)
}

fn resolve_commit_author(raw: Option<RawCommitAuthorConfig>) -> Result<CommitAuthorConfig> {
    let raw_author = raw.unwrap_or_default();
    let commit_author_name = raw_author
        .name
        .unwrap_or_else(|| DEFAULT_COMMIT_AUTHOR_NAME.to_string())
        .trim()
        .to_string();
    if commit_author_name.is_empty() {
        bail!("`release_pr.commit_author.name` cannot be empty.");
    }

    let commit_author_email = raw_author
        .email
        .unwrap_or_else(|| DEFAULT_COMMIT_AUTHOR_EMAIL.to_string())
        .trim()
        .to_string();
    if commit_author_email.is_empty() {
        bail!("`release_pr.commit_author.email` cannot be empty.");
    }

    Ok(CommitAuthorConfig {
        name: commit_author_name,
        email: commit_author_email,
    })
}

fn resolve_changelog_config(raw: Option<RawChangelogConfig>) -> Result<ChangelogConfig> {
    let raw_changelog = raw.unwrap_or_default();
    let changelog_enabled = raw_changelog.enabled.unwrap_or(true);
    let changelog_provider = raw_changelog
        .provider
        .as_deref()
        .map(ChangelogProvider::from_str)
        .transpose()?
        .unwrap_or(ChangelogProvider::GitCliff);
    let changelog_output_file = normalize_repo_relative_path(
        raw_changelog
            .output_file
            .as_deref()
            .unwrap_or(DEFAULT_CHANGELOG_OUTPUT_FILE),
        "`release_pr.changelog.output_file` path",
    )?;
    let raw_changelogen = raw_changelog.changelogen.unwrap_or_default();
    let changelogen_version = match changelog_provider {
        ChangelogProvider::Changelogen => {
            normalize_changelogen_version(raw_changelogen.version.as_deref())?
        }
        ChangelogProvider::GitCliff => DEFAULT_CHANGELOGEN_VERSION.to_string(),
    };

    Ok(ChangelogConfig {
        enabled: changelog_enabled,
        provider: changelog_provider,
        output_file: changelog_output_file,
        changelogen: ChangelogenConfig {
            version: changelogen_version,
        },
    })
}

fn resolve_tagging_config(raw: Option<RawTaggingConfig>) -> Result<TaggingConfig> {
    let raw_tagging = raw.unwrap_or_default();
    let tagging_enabled = raw_tagging.enabled.unwrap_or(DEFAULT_TAGGING_ENABLED);
    let tag_template = tag_template::normalize_tag_template(
        raw_tagging
            .tag_template
            .as_deref()
            .unwrap_or(tag_template::DEFAULT_TAG_TEMPLATE),
    )
    .context("Invalid `release_pr.tagging.tag_template`.")?;

    Ok(TaggingConfig {
        enabled: tagging_enabled,
        tag_template,
    })
}

fn resolve_preview_comment_config(raw: Option<RawPreviewCommentConfig>) -> PreviewCommentConfig {
    let raw_preview_comment = raw.unwrap_or_default();

    PreviewCommentConfig {
        enabled: raw_preview_comment
            .enabled
            .unwrap_or(DEFAULT_PREVIEW_COMMENT_ENABLED),
    }
}

fn normalize_repo_relative_path(value: &str, label: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{label} cannot be empty.");
    }

    let path = Path::new(trimmed);
    if path.is_absolute() {
        bail!("{label} `{trimmed}` must be repository-relative.");
    }

    for component in path.components() {
        match component {
            Component::CurDir | Component::Normal(_) => {}
            Component::ParentDir => {
                bail!("{label} `{trimmed}` cannot contain `..`.");
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("{label} `{trimmed}` must be repository-relative.");
            }
        }
    }

    Ok(trimmed.to_string())
}

fn normalize_changelogen_version(value: Option<&str>) -> Result<String> {
    let trimmed = value.unwrap_or(DEFAULT_CHANGELOGEN_VERSION).trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_CHANGELOGEN_VERSION.to_string());
    }

    if Version::parse(trimmed).is_err() {
        bail!(
            "`release_pr.changelog.changelogen.version` must be a SemVer version like `{DEFAULT_CHANGELOGEN_VERSION}`."
        );
    }

    Ok(trimmed.to_string())
}

fn normalize_version_selector(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("Version selector cannot be empty.");
    }

    version_selector::parse_selector(trimmed).with_context(|| {
        format!("Invalid version selector `{trimmed}` in `release_pr.version_updates`.")
    })?;

    Ok(trimmed.to_string())
}

fn validate_branch_pattern(pattern: &str) -> Result<()> {
    let mut remaining = pattern;
    while let Some(start_idx) = remaining.find("{{") {
        let after_open = &remaining[start_idx + 2..];
        let Some(end_rel_idx) = after_open.find("}}") else {
            bail!("Invalid `release_pr.release_branch_pattern`: unmatched `{{` in `{pattern}`.");
        };
        let token = after_open[..end_rel_idx].trim();
        if token != "version" {
            bail!(
                "Invalid `release_pr.release_branch_pattern`: unsupported token `{{{{{token}}}}}`. \
                 Only `{{{{version}}}}` is supported."
            );
        }
        remaining = &after_open[end_rel_idx + 2..];
    }

    if remaining.contains("}}") {
        bail!("Invalid `release_pr.release_branch_pattern`: unmatched `}}` in `{pattern}`.");
    }

    Ok(())
}

fn collect_warnings(parsed: &toml::Value) -> Vec<String> {
    let mut warnings = Vec::new();
    let Some(root) = parsed.as_table() else {
        return warnings;
    };

    let allowed_root: BTreeSet<&str> =
        BTreeSet::from(["provider", "default_branch", "workflow_file", "release_pr"]);
    for key in root
        .keys()
        .filter(|key| !allowed_root.contains(key.as_str()))
    {
        warnings.push(format!("Unknown config key `{key}` was ignored."));
    }

    let Some(release_pr) = root.get("release_pr").and_then(toml::Value::as_table) else {
        return warnings;
    };

    let allowed_release_pr: BTreeSet<&str> = BTreeSet::from([
        "version_updates",
        "format_overrides",
        "release_branch_pattern",
        "pr_template_file",
        "bump_minor_pre_major",
        "bump_patch_for_minor_pre_major",
        "commit_author",
        "changelog",
        "tagging",
        "preview_comment",
    ]);
    for key in release_pr
        .keys()
        .filter(|key| !allowed_release_pr.contains(key.as_str()))
    {
        warnings.push(format!(
            "Unknown config key `release_pr.{key}` was ignored."
        ));
    }

    let Some(commit_author) = release_pr
        .get("commit_author")
        .and_then(toml::Value::as_table)
    else {
        return collect_release_pr_nested_warnings(release_pr, warnings);
    };

    let allowed_author: BTreeSet<&str> = BTreeSet::from(["name", "email"]);
    for key in commit_author
        .keys()
        .filter(|key| !allowed_author.contains(key.as_str()))
    {
        warnings.push(format!(
            "Unknown config key `release_pr.commit_author.{key}` was ignored."
        ));
    }

    collect_release_pr_nested_warnings(release_pr, warnings)
}

fn collect_release_pr_nested_warnings(
    release_pr: &toml::value::Table,
    mut warnings: Vec<String>,
) -> Vec<String> {
    if let Some(changelog) = release_pr.get("changelog").and_then(toml::Value::as_table) {
        let allowed_changelog: BTreeSet<&str> =
            BTreeSet::from(["enabled", "provider", "output_file", "changelogen"]);
        for key in changelog
            .keys()
            .filter(|key| !allowed_changelog.contains(key.as_str()))
        {
            warnings.push(format!(
                "Unknown config key `release_pr.changelog.{key}` was ignored."
            ));
        }

        if let Some(changelogen) = changelog.get("changelogen").and_then(toml::Value::as_table) {
            let allowed_changelogen: BTreeSet<&str> = BTreeSet::from(["version"]);
            for key in changelogen
                .keys()
                .filter(|key| !allowed_changelogen.contains(key.as_str()))
            {
                warnings.push(format!(
                    "Unknown config key `release_pr.changelog.changelogen.{key}` was ignored."
                ));
            }
        }
    }

    if let Some(tagging) = release_pr.get("tagging").and_then(toml::Value::as_table) {
        let allowed_tagging: BTreeSet<&str> = BTreeSet::from(["enabled", "tag_template"]);
        for key in tagging
            .keys()
            .filter(|key| !allowed_tagging.contains(key.as_str()))
        {
            warnings.push(format!(
                "Unknown config key `release_pr.tagging.{key}` was ignored."
            ));
        }
    }

    if let Some(preview_comment) = release_pr
        .get("preview_comment")
        .and_then(toml::Value::as_table)
    {
        let allowed_preview_comment: BTreeSet<&str> = BTreeSet::from(["enabled"]);
        for key in preview_comment
            .keys()
            .filter(|key| !allowed_preview_comment.contains(key.as_str()))
        {
            warnings.push(format!(
                "Unknown config key `release_pr.preview_comment.{key}` was ignored."
            ));
        }
    }

    warnings
}

fn resolve_config_location(
    explicit_path: Option<&Path>,
    cwd: &Path,
) -> Result<Option<(PathBuf, bool)>> {
    if let Some(explicit) = explicit_path {
        if !explicit.exists() {
            bail!(
                "Config file `{}` was not found. Pass a valid path with `--config`.",
                explicit.display()
            );
        }
        return Ok(Some((explicit.to_path_buf(), true)));
    }

    for candidate in ["brel.toml", ".brel.toml"] {
        let path = cwd.join(candidate);
        if path.exists() {
            return Ok(Some((path, false)));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn provider_metadata_is_the_source_of_truth() {
        let choices = Provider::choices();
        assert_eq!(choices.len(), 4);
        assert_eq!(
            choices
                .iter()
                .map(|choice| (
                    choice.provider,
                    choice.config_value,
                    choice.prompt_label,
                    choice.init_supported
                ))
                .collect::<Vec<_>>(),
            vec![
                (Provider::Github, "github", "GitHub", true),
                (Provider::Gitlab, "gitlab", "GitLab", true),
                (Provider::Forgejo, "forgejo", "Forgejo", true),
                (Provider::Gitea, "gitea", "Gitea", false),
            ]
        );

        for choice in choices {
            assert_eq!(choice.provider.as_str(), choice.config_value);
            assert_eq!(choice.provider.prompt_label(), choice.prompt_label);
            assert_eq!(choice.provider.is_init_supported(), choice.init_supported);
            assert_eq!(
                Provider::from_str(choice.config_value).unwrap(),
                choice.provider
            );
        }
    }

    #[test]
    fn init_supported_provider_choices_include_github_gitlab_and_forgejo() {
        let init_supported = Provider::choices()
            .iter()
            .filter(|choice| choice.init_supported)
            .map(|choice| choice.provider)
            .collect::<Vec<_>>();

        assert_eq!(
            init_supported,
            vec![Provider::Github, Provider::Gitlab, Provider::Forgejo]
        );
    }

    #[test]
    fn changelog_provider_metadata_is_the_source_of_truth() {
        let choices = ChangelogProvider::choices();
        assert_eq!(choices.len(), 2);
        assert_eq!(
            choices
                .iter()
                .map(|choice| (choice.provider, choice.config_value, choice.prompt_label))
                .collect::<Vec<_>>(),
            vec![
                (ChangelogProvider::GitCliff, "git-cliff", "git-cliff"),
                (ChangelogProvider::Changelogen, "changelogen", "changelogen"),
            ]
        );

        for choice in choices {
            assert_eq!(choice.provider.as_str(), choice.config_value);
            assert_eq!(choice.provider.prompt_label(), choice.prompt_label);
            assert_eq!(
                ChangelogProvider::from_str(choice.config_value).unwrap(),
                choice.provider
            );
        }
    }

    #[test]
    fn discovers_brel_toml_before_dot_brel_toml() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();

        fs::write(cwd.join(".brel.toml"), "default_branch = \"dev\"").unwrap();
        fs::write(cwd.join("brel.toml"), "default_branch = \"mainline\"").unwrap();

        let config = load(None, cwd).unwrap();
        assert_eq!(config.default_branch, "mainline");
        assert!(matches!(config.source, ConfigSource::Discovered(_)));
    }

    #[test]
    fn explicit_config_path_wins_over_discovery() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        let explicit_path = cwd.join("custom.toml");

        fs::write(cwd.join("brel.toml"), "default_branch = \"mainline\"").unwrap();
        fs::write(&explicit_path, "default_branch = \"release\"").unwrap();

        let config = load(Some(explicit_path.as_path()), cwd).unwrap();
        assert_eq!(config.default_branch, "release");
        assert!(matches!(config.source, ConfigSource::Explicit(_)));
    }

    #[test]
    fn returns_defaults_when_no_config_file_exists() {
        let temp_dir = tempdir().unwrap();
        let config = load(None, temp_dir.path()).unwrap();

        assert_eq!(config.provider, Provider::Github);
        assert_eq!(config.default_branch, "main");
        assert_eq!(config.workflow_file, "release-pr.yml");
        assert_eq!(
            config.release_pr.release_branch_pattern,
            DEFAULT_RELEASE_BRANCH_PATTERN
        );
        assert_eq!(config.release_pr.version_updates.len(), 0);
        assert!(config.release_pr.changelog.enabled);
        assert_eq!(
            config.release_pr.changelog.output_file,
            DEFAULT_CHANGELOG_OUTPUT_FILE
        );
        assert_eq!(
            config.release_pr.changelog.provider,
            ChangelogProvider::GitCliff
        );
        assert_eq!(
            config.release_pr.changelog.changelogen.version,
            DEFAULT_CHANGELOGEN_VERSION
        );
        assert!(!config.release_pr.tagging.enabled);
        assert_eq!(
            config.release_pr.tagging.tag_template,
            tag_template::DEFAULT_TAG_TEMPLATE
        );
        assert!(matches!(config.source, ConfigSource::Defaulted));
    }

    #[test]
    fn fails_on_invalid_toml() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(cwd.join("brel.toml"), "provider = [").unwrap();

        let err = load(None, cwd).unwrap_err();
        assert!(err.to_string().contains("not valid TOML"));
    }

    #[test]
    fn fails_on_unknown_provider() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(cwd.join("brel.toml"), "provider = \"bitbucket\"").unwrap();

        let err = load(None, cwd).unwrap_err();
        assert!(err.to_string().contains("Unsupported provider"));
    }

    #[test]
    fn parses_forgejo_provider() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(cwd.join("brel.toml"), "provider = \"forgejo\"").unwrap();

        let config = load(None, cwd).unwrap();
        assert_eq!(config.provider, Provider::Forgejo);
        assert_eq!(config.workflow_file, DEFAULT_WORKFLOW_FILE);
    }

    #[test]
    fn warns_on_unknown_root_keys() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            "provider = \"github\"\nexperimental = true",
        )
        .unwrap();

        let config = load(None, cwd).unwrap();
        assert_eq!(config.warnings.len(), 1);
        assert!(config.warnings[0].contains("experimental"));
    }

    #[test]
    fn parses_release_pr_version_update_map() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
provider = "github"

[release_pr.version_updates]
"package.json" = ["version"]
"Cargo.toml" = ["package.version"]
"workflow.yml" = ["release.version"]

[release_pr.format_overrides]
"Cargo.toml" = "toml"
"workflow.yml" = "yaml"

[release_pr.commit_author]
name = "release bot"
email = "release@example.com"
"#,
        )
        .unwrap();

        let config = load(None, cwd).unwrap();
        assert_eq!(
            config
                .release_pr
                .version_updates
                .get("package.json")
                .unwrap(),
            &vec!["version".to_string()]
        );
        assert_eq!(
            config.release_pr.version_updates.get("Cargo.toml").unwrap(),
            &vec!["package.version".to_string()]
        );
        assert_eq!(
            config.release_pr.format_overrides.get("Cargo.toml"),
            Some(&VersionFileFormat::Toml)
        );
        assert_eq!(
            config
                .release_pr
                .version_updates
                .get("workflow.yml")
                .unwrap(),
            &vec!["release.version".to_string()]
        );
        assert_eq!(
            config.release_pr.format_overrides.get("workflow.yml"),
            Some(&VersionFileFormat::Yaml)
        );
        assert_eq!(config.release_pr.commit_author.name, "release bot");
        assert_eq!(config.release_pr.commit_author.email, "release@example.com");
        assert!(config.release_pr.changelog.enabled);
        assert_eq!(
            config.release_pr.changelog.output_file,
            DEFAULT_CHANGELOG_OUTPUT_FILE
        );
        assert_eq!(
            config.release_pr.changelog.provider,
            ChangelogProvider::GitCliff
        );
        assert_eq!(
            config.release_pr.changelog.changelogen.version,
            DEFAULT_CHANGELOGEN_VERSION
        );
        assert!(!config.release_pr.tagging.enabled);
        assert_eq!(
            config.release_pr.tagging.tag_template,
            tag_template::DEFAULT_TAG_TEMPLATE
        );
    }

    #[test]
    fn parses_release_pr_version_selectors() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr.version_updates]
"package.json" = ["packages[0].version", "package[name=brel].version"]
"#,
        )
        .unwrap();

        let config = load(None, cwd).unwrap();
        assert_eq!(
            config
                .release_pr
                .version_updates
                .get("package.json")
                .unwrap(),
            &vec![
                "packages[0].version".to_string(),
                "package[name=brel].version".to_string()
            ]
        );
    }

    #[test]
    fn parses_release_pr_changelog_settings() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr.changelog]
enabled = false
provider = "changelogen"
output_file = "docs/changelog.md"

[release_pr.changelog.changelogen]
version = "0.5.0"
"#,
        )
        .unwrap();

        let config = load(None, cwd).unwrap();
        assert!(!config.release_pr.changelog.enabled);
        assert_eq!(
            config.release_pr.changelog.provider,
            ChangelogProvider::Changelogen
        );
        assert_eq!(config.release_pr.changelog.output_file, "docs/changelog.md");
        assert_eq!(config.release_pr.changelog.changelogen.version, "0.5.0");
    }

    #[test]
    fn pre_major_bump_toggles_default_to_false() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(cwd.join("brel.toml"), "[release_pr]\n").unwrap();

        let config = load(None, cwd).unwrap();
        assert!(!config.release_pr.bump_minor_pre_major);
        assert!(!config.release_pr.bump_patch_for_minor_pre_major);
    }

    #[test]
    fn parses_pre_major_bump_toggles() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            "[release_pr]\nbump_minor_pre_major = true\nbump_patch_for_minor_pre_major = true\n",
        )
        .unwrap();

        let config = load(None, cwd).unwrap();
        assert!(config.release_pr.bump_minor_pre_major);
        assert!(config.release_pr.bump_patch_for_minor_pre_major);
        assert!(config.warnings.is_empty());
    }

    #[test]
    fn empty_changelogen_version_uses_default() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr.changelog.changelogen]
version = ""
"#,
        )
        .unwrap();

        let config = load(None, cwd).unwrap();
        assert_eq!(
            config.release_pr.changelog.changelogen.version,
            DEFAULT_CHANGELOGEN_VERSION
        );
    }

    #[test]
    fn parses_explicit_git_cliff_changelog_provider() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr.changelog]
provider = "git-cliff"
"#,
        )
        .unwrap();

        let config = load(None, cwd).unwrap();
        assert_eq!(
            config.release_pr.changelog.provider,
            ChangelogProvider::GitCliff
        );
    }

    #[test]
    fn default_git_cliff_provider_ignores_invalid_changelogen_version() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr.changelog.changelogen]
version = "latest"
"#,
        )
        .unwrap();

        let config = load(None, cwd).unwrap();
        assert_eq!(
            config.release_pr.changelog.provider,
            ChangelogProvider::GitCliff
        );
        assert_eq!(
            config.release_pr.changelog.changelogen.version,
            DEFAULT_CHANGELOGEN_VERSION
        );
    }

    #[test]
    fn explicit_git_cliff_provider_ignores_invalid_changelogen_version() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr.changelog]
provider = "git-cliff"

[release_pr.changelog.changelogen]
version = "latest"
"#,
        )
        .unwrap();

        let config = load(None, cwd).unwrap();
        assert_eq!(
            config.release_pr.changelog.provider,
            ChangelogProvider::GitCliff
        );
        assert_eq!(
            config.release_pr.changelog.changelogen.version,
            DEFAULT_CHANGELOGEN_VERSION
        );
    }

    #[test]
    fn rejects_invalid_changelog_provider() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr.changelog]
provider = "town-crier"
"#,
        )
        .unwrap();

        let err = load(None, cwd).unwrap_err();
        assert!(
            err.to_string()
                .contains("Unsupported `release_pr.changelog.provider` value `town-crier`")
        );
    }

    #[test]
    fn rejects_invalid_changelogen_version() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr.changelog]
provider = "changelogen"

[release_pr.changelog.changelogen]
version = "latest"
"#,
        )
        .unwrap();

        let err = load(None, cwd).unwrap_err();
        assert!(
            err.to_string()
                .contains("`release_pr.changelog.changelogen.version` must be a SemVer version")
        );
    }

    #[test]
    fn parses_release_pr_tagging_settings() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr.tagging]
enabled = true
tag_template = "{{version}}"
"#,
        )
        .unwrap();

        let config = load(None, cwd).unwrap();
        assert!(config.release_pr.tagging.enabled);
        assert_eq!(config.release_pr.tagging.tag_template, "{version}");
    }

    #[test]
    fn preview_comment_defaults_to_disabled() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(cwd.join("brel.toml"), "[release_pr]\n").unwrap();

        let config = load(None, cwd).unwrap();
        assert!(!config.release_pr.preview_comment.enabled);
        assert!(config.warnings.is_empty());
    }

    #[test]
    fn parses_release_pr_preview_comment_settings() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr.preview_comment]
enabled = true
"#,
        )
        .unwrap();

        let config = load(None, cwd).unwrap();
        assert!(config.release_pr.preview_comment.enabled);
        assert!(config.warnings.is_empty());
    }

    #[test]
    fn warns_on_unknown_preview_comment_keys() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr.preview_comment]
enabled = true
extra = 1
"#,
        )
        .unwrap();

        let config = load(None, cwd).unwrap();
        assert_eq!(config.warnings.len(), 1);
        assert!(config.warnings[0].contains("release_pr.preview_comment.extra"));
    }

    #[test]
    fn warns_when_preview_comment_enabled_for_non_github_provider() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
provider = "gitlab"

[release_pr.preview_comment]
enabled = true
"#,
        )
        .unwrap();

        let config = load(None, cwd).unwrap();
        assert_eq!(config.warnings.len(), 1);
        assert!(config.warnings[0].contains("only supported for provider `github`"));
    }

    #[test]
    fn rejects_invalid_release_pr_tag_template() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr.tagging]
tag_template = "release"
"#,
        )
        .unwrap();

        let err = load(None, cwd).unwrap_err();
        assert!(
            err.to_string()
                .contains("Invalid `release_pr.tagging.tag_template`")
        );
    }

    #[test]
    fn rejects_empty_version_selector() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr.version_updates]
"package.json" = [""]
"#,
        )
        .unwrap();

        let err = load(None, cwd).unwrap_err();
        assert!(err.to_string().contains("Version selector cannot be empty"));
    }

    #[test]
    fn rejects_malformed_version_selector() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr.version_updates]
"package.json" = ["package[name].version"]
"#,
        )
        .unwrap();

        let err = load(None, cwd).unwrap_err();
        assert!(err.to_string().contains("Invalid version selector"));
    }

    #[test]
    fn rejects_format_override_without_matching_update_target() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr.version_updates]
"package.json" = ["version"]

[release_pr.format_overrides]
"Cargo.toml" = "toml"
"#,
        )
        .unwrap();

        let err = load(None, cwd).unwrap_err();
        assert!(
            err.to_string()
                .contains("no matching `release_pr.version_updates` entry")
        );
    }

    #[test]
    fn warns_on_unknown_nested_release_pr_keys() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr]
foo = "bar"

[release_pr.commit_author]
name = "brel[bot]"
email = "brel@example.com"
extra = "x"

[release_pr.changelog]
provider = "changelogen"
wat = true

[release_pr.changelog.changelogen]
version = "0.6.2"
extra = true

[release_pr.tagging]
extra = 1
"#,
        )
        .unwrap();

        let config = load(None, cwd).unwrap();
        assert_eq!(config.warnings.len(), 5);
        assert!(
            config
                .warnings
                .iter()
                .any(|warning| warning.contains("release_pr.foo"))
        );
        assert!(
            config
                .warnings
                .iter()
                .any(|warning| warning.contains("release_pr.commit_author.extra"))
        );
        assert!(
            config
                .warnings
                .iter()
                .any(|warning| warning.contains("release_pr.changelog.wat"))
        );
        assert!(
            config
                .warnings
                .iter()
                .all(|warning| !warning.contains("release_pr.changelog.provider"))
        );
        assert!(
            config
                .warnings
                .iter()
                .any(|warning| warning.contains("release_pr.changelog.changelogen.extra"))
        );
        assert!(
            config
                .warnings
                .iter()
                .all(|warning| !warning.contains("release_pr.changelog.changelogen.version"))
        );
        assert!(
            config
                .warnings
                .iter()
                .any(|warning| warning.contains("release_pr.tagging.extra"))
        );
    }

    #[test]
    fn rejects_parent_segments_in_release_pr_paths() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr.version_updates]
"../package.json" = ["version"]
"#,
        )
        .unwrap();

        let err = load(None, cwd).unwrap_err();
        assert!(err.to_string().contains("cannot contain `..`"));
    }

    #[test]
    fn rejects_parent_segments_in_changelog_output_path() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr.changelog]
output_file = "../CHANGELOG.md"
"#,
        )
        .unwrap();

        let err = load(None, cwd).unwrap_err();
        assert!(err.to_string().contains("cannot contain `..`"));
    }

    #[test]
    fn validates_release_branch_pattern_tokens() {
        let temp_dir = tempdir().unwrap();
        let cwd = temp_dir.path();
        fs::write(
            cwd.join("brel.toml"),
            r#"
[release_pr]
release_branch_pattern = "brel/release/{{date}}"
"#,
        )
        .unwrap();

        let err = load(None, cwd).unwrap_err();
        assert!(err.to_string().contains("unsupported token"));
    }
}
