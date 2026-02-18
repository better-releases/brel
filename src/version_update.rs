use crate::config::VersionFileFormat;
use crate::version_selector::{SegmentQualifier, VersionSelector, parse_selector};
use anyhow::{Context, Result, bail};
use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use toml::Value as TomlValue;
use toml_edit::{DocumentMut, Item, Table, Value as TomlEditValue};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateReport {
    pub changed_files: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PathStep {
    Key(String),
    Index(usize),
}

pub fn apply_version_updates(
    repo_root: &Path,
    next_version: &str,
    version_updates: &BTreeMap<String, Vec<String>>,
    format_overrides: &BTreeMap<String, VersionFileFormat>,
) -> Result<UpdateReport> {
    let mut changed_files = Vec::new();

    for (relative_path, selectors) in version_updates {
        let file_path = repo_root.join(relative_path);
        if !file_path.exists() {
            bail!("Configured version update file `{relative_path}` was not found.");
        }

        let format =
            detect_file_format(relative_path, format_overrides.get(relative_path).copied())?;
        let content = fs::read_to_string(&file_path)
            .with_context(|| format!("Failed to read `{}`.", file_path.display()))?;

        let parsed_selectors = parse_selectors(selectors, &file_path)?;
        let changed = match format {
            VersionFileFormat::Json => {
                update_json_file(&file_path, &content, &parsed_selectors, next_version)?
            }
            VersionFileFormat::Toml => {
                update_toml_file(&file_path, &content, &parsed_selectors, next_version)?
            }
        };

        if changed {
            changed_files.push(PathBuf::from(relative_path));
        }
    }

    Ok(UpdateReport { changed_files })
}

fn parse_selectors(
    selectors: &[String],
    file_path: &Path,
) -> Result<Vec<(String, VersionSelector)>> {
    let mut parsed = Vec::with_capacity(selectors.len());
    for raw_selector in selectors {
        let selector_text = raw_selector.trim();
        let selector = parse_selector(selector_text).with_context(|| {
            format!(
                "Invalid version selector `{selector_text}` while updating `{}`.",
                file_path.display()
            )
        })?;
        parsed.push((selector_text.to_string(), selector));
    }
    Ok(parsed)
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
    selectors: &[(String, VersionSelector)],
    next_version: &str,
) -> Result<bool> {
    let mut value: JsonValue = serde_json::from_str(content)
        .with_context(|| format!("Failed to parse JSON file `{}`.", file_path.display()))?;

    let mut changed = false;
    for (selector_text, selector) in selectors {
        let target_paths = resolve_json_paths(&value, selector_text, selector, file_path)?;
        for path in &target_paths {
            changed |=
                set_json_string_at_path(&mut value, path, next_version, selector_text, file_path)
                    .with_context(|| {
                    format!(
                        "While updating selector `{selector_text}` in `{}`.",
                        file_path.display()
                    )
                })?;
        }
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

fn resolve_json_paths(
    root: &JsonValue,
    selector_text: &str,
    selector: &VersionSelector,
    file_path: &Path,
) -> Result<Vec<Vec<PathStep>>> {
    let mut current_paths = vec![Vec::new()];

    for segment in &selector.segments {
        let mut next_paths = BTreeSet::new();

        for current_path in &current_paths {
            let Some(node) = json_value_at_path(root, current_path) else {
                continue;
            };

            let Some(child) = node.as_object().and_then(|object| object.get(&segment.key)) else {
                continue;
            };

            let mut child_path = current_path.clone();
            child_path.push(PathStep::Key(segment.key.clone()));

            match &segment.qualifier {
                None => {
                    next_paths.insert(child_path);
                }
                Some(SegmentQualifier::Index(index)) => {
                    let Some(array) = child.as_array() else {
                        bail!(
                            "Selector `{selector_text}` expects segment `{}` to be an array in `{}`.",
                            segment.key,
                            file_path.display()
                        );
                    };

                    if array.get(*index).is_some() {
                        let mut indexed_path = child_path;
                        indexed_path.push(PathStep::Index(*index));
                        next_paths.insert(indexed_path);
                    }
                }
                Some(SegmentQualifier::Filter { field, value }) => {
                    let Some(array) = child.as_array() else {
                        bail!(
                            "Selector `{selector_text}` expects segment `{}` to be an array in `{}`.",
                            segment.key,
                            file_path.display()
                        );
                    };

                    for (idx, element) in array.iter().enumerate() {
                        let Some(object) = element.as_object() else {
                            bail!(
                                "Selector `{selector_text}` expects all elements under `{}` to be JSON objects in `{}`.",
                                segment.key,
                                file_path.display()
                            );
                        };

                        let Some(field_value) = object.get(field) else {
                            continue;
                        };

                        let Some(actual_value) = field_value.as_str() else {
                            bail!(
                                "Selector `{selector_text}` expects filter field `{field}` to be a string in `{}`.",
                                file_path.display()
                            );
                        };

                        if actual_value == value {
                            let mut indexed_path = child_path.clone();
                            indexed_path.push(PathStep::Index(idx));
                            next_paths.insert(indexed_path);
                        }
                    }
                }
            }
        }

        current_paths = next_paths.into_iter().collect();
    }

    if current_paths.is_empty() {
        bail!(
            "Selector `{selector_text}` matched no values in `{}`.",
            file_path.display()
        );
    }

    Ok(current_paths)
}

fn json_value_at_path<'a>(root: &'a JsonValue, path: &[PathStep]) -> Option<&'a JsonValue> {
    let mut current = root;
    for step in path {
        match step {
            PathStep::Key(key) => {
                current = current.as_object()?.get(key)?;
            }
            PathStep::Index(index) => {
                current = current.as_array()?.get(*index)?;
            }
        }
    }

    Some(current)
}

fn set_json_string_at_path(
    root: &mut JsonValue,
    path: &[PathStep],
    next_version: &str,
    selector_text: &str,
    file_path: &Path,
) -> Result<bool> {
    let mut current = root;
    for step in path {
        match step {
            PathStep::Key(key) => {
                let object = current.as_object_mut().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Internal selector path resolution error for `{selector_text}` in `{}`.",
                        file_path.display()
                    )
                })?;
                current = object.get_mut(key).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Internal selector path resolution error for `{selector_text}` in `{}`.",
                        file_path.display()
                    )
                })?;
            }
            PathStep::Index(index) => {
                let array = current.as_array_mut().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Internal selector path resolution error for `{selector_text}` in `{}`.",
                        file_path.display()
                    )
                })?;
                current = array.get_mut(*index).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Internal selector path resolution error for `{selector_text}` in `{}`.",
                        file_path.display()
                    )
                })?;
            }
        }
    }

    let Some(existing_value) = current.as_str() else {
        bail!(
            "Selector `{selector_text}` matched a non-string JSON value in `{}`.",
            file_path.display()
        );
    };

    let changed = existing_value != next_version;
    if changed {
        *current = JsonValue::String(next_version.to_string());
    }

    Ok(changed)
}

fn update_toml_file(
    file_path: &Path,
    content: &str,
    selectors: &[(String, VersionSelector)],
    next_version: &str,
) -> Result<bool> {
    let source_value: TomlValue = content
        .parse()
        .with_context(|| format!("Failed to parse TOML file `{}`.", file_path.display()))?;
    let mut document = content
        .parse::<DocumentMut>()
        .with_context(|| format!("Failed to parse TOML file `{}`.", file_path.display()))?;

    let mut changed = false;
    for (selector_text, selector) in selectors {
        let target_paths = resolve_toml_paths(&source_value, selector_text, selector, file_path)?;
        for path in &target_paths {
            changed |= set_toml_string_at_path(
                document.as_item_mut(),
                path,
                next_version,
                selector_text,
                file_path,
            )
            .with_context(|| {
                format!(
                    "While updating selector `{selector_text}` in `{}`.",
                    file_path.display()
                )
            })?;
        }
    }

    if !changed {
        return Ok(false);
    }

    let mut output = document.to_string();
    if !output.ends_with('\n') {
        output.push('\n');
    }
    fs::write(file_path, output)
        .with_context(|| format!("Failed to write `{}`.", file_path.display()))?;
    Ok(true)
}

fn resolve_toml_paths(
    root: &TomlValue,
    selector_text: &str,
    selector: &VersionSelector,
    file_path: &Path,
) -> Result<Vec<Vec<PathStep>>> {
    let mut current_paths = vec![Vec::new()];

    for segment in &selector.segments {
        let mut next_paths = BTreeSet::new();

        for current_path in &current_paths {
            let Some(node) = toml_value_at_path(root, current_path) else {
                continue;
            };

            let Some(child) = node.as_table().and_then(|table| table.get(&segment.key)) else {
                continue;
            };

            let mut child_path = current_path.clone();
            child_path.push(PathStep::Key(segment.key.clone()));

            match &segment.qualifier {
                None => {
                    next_paths.insert(child_path);
                }
                Some(SegmentQualifier::Index(index)) => {
                    let Some(array) = child.as_array() else {
                        bail!(
                            "Selector `{selector_text}` expects segment `{}` to be an array in `{}`.",
                            segment.key,
                            file_path.display()
                        );
                    };

                    if array.get(*index).is_some() {
                        let mut indexed_path = child_path;
                        indexed_path.push(PathStep::Index(*index));
                        next_paths.insert(indexed_path);
                    }
                }
                Some(SegmentQualifier::Filter { field, value }) => {
                    let Some(array) = child.as_array() else {
                        bail!(
                            "Selector `{selector_text}` expects segment `{}` to be an array in `{}`.",
                            segment.key,
                            file_path.display()
                        );
                    };

                    for (idx, element) in array.iter().enumerate() {
                        let Some(table) = element.as_table() else {
                            bail!(
                                "Selector `{selector_text}` expects all elements under `{}` to be TOML tables in `{}`.",
                                segment.key,
                                file_path.display()
                            );
                        };

                        let Some(field_value) = table.get(field) else {
                            continue;
                        };

                        let Some(actual_value) = field_value.as_str() else {
                            bail!(
                                "Selector `{selector_text}` expects filter field `{field}` to be a string in `{}`.",
                                file_path.display()
                            );
                        };

                        if actual_value == value {
                            let mut indexed_path = child_path.clone();
                            indexed_path.push(PathStep::Index(idx));
                            next_paths.insert(indexed_path);
                        }
                    }
                }
            }
        }

        current_paths = next_paths.into_iter().collect();
    }

    if current_paths.is_empty() {
        bail!(
            "Selector `{selector_text}` matched no values in `{}`.",
            file_path.display()
        );
    }

    Ok(current_paths)
}

fn toml_value_at_path<'a>(root: &'a TomlValue, path: &[PathStep]) -> Option<&'a TomlValue> {
    let mut current = root;
    for step in path {
        match step {
            PathStep::Key(key) => {
                current = current.as_table()?.get(key)?;
            }
            PathStep::Index(index) => {
                current = current.as_array()?.get(*index)?;
            }
        }
    }

    Some(current)
}

fn set_toml_string_at_path(
    root: &mut Item,
    path: &[PathStep],
    next_version: &str,
    selector_text: &str,
    file_path: &Path,
) -> Result<bool> {
    set_toml_string_in_item(root, path, next_version, selector_text, file_path)
}

fn set_toml_string_in_item(
    item: &mut Item,
    path: &[PathStep],
    next_version: &str,
    selector_text: &str,
    file_path: &Path,
) -> Result<bool> {
    if path.is_empty() {
        return match item {
            Item::Value(value) => {
                set_toml_string_in_value(value, &[], next_version, selector_text, file_path)
            }
            _ => bail!(
                "Selector `{selector_text}` matched a non-string TOML value in `{}`.",
                file_path.display()
            ),
        };
    }

    match &path[0] {
        PathStep::Key(key) => match item {
            Item::Table(table) => {
                let child = table.get_mut(key).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Internal selector path resolution error for `{selector_text}` in `{}`.",
                        file_path.display()
                    )
                })?;
                set_toml_string_in_item(child, &path[1..], next_version, selector_text, file_path)
            }
            Item::Value(TomlEditValue::InlineTable(table)) => {
                let child = table.get_mut(key).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Internal selector path resolution error for `{selector_text}` in `{}`.",
                        file_path.display()
                    )
                })?;
                set_toml_string_in_value(child, &path[1..], next_version, selector_text, file_path)
            }
            _ => bail!(
                "Internal selector path resolution error for `{selector_text}` in `{}`.",
                file_path.display()
            ),
        },
        PathStep::Index(index) => match item {
            Item::ArrayOfTables(array) => {
                let child = array.get_mut(*index).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Internal selector path resolution error for `{selector_text}` in `{}`.",
                        file_path.display()
                    )
                })?;
                set_toml_string_in_table(child, &path[1..], next_version, selector_text, file_path)
            }
            Item::Value(TomlEditValue::Array(array)) => {
                let child = array.get_mut(*index).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Internal selector path resolution error for `{selector_text}` in `{}`.",
                        file_path.display()
                    )
                })?;
                set_toml_string_in_value(child, &path[1..], next_version, selector_text, file_path)
            }
            _ => bail!(
                "Internal selector path resolution error for `{selector_text}` in `{}`.",
                file_path.display()
            ),
        },
    }
}

fn set_toml_string_in_table(
    table: &mut Table,
    path: &[PathStep],
    next_version: &str,
    selector_text: &str,
    file_path: &Path,
) -> Result<bool> {
    if path.is_empty() {
        bail!(
            "Selector `{selector_text}` matched a non-string TOML value in `{}`.",
            file_path.display()
        );
    }

    match &path[0] {
        PathStep::Key(key) => {
            let child = table.get_mut(key).ok_or_else(|| {
                anyhow::anyhow!(
                    "Internal selector path resolution error for `{selector_text}` in `{}`.",
                    file_path.display()
                )
            })?;
            set_toml_string_in_item(child, &path[1..], next_version, selector_text, file_path)
        }
        PathStep::Index(_) => bail!(
            "Internal selector path resolution error for `{selector_text}` in `{}`.",
            file_path.display()
        ),
    }
}

fn set_toml_string_in_value(
    value: &mut TomlEditValue,
    path: &[PathStep],
    next_version: &str,
    selector_text: &str,
    file_path: &Path,
) -> Result<bool> {
    if path.is_empty() {
        let Some(existing_value) = value.as_str() else {
            bail!(
                "Selector `{selector_text}` matched a non-string TOML value in `{}`.",
                file_path.display()
            );
        };

        let changed = existing_value != next_version;
        if changed {
            *value = TomlEditValue::from(next_version);
        }
        return Ok(changed);
    }

    match &path[0] {
        PathStep::Key(key) => {
            let TomlEditValue::InlineTable(table) = value else {
                bail!(
                    "Internal selector path resolution error for `{selector_text}` in `{}`.",
                    file_path.display()
                );
            };
            let child = table.get_mut(key).ok_or_else(|| {
                anyhow::anyhow!(
                    "Internal selector path resolution error for `{selector_text}` in `{}`.",
                    file_path.display()
                )
            })?;
            set_toml_string_in_value(child, &path[1..], next_version, selector_text, file_path)
        }
        PathStep::Index(index) => {
            let TomlEditValue::Array(array) = value else {
                bail!(
                    "Internal selector path resolution error for `{selector_text}` in `{}`.",
                    file_path.display()
                );
            };
            let child = array.get_mut(*index).ok_or_else(|| {
                anyhow::anyhow!(
                    "Internal selector path resolution error for `{selector_text}` in `{}`.",
                    file_path.display()
                )
            })?;
            set_toml_string_in_value(child, &path[1..], next_version, selector_text, file_path)
        }
    }
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
    fn updates_json_indexed_value() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.json");
        fs::write(
            &file_path,
            "{\n  \"packages\": [\n    {\"name\": \"a\", \"version\": \"1.0.0\"},\n    {\"name\": \"b\", \"version\": \"2.0.0\"}\n  ]\n}\n",
        )
        .unwrap();

        let mut updates = BTreeMap::new();
        updates.insert(
            "package.json".to_string(),
            vec!["packages[1].version".to_string()],
        );

        let report =
            apply_version_updates(temp_dir.path(), "9.9.9", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("package.json")]);
        let content = fs::read_to_string(file_path).unwrap();
        assert!(content.contains("\"name\": \"a\",\n      \"version\": \"1.0.0\""));
        assert!(content.contains("\"name\": \"b\",\n      \"version\": \"9.9.9\""));
    }

    #[test]
    fn updates_all_json_filter_matches() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.json");
        fs::write(
            &file_path,
            "{\n  \"package\": [\n    {\"name\": \"brel\", \"version\": \"1.0.0\"},\n    {\"name\": \"other\", \"version\": \"2.0.0\"},\n    {\"name\": \"brel\", \"version\": \"3.0.0\"}\n  ]\n}\n",
        )
        .unwrap();

        let mut updates = BTreeMap::new();
        updates.insert(
            "package.json".to_string(),
            vec!["package[name=brel].version".to_string()],
        );

        let report =
            apply_version_updates(temp_dir.path(), "7.7.7", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("package.json")]);
        let content = fs::read_to_string(file_path).unwrap();
        assert_eq!(content.matches("\"version\": \"7.7.7\"").count(), 2);
        assert!(content.contains("\"version\": \"2.0.0\""));
    }

    #[test]
    fn updates_nested_toml_key_without_reformatting() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("Cargo.toml");
        fs::write(
            &file_path,
            "[package]\n# Keep this comment\nname = \"demo\"\nversion = \"1.0.0\"\n",
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
        assert!(content.contains("# Keep this comment"));
        assert!(content.contains("version = \"1.1.0\""));
    }

    #[test]
    fn updates_cargo_lock_style_selector() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("Cargo.lock");
        fs::write(
            &file_path,
            "version = 4\n\n[[package]]\nname = \"dep\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"brel\"\nversion = \"0.2.0\"\n",
        )
        .unwrap();

        let mut updates = BTreeMap::new();
        updates.insert(
            "Cargo.lock".to_string(),
            vec!["package[name=brel].version".to_string()],
        );

        let mut overrides = BTreeMap::new();
        overrides.insert("Cargo.lock".to_string(), VersionFileFormat::Toml);

        let report = apply_version_updates(temp_dir.path(), "0.3.0", &updates, &overrides).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("Cargo.lock")]);
        let content = fs::read_to_string(file_path).unwrap();
        assert!(content.contains("name = \"dep\"\nversion = \"0.1.0\""));
        assert!(content.contains("name = \"brel\"\nversion = \"0.3.0\""));
    }

    #[test]
    fn fails_when_selector_matches_no_values() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.json");
        fs::write(&file_path, "{ \"name\": \"demo\" }\n").unwrap();

        let mut updates = BTreeMap::new();
        updates.insert("package.json".to_string(), vec!["version".to_string()]);

        let err = apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new())
            .unwrap_err();
        assert!(err.to_string().contains("matched no values"));
    }

    #[test]
    fn fails_when_json_target_is_not_string() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.json");
        fs::write(&file_path, "{ \"version\": {\"major\": 1} }\n").unwrap();

        let mut updates = BTreeMap::new();
        updates.insert("package.json".to_string(), vec!["version".to_string()]);

        let err = apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new())
            .unwrap_err();
        let err_text = format!("{err:#}");
        assert!(err_text.contains("non-string JSON value"));
    }

    #[test]
    fn fails_when_toml_target_is_not_string() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("Cargo.toml");
        fs::write(&file_path, "[package]\nversion = 1\n").unwrap();

        let mut updates = BTreeMap::new();
        updates.insert(
            "Cargo.toml".to_string(),
            vec!["package.version".to_string()],
        );

        let err = apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new())
            .unwrap_err();
        let err_text = format!("{err:#}");
        assert!(err_text.contains("non-string TOML value"));
    }

    #[test]
    fn fails_when_filter_is_applied_to_non_array() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.json");
        fs::write(
            &file_path,
            "{\n  \"package\": {\"name\": \"brel\", \"version\": \"1.0.0\"}\n}\n",
        )
        .unwrap();

        let mut updates = BTreeMap::new();
        updates.insert(
            "package.json".to_string(),
            vec!["package[name=brel].version".to_string()],
        );

        let err = apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new())
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("expects segment `package` to be an array")
        );
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
}
