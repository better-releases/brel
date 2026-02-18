use anyhow::{Result, bail};
use semver::Version;

pub const DEFAULT_TAG_TEMPLATE: &str = "v{version}";
pub const VERSION_TOKEN: &str = "{version}";
const LEGACY_VERSION_TOKEN: &str = "{{version}}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagTemplate {
    canonical: String,
    prefix: String,
    suffix: String,
}

impl TagTemplate {
    pub fn parse(value: &str) -> Result<Self> {
        let canonical = normalize_tag_template(value)?;
        let token_index = canonical
            .find(VERSION_TOKEN)
            .expect("normalized template always includes VERSION_TOKEN exactly once");
        let prefix = canonical[..token_index].to_string();
        let suffix = canonical[token_index + VERSION_TOKEN.len()..].to_string();
        Ok(Self {
            canonical,
            prefix,
            suffix,
        })
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub fn suffix(&self) -> &str {
        &self.suffix
    }

    pub fn render(&self, version: &str) -> String {
        format!("{}{}{}", self.prefix, version, self.suffix)
    }

    pub fn parse_stable_version(&self, raw_tag: &str) -> Option<Version> {
        let tag = raw_tag.trim();
        if !tag.starts_with(&self.prefix) || !tag.ends_with(&self.suffix) {
            return None;
        }
        if tag.len() < self.prefix.len() + self.suffix.len() {
            return None;
        }

        let version_segment = &tag[self.prefix.len()..tag.len() - self.suffix.len()];
        let version = Version::parse(version_segment).ok()?;
        if !version.pre.is_empty() || !version.build.is_empty() {
            return None;
        }
        Some(version)
    }
}

pub fn normalize_tag_template(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("Tag template cannot be empty.");
    }

    let canonical = trimmed.replace(LEGACY_VERSION_TOKEN, VERSION_TOKEN);
    let token_count = canonical.match_indices(VERSION_TOKEN).count();
    if token_count != 1 {
        bail!(
            "Tag template must include exactly one `{}` token.",
            VERSION_TOKEN
        );
    }

    Ok(canonical)
}

pub fn shell_escape_single(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':'))
    {
        return value.to_string();
    }

    let escaped = value.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_legacy_double_brace_token() {
        assert_eq!(
            normalize_tag_template("v{{version}}").unwrap(),
            "v{version}".to_string()
        );
    }

    #[test]
    fn rejects_templates_without_single_token() {
        assert!(normalize_tag_template("release").is_err());
        assert!(normalize_tag_template("{version}-{version}").is_err());
    }

    #[test]
    fn renders_and_parses_stable_versions() {
        let template = TagTemplate::parse("release-{version}").unwrap();
        assert_eq!(template.render("1.2.3"), "release-1.2.3");
        assert_eq!(
            template.parse_stable_version("release-1.2.3"),
            Some(Version::new(1, 2, 3))
        );
        assert!(
            template
                .parse_stable_version("release-1.2.3-rc.1")
                .is_none()
        );
    }

    #[test]
    fn shell_escape_wraps_non_safe_values() {
        assert_eq!(shell_escape_single(""), "''");
        assert_eq!(shell_escape_single("safe-value"), "safe-value");
        assert_eq!(shell_escape_single("has space"), "'has space'");
    }
}
