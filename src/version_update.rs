use crate::config::VersionFileFormat;
use anyhow::{Context, Result, bail};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use toml::Value as TomlValue;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateReport {
    pub changed_files: Vec<PathBuf>,
}

pub fn apply_version_updates(
    repo_root: &Path,
    next_version: &str,
    version_updates: &BTreeMap<String, Vec<String>>,
    format_overrides: &BTreeMap<String, VersionFileFormat>,
) -> Result<UpdateReport> {
    let mut changed_files = Vec::new();

    for (relative_path, keys) in version_updates {
        let file_path = repo_root.join(relative_path);
        if !file_path.exists() {
            bail!("Configured version update file `{relative_path}` was not found.");
        }

        let format =
            detect_file_format(relative_path, format_overrides.get(relative_path).copied())?;
        let content = fs::read_to_string(&file_path)
            .with_context(|| format!("Failed to read `{}`.", file_path.display()))?;

        let changed = match format {
            VersionFileFormat::Json => update_json_file(&file_path, &content, keys, next_version)?,
            VersionFileFormat::Toml => update_toml_file(&file_path, &content, keys, next_version)?,
        };

        if changed {
            changed_files.push(PathBuf::from(relative_path));
        }
    }

    Ok(UpdateReport { changed_files })
}

fn detect_file_format(
    relative_path: &str,
    override_format: Option<VersionFileFormat>,
) -> Result<VersionFileFormat> {
    if let Some(explicit) = override_format {
        return Ok(explicit);
    }

    match Path::new(relative_path)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("json") => Ok(VersionFileFormat::Json),
        Some("toml") => Ok(VersionFileFormat::Toml),
        _ => bail!(
            "Cannot infer file format for `{relative_path}`. Use `release_pr.format_overrides` \
             with `json` or `toml`."
        ),
    }
}

fn update_json_file(
    file_path: &Path,
    content: &str,
    keys: &[String],
    next_version: &str,
) -> Result<bool> {
    let mut value: JsonValue = serde_json::from_str(content)
        .with_context(|| format!("Failed to parse JSON file `{}`.", file_path.display()))?;

    let mut changed = false;
    for key_path in keys {
        changed |= set_json_dot_path(&mut value, key_path, next_version).with_context(|| {
            format!(
                "While updating `{}` in `{}`.",
                key_path,
                file_path.display()
            )
        })?;
    }

    if !changed {
        return Ok(false);
    }

    let mut output = serde_json::to_string_pretty(&value)
        .with_context(|| format!("Failed to serialize JSON file `{}`.", file_path.display()))?;
    output.push('\n');
    fs::write(file_path, output)
        .with_context(|| format!("Failed to write `{}`.", file_path.display()))?;
    Ok(true)
}

fn set_json_dot_path(root: &mut JsonValue, path: &str, next_version: &str) -> Result<bool> {
    let mut segments = path.split('.').peekable();
    let mut current = root;

    while let Some(segment) = segments.next() {
        let is_last = segments.peek().is_none();
        let object = current
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("Expected `{segment}` parent to be a JSON object."))?;

        let value = object
            .get_mut(segment)
            .ok_or_else(|| anyhow::anyhow!("Key `{segment}` does not exist."))?;

        if is_last {
            if value.is_array() || value.is_object() {
                bail!("Target `{path}` must point to a scalar JSON value.");
            }
            let changed = !matches!(value, JsonValue::String(existing) if existing == next_version);
            *value = JsonValue::String(next_version.to_string());
            return Ok(changed);
        }

        current = value;
    }

    bail!("Version key path cannot be empty.")
}

fn update_toml_file(
    file_path: &Path,
    content: &str,
    keys: &[String],
    next_version: &str,
) -> Result<bool> {
    let mut value: TomlValue = content
        .parse()
        .with_context(|| format!("Failed to parse TOML file `{}`.", file_path.display()))?;

    let mut changed = false;
    for key_path in keys {
        changed |= set_toml_dot_path(&mut value, key_path, next_version).with_context(|| {
            format!(
                "While updating `{}` in `{}`.",
                key_path,
                file_path.display()
            )
        })?;
    }

    if !changed {
        return Ok(false);
    }

    let mut output = toml::to_string_pretty(&value)
        .with_context(|| format!("Failed to serialize TOML file `{}`.", file_path.display()))?;
    output.push('\n');
    fs::write(file_path, output)
        .with_context(|| format!("Failed to write `{}`.", file_path.display()))?;
    Ok(true)
}

fn set_toml_dot_path(root: &mut TomlValue, path: &str, next_version: &str) -> Result<bool> {
    let mut segments = path.split('.').peekable();
    let mut current = root;

    while let Some(segment) = segments.next() {
        let is_last = segments.peek().is_none();
        let table = current
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("Expected `{segment}` parent to be a TOML table."))?;

        let value = table
            .get_mut(segment)
            .ok_or_else(|| anyhow::anyhow!("Key `{segment}` does not exist."))?;

        if is_last {
            if value.is_array() || value.is_table() {
                bail!("Target `{path}` must point to a scalar TOML value.");
            }
            let changed = !matches!(value.as_str(), Some(existing) if existing == next_version);
            *value = TomlValue::String(next_version.to_string());
            return Ok(changed);
        }

        current = value;
    }

    bail!("Version key path cannot be empty.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn updates_nested_json_key() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.json");
        fs::write(
            &file_path,
            "{\n  \"package\": {\"version\": \"1.0.0\"},\n  \"name\": \"demo\"\n}\n",
        )
        .unwrap();

        let mut updates = BTreeMap::new();
        updates.insert(
            "package.json".to_string(),
            vec!["package.version".to_string()],
        );
        let report =
            apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("package.json")]);
        let content = fs::read_to_string(file_path).unwrap();
        assert!(content.contains("\"version\": \"1.1.0\""));
    }

    #[test]
    fn updates_nested_toml_key() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("Cargo.toml");
        fs::write(
            &file_path,
            "[package]\nname = \"demo\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();

        let mut updates = BTreeMap::new();
        updates.insert(
            "Cargo.toml".to_string(),
            vec!["package.version".to_string()],
        );
        let report =
            apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("Cargo.toml")]);
        let content = fs::read_to_string(file_path).unwrap();
        assert!(content.contains("version = \"1.1.0\""));
    }

    #[test]
    fn fails_when_file_missing() {
        let temp_dir = tempdir().unwrap();
        let mut updates = BTreeMap::new();
        updates.insert("missing.json".to_string(), vec!["version".to_string()]);

        let err = apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new())
            .unwrap_err();
        assert!(err.to_string().contains("was not found"));
    }

    #[test]
    fn fails_when_key_missing() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.json");
        fs::write(&file_path, "{ \"name\": \"demo\" }\n").unwrap();

        let mut updates = BTreeMap::new();
        updates.insert("package.json".to_string(), vec!["version".to_string()]);

        let err = apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new())
            .unwrap_err();
        let err_text = format!("{err:#}");
        assert!(err_text.contains("Key `version` does not exist"));
    }

    #[test]
    fn fails_when_target_is_not_scalar() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.json");
        fs::write(&file_path, "{ \"version\": {\"major\": 1} }\n").unwrap();

        let mut updates = BTreeMap::new();
        updates.insert("package.json".to_string(), vec!["version".to_string()]);

        let err = apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new())
            .unwrap_err();
        let err_text = format!("{err:#}");
        assert!(err_text.contains("must point to a scalar"));
    }
}
