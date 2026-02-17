use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

pub const DEFAULT_BRANCH: &str = "main";
pub const DEFAULT_WORKFLOW_FILE: &str = "release-pr.yml";
pub const DEFAULT_RELEASE_BRANCH_PATTERN: &str = "brel/release/v{{version}}";
pub const DEFAULT_COMMIT_AUTHOR_NAME: &str = "brel[bot]";
pub const DEFAULT_COMMIT_AUTHOR_EMAIL: &str = "brel[bot]@users.noreply.github.com";
pub const DEFAULT_CHANGELOG_OUTPUT_FILE: &str = "CHANGELOG.md";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Github,
    Gitlab,
    Gitea,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
            Self::Gitea => "gitea",
        }
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
        match value.trim().to_ascii_lowercase().as_str() {
            "github" => Ok(Self::Github),
            "gitlab" => Ok(Self::Gitlab),
            "gitea" => Ok(Self::Gitea),
            other => bail!("Unsupported provider `{other}` in config."),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionFileFormat {
    Json,
    Toml,
}

impl VersionFileFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Toml => "toml",
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
            other => bail!("Unsupported format override `{other}`. Expected `json` or `toml`."),
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
    pub output_file: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasePrConfig {
    pub version_updates: BTreeMap<String, Vec<String>>,
    pub format_overrides: BTreeMap<String, VersionFileFormat>,
    pub release_branch_pattern: String,
    pub pr_template_file: Option<String>,
    pub commit_author: CommitAuthorConfig,
    pub changelog: ChangelogConfig,
}

impl Default for ReleasePrConfig {
    fn default() -> Self {
        Self {
            version_updates: BTreeMap::new(),
            format_overrides: BTreeMap::new(),
            release_branch_pattern: DEFAULT_RELEASE_BRANCH_PATTERN.to_string(),
            pr_template_file: None,
            commit_author: CommitAuthorConfig {
                name: DEFAULT_COMMIT_AUTHOR_NAME.to_string(),
                email: DEFAULT_COMMIT_AUTHOR_EMAIL.to_string(),
            },
            changelog: ChangelogConfig {
                enabled: true,
                output_file: DEFAULT_CHANGELOG_OUTPUT_FILE.to_string(),
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

#[derive(Debug, facet::Facet)]
struct RawConfig {
    provider: Option<String>,
    default_branch: Option<String>,
    workflow_file: Option<String>,
    release_pr: Option<RawReleasePrConfig>,
}

#[derive(Debug, Default, facet::Facet)]
struct RawReleasePrConfig {
    version_updates: Option<BTreeMap<String, Vec<String>>>,
    format_overrides: Option<BTreeMap<String, String>>,
    release_branch_pattern: Option<String>,
    pr_template_file: Option<String>,
    commit_author: Option<RawCommitAuthorConfig>,
    changelog: Option<RawChangelogConfig>,
}

#[derive(Debug, Default, facet::Facet)]
struct RawCommitAuthorConfig {
    name: Option<String>,
    email: Option<String>,
}

#[derive(Debug, Default, facet::Facet)]
struct RawChangelogConfig {
    enabled: Option<bool>,
    output_file: Option<String>,
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
    let warnings = collect_warnings(&parsed_toml);

    let raw: RawConfig = facet_toml::from_str(&raw_contents).with_context(|| {
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
        .unwrap_or_else(|| DEFAULT_WORKFLOW_FILE.to_string())
        .trim()
        .to_string();
    if workflow_file.is_empty() {
        bail!("`workflow_file` cannot be empty.");
    }

    let release_pr = resolve_release_pr_config(raw.release_pr)?;

    Ok(ResolvedConfig {
        provider,
        default_branch,
        workflow_file,
        release_pr,
        source,
        warnings,
    })
}

fn resolve_release_pr_config(raw: Option<RawReleasePrConfig>) -> Result<ReleasePrConfig> {
    let Some(raw_release_pr) = raw else {
        return Ok(ReleasePrConfig::default());
    };

    let mut version_updates = BTreeMap::new();
    for (path, keys) in raw_release_pr.version_updates.unwrap_or_default() {
        let normalized_path =
            normalize_repo_relative_path(&path, "`release_pr.version_updates` path")?;
        if keys.is_empty() {
            bail!("`release_pr.version_updates[\"{normalized_path}\"]` cannot be empty.");
        }

        let mut normalized_keys = Vec::with_capacity(keys.len());
        for key in keys {
            normalized_keys.push(normalize_dot_path(&key)?);
        }

        if version_updates
            .insert(normalized_path.clone(), normalized_keys)
            .is_some()
        {
            bail!("Duplicate `release_pr.version_updates` path `{normalized_path}`.");
        }
    }

    let mut format_overrides = BTreeMap::new();
    for (path, format_value) in raw_release_pr.format_overrides.unwrap_or_default() {
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

    let release_branch_pattern = raw_release_pr
        .release_branch_pattern
        .unwrap_or_else(|| DEFAULT_RELEASE_BRANCH_PATTERN.to_string())
        .trim()
        .to_string();
    if release_branch_pattern.is_empty() {
        bail!("`release_pr.release_branch_pattern` cannot be empty.");
    }
    validate_branch_pattern(&release_branch_pattern)?;

    let pr_template_file = match raw_release_pr.pr_template_file {
        Some(path) => {
            let normalized =
                normalize_repo_relative_path(&path, "`release_pr.pr_template_file` path")?;
            Some(normalized)
        }
        None => None,
    };

    let raw_author = raw_release_pr.commit_author.unwrap_or_default();
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

    let raw_changelog = raw_release_pr.changelog.unwrap_or_default();
    let changelog_enabled = raw_changelog.enabled.unwrap_or(true);
    let changelog_output_file = normalize_repo_relative_path(
        raw_changelog
            .output_file
            .as_deref()
            .unwrap_or(DEFAULT_CHANGELOG_OUTPUT_FILE),
        "`release_pr.changelog.output_file` path",
    )?;

    Ok(ReleasePrConfig {
        version_updates,
        format_overrides,
        release_branch_pattern,
        pr_template_file,
        commit_author: CommitAuthorConfig {
            name: commit_author_name,
            email: commit_author_email,
        },
        changelog: ChangelogConfig {
            enabled: changelog_enabled,
            output_file: changelog_output_file,
        },
    })
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
            Component::CurDir => {}
            Component::Normal(_) => {}
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

fn normalize_dot_path(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("Version key path cannot be empty.");
    }

    if trimmed.split('.').any(|segment| segment.trim().is_empty()) {
        bail!("Version key path `{trimmed}` is invalid. Use dot-separated non-empty segments.");
    }

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
        "commit_author",
        "changelog",
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
        return collect_changelog_warnings(release_pr, warnings);
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

    collect_changelog_warnings(release_pr, warnings)
}

fn collect_changelog_warnings(
    release_pr: &toml::value::Table,
    mut warnings: Vec<String>,
) -> Vec<String> {
    let Some(changelog) = release_pr.get("changelog").and_then(toml::Value::as_table) else {
        return warnings;
    };

    let allowed_changelog: BTreeSet<&str> = BTreeSet::from(["enabled", "output_file"]);
    for key in changelog
        .keys()
        .filter(|key| !allowed_changelog.contains(key.as_str()))
    {
        warnings.push(format!(
            "Unknown config key `release_pr.changelog.{key}` was ignored."
        ));
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

[release_pr.format_overrides]
"Cargo.toml" = "toml"

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
        assert_eq!(config.release_pr.commit_author.name, "release bot");
        assert_eq!(config.release_pr.commit_author.email, "release@example.com");
        assert!(config.release_pr.changelog.enabled);
        assert_eq!(
            config.release_pr.changelog.output_file,
            DEFAULT_CHANGELOG_OUTPUT_FILE
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
output_file = "docs/changelog.md"
"#,
        )
        .unwrap();

        let config = load(None, cwd).unwrap();
        assert!(!config.release_pr.changelog.enabled);
        assert_eq!(config.release_pr.changelog.output_file, "docs/changelog.md");
    }

    #[test]
    fn rejects_empty_version_key_path() {
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
        assert!(err.to_string().contains("Version key path"));
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
wat = true
"#,
        )
        .unwrap();

        let config = load(None, cwd).unwrap();
        assert_eq!(config.warnings.len(), 3);
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
