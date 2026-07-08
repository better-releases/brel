use crate::config::VersionFileFormat;
use crate::version_selector::{SegmentQualifier, VersionSelector, parse_selector};
use anyhow::{Context, Result, bail};
use saphyr::{MarkedYaml, ScalarStyle, YamlData, YamlLoader};
use saphyr_parser::Parser;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceSpan {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum JsonNode {
    Object(JsonObject),
    Array(JsonArray),
    String(JsonString),
    Number(SourceSpan),
    Bool(SourceSpan),
    Null(SourceSpan),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JsonObject {
    entries: Vec<JsonObjectEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JsonObjectEntry {
    key: String,
    value: JsonNode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JsonArray {
    elements: Vec<JsonNode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JsonString {
    span: SourceSpan,
    value: String,
}

impl JsonNode {
    fn as_object(&self) -> Option<&JsonObject> {
        match self {
            Self::Object(object) => Some(object),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&JsonArray> {
        match self {
            Self::Array(array) => Some(array),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(&value.value),
            _ => None,
        }
    }

    fn string_span(&self) -> Option<SourceSpan> {
        match self {
            Self::String(value) => Some(value.span),
            _ => None,
        }
    }
}

impl JsonObject {
    fn get(&self, key: &str) -> Option<&JsonNode> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.key == key)
            .map(|entry| &entry.value)
    }
}

struct JsonParser<'a> {
    input: &'a str,
    position: usize,
}

impl<'a> JsonParser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, position: 0 }
    }

    fn parse_document(mut self) -> Result<JsonNode> {
        self.skip_whitespace();
        let value = self.parse_value()?;
        self.skip_whitespace();

        if self.position != self.input.len() {
            bail!("Unexpected trailing content at byte {}.", self.position);
        }

        Ok(value)
    }

    fn parse_value(&mut self) -> Result<JsonNode> {
        self.skip_whitespace();

        match self.peek_byte() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => self.parse_string().map(JsonNode::String),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            Some(b't') => self.parse_keyword("true", JsonNode::Bool),
            Some(b'f') => self.parse_keyword("false", JsonNode::Bool),
            Some(b'n') => self.parse_keyword("null", JsonNode::Null),
            Some(_) => bail!("Unexpected character at byte {}.", self.position),
            None => bail!("Unexpected end of JSON input."),
        }
    }

    fn parse_object(&mut self) -> Result<JsonNode> {
        self.expect_byte(b'{', "`{`")?;
        self.skip_whitespace();

        let mut entries = Vec::new();
        if self.consume_byte_if(b'}') {
            return Ok(JsonNode::Object(JsonObject { entries }));
        }

        loop {
            self.skip_whitespace();
            let key = self.parse_string()?;
            self.skip_whitespace();
            self.expect_byte(b':', "`:`")?;
            let value = self.parse_value()?;
            entries.push(JsonObjectEntry {
                key: key.value,
                value,
            });

            self.skip_whitespace();
            if self.consume_byte_if(b',') {
                continue;
            }

            self.expect_byte(b'}', "`}`")?;
            return Ok(JsonNode::Object(JsonObject { entries }));
        }
    }

    fn parse_array(&mut self) -> Result<JsonNode> {
        self.expect_byte(b'[', "`[`")?;
        self.skip_whitespace();

        let mut elements = Vec::new();
        if self.consume_byte_if(b']') {
            return Ok(JsonNode::Array(JsonArray { elements }));
        }

        loop {
            let value = self.parse_value()?;
            elements.push(value);

            self.skip_whitespace();
            if self.consume_byte_if(b',') {
                continue;
            }

            self.expect_byte(b']', "`]`")?;
            return Ok(JsonNode::Array(JsonArray { elements }));
        }
    }

    fn parse_string(&mut self) -> Result<JsonString> {
        let start = self.position;
        self.expect_byte(b'"', "string opening quote")?;

        while let Some(byte) = self.peek_byte() {
            match byte {
                b'"' => {
                    self.position += 1;
                    let raw = &self.input[start..self.position];
                    let value = serde_json::from_str(raw)
                        .with_context(|| format!("Invalid JSON string literal at byte {start}."))?;

                    return Ok(JsonString {
                        span: SourceSpan {
                            start,
                            end: self.position,
                        },
                        value,
                    });
                }
                b'\\' => self.parse_escape_sequence()?,
                0x00..=0x1f => {
                    bail!(
                        "Invalid control character in JSON string at byte {}.",
                        self.position
                    )
                }
                _ => {
                    self.position += 1;
                }
            }
        }

        bail!("Unterminated JSON string starting at byte {start}.")
    }

    fn parse_escape_sequence(&mut self) -> Result<()> {
        self.expect_byte(b'\\', "escape sequence")?;
        let Some(escaped) = self.peek_byte() else {
            bail!("Unterminated JSON escape sequence at end of input.");
        };

        match escaped {
            b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {
                self.position += 1;
            }
            b'u' => {
                self.position += 1;
                for _ in 0..4 {
                    let Some(hex) = self.peek_byte() else {
                        bail!("Incomplete unicode escape sequence at end of input.");
                    };
                    if !hex.is_ascii_hexdigit() {
                        bail!("Invalid unicode escape sequence at byte {}.", self.position);
                    }
                    self.position += 1;
                }
            }
            _ => bail!("Invalid escape sequence at byte {}.", self.position),
        }

        Ok(())
    }

    fn parse_number(&mut self) -> Result<JsonNode> {
        let start = self.position;

        if self.consume_byte_if(b'-') && !matches!(self.peek_byte(), Some(b'0'..=b'9')) {
            bail!("Invalid number at byte {start}.");
        }

        match self.peek_byte() {
            Some(b'0') => {
                self.position += 1;
            }
            Some(b'1'..=b'9') => {
                self.position += 1;
                while matches!(self.peek_byte(), Some(b'0'..=b'9')) {
                    self.position += 1;
                }
            }
            _ => bail!("Invalid number at byte {start}."),
        }

        if self.consume_byte_if(b'.') {
            if !matches!(self.peek_byte(), Some(b'0'..=b'9')) {
                bail!("Invalid number at byte {start}.");
            }

            while matches!(self.peek_byte(), Some(b'0'..=b'9')) {
                self.position += 1;
            }
        }

        if matches!(self.peek_byte(), Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.peek_byte(), Some(b'+' | b'-')) {
                self.position += 1;
            }

            if !matches!(self.peek_byte(), Some(b'0'..=b'9')) {
                bail!("Invalid number at byte {start}.");
            }

            while matches!(self.peek_byte(), Some(b'0'..=b'9')) {
                self.position += 1;
            }
        }

        Ok(JsonNode::Number(SourceSpan {
            start,
            end: self.position,
        }))
    }

    fn parse_keyword<F>(&mut self, keyword: &str, constructor: F) -> Result<JsonNode>
    where
        F: FnOnce(SourceSpan) -> JsonNode,
    {
        let start = self.position;
        if !self
            .input
            .get(self.position..)
            .is_some_and(|tail| tail.starts_with(keyword))
        {
            bail!("Unexpected token at byte {start}.");
        }

        self.position += keyword.len();
        Ok(constructor(SourceSpan {
            start,
            end: self.position,
        }))
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek_byte(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.position += 1;
        }
    }

    fn expect_byte(&mut self, expected: u8, label: &str) -> Result<()> {
        match self.peek_byte() {
            Some(byte) if byte == expected => {
                self.position += 1;
                Ok(())
            }
            Some(_) => bail!("Expected {label} at byte {}.", self.position),
            None => bail!("Expected {label} at end of input."),
        }
    }

    fn consume_byte_if(&mut self, expected: u8) -> bool {
        if self.peek_byte() == Some(expected) {
            self.position += 1;
            return true;
        }

        false
    }

    fn peek_byte(&self) -> Option<u8> {
        self.input.as_bytes().get(self.position).copied()
    }
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
            VersionFileFormat::Yaml => {
                update_yaml_file(&file_path, &content, &parsed_selectors, next_version)?
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
        Some("yaml" | "yml") => Ok(VersionFileFormat::Yaml),
        _ => bail!(
            "Cannot infer file format for `{relative_path}`. Use `release_pr.format_overrides` \
             with `json`, `toml`, or `yaml`."
        ),
    }
}

fn update_json_file(
    file_path: &Path,
    content: &str,
    selectors: &[(String, VersionSelector)],
    next_version: &str,
) -> Result<bool> {
    let value = JsonParser::new(content)
        .parse_document()
        .with_context(|| format!("Failed to parse JSON file `{}`.", file_path.display()))?;
    let replacement_literal = serde_json::to_string(next_version).with_context(|| {
        format!(
            "Failed to serialize JSON value for `{}`.",
            file_path.display()
        )
    })?;
    let mut replacements: BTreeMap<usize, (SourceSpan, String)> = BTreeMap::new();

    for (selector_text, selector) in selectors {
        let target_paths = resolve_json_paths(&value, selector_text, selector, file_path)?;
        for path in &target_paths {
            if let Some(span) =
                json_string_span_at_path(&value, path, next_version, selector_text, file_path)
                    .with_context(|| {
                        format!(
                            "While updating selector `{selector_text}` in `{}`.",
                            file_path.display()
                        )
                    })?
            {
                replacements
                    .entry(span.start)
                    .or_insert_with(|| (span, replacement_literal.clone()));
            }
        }
    }

    if replacements.is_empty() {
        return Ok(false);
    }

    let mut output = content.to_string();
    for (_, (span, replacement)) in replacements.iter().rev() {
        output.replace_range(span.start..span.end, replacement);
    }

    fs::write(file_path, output)
        .with_context(|| format!("Failed to write `{}`.", file_path.display()))?;
    Ok(true)
}

fn resolve_json_paths(
    root: &JsonNode,
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

                    if array.elements.get(*index).is_some() {
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

                    for (idx, element) in array.elements.iter().enumerate() {
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

fn json_value_at_path<'a>(root: &'a JsonNode, path: &[PathStep]) -> Option<&'a JsonNode> {
    let mut current = root;
    for step in path {
        match step {
            PathStep::Key(key) => {
                current = current.as_object()?.get(key)?;
            }
            PathStep::Index(index) => {
                current = current.as_array()?.elements.get(*index)?;
            }
        }
    }

    Some(current)
}

fn json_string_span_at_path(
    root: &JsonNode,
    path: &[PathStep],
    next_version: &str,
    selector_text: &str,
    file_path: &Path,
) -> Result<Option<SourceSpan>> {
    let current = json_value_at_path(root, path).ok_or_else(|| {
        anyhow::anyhow!(
            "Internal selector path resolution error for `{selector_text}` in `{}`.",
            file_path.display()
        )
    })?;

    let Some(existing_value) = current.as_str() else {
        bail!(
            "Selector `{selector_text}` matched a non-string JSON value in `{}`.",
            file_path.display()
        );
    };

    if existing_value == next_version {
        return Ok(None);
    }

    Ok(Some(current.string_span().ok_or_else(|| {
        anyhow::anyhow!(
            "Internal selector path resolution error for `{selector_text}` in `{}`.",
            file_path.display()
        )
    })?))
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

fn update_yaml_file(
    file_path: &Path,
    content: &str,
    selectors: &[(String, VersionSelector)],
    next_version: &str,
) -> Result<bool> {
    let documents = load_yaml_documents(content)
        .with_context(|| format!("Failed to parse YAML file `{}`.", file_path.display()))?;
    let replacement_literal = serde_json::to_string(next_version).with_context(|| {
        format!(
            "Failed to serialize YAML value for `{}`.",
            file_path.display()
        )
    })?;

    let Some(root) = documents.first() else {
        if let Some((selector_text, _)) = selectors.first() {
            bail!(
                "Selector `{selector_text}` matched no values in `{}`.",
                file_path.display()
            );
        }
        return Ok(false);
    };

    let mut replacements: BTreeMap<usize, (SourceSpan, String)> = BTreeMap::new();

    for (selector_text, selector) in selectors {
        let target_paths = resolve_yaml_paths(root, selector_text, selector, file_path)?;
        for path in &target_paths {
            if let Some(span) = yaml_scalar_span_at_path(
                root,
                content,
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
            })? {
                replacements
                    .entry(span.start)
                    .or_insert_with(|| (span, replacement_literal.clone()));
            }
        }
    }

    if replacements.is_empty() {
        return Ok(false);
    }

    let mut output = content.to_string();
    for (_, (span, replacement)) in replacements.iter().rev() {
        output.replace_range(span.start..span.end, replacement);
    }

    fs::write(file_path, output)
        .with_context(|| format!("Failed to write `{}`.", file_path.display()))?;
    Ok(true)
}

fn resolve_yaml_paths(
    root: &MarkedYaml,
    selector_text: &str,
    selector: &VersionSelector,
    file_path: &Path,
) -> Result<Vec<Vec<PathStep>>> {
    let mut current_paths = vec![Vec::new()];

    for segment in &selector.segments {
        let mut next_paths = BTreeSet::new();

        for current_path in &current_paths {
            let Some(node) = yaml_value_at_path(root, current_path) else {
                continue;
            };

            let Some(child) = yaml_mapping_get(node, &segment.key) else {
                continue;
            };

            let mut child_path = current_path.clone();
            child_path.push(PathStep::Key(segment.key.clone()));

            match &segment.qualifier {
                None => {
                    next_paths.insert(child_path);
                }
                Some(SegmentQualifier::Index(index)) => {
                    let Some(sequence) = yaml_sequence(child) else {
                        bail!(
                            "Selector `{selector_text}` expects segment `{}` to be an array in `{}`.",
                            segment.key,
                            file_path.display()
                        );
                    };

                    if sequence.get(*index).is_some() {
                        let mut indexed_path = child_path;
                        indexed_path.push(PathStep::Index(*index));
                        next_paths.insert(indexed_path);
                    }
                }
                Some(SegmentQualifier::Filter { field, value }) => {
                    let Some(sequence) = yaml_sequence(child) else {
                        bail!(
                            "Selector `{selector_text}` expects segment `{}` to be an array in `{}`.",
                            segment.key,
                            file_path.display()
                        );
                    };

                    for (idx, element) in sequence.iter().enumerate() {
                        if !matches!(element.data, YamlData::Mapping(_)) {
                            bail!(
                                "Selector `{selector_text}` expects all elements under `{}` to be YAML mappings in `{}`.",
                                segment.key,
                                file_path.display()
                            );
                        }

                        let Some(field_value) = yaml_mapping_get(element, field) else {
                            continue;
                        };

                        let Some(actual_value) = yaml_scalar_str(field_value) else {
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

fn yaml_value_at_path<'a>(root: &'a MarkedYaml, path: &[PathStep]) -> Option<&'a MarkedYaml<'a>> {
    let mut current = root;
    for step in path {
        match step {
            PathStep::Key(key) => {
                current = yaml_mapping_get(current, key)?;
            }
            PathStep::Index(index) => {
                current = yaml_sequence(current)?.get(*index)?;
            }
        }
    }

    Some(current)
}

fn yaml_scalar_span_at_path(
    root: &MarkedYaml,
    content: &str,
    path: &[PathStep],
    next_version: &str,
    selector_text: &str,
    file_path: &Path,
) -> Result<Option<SourceSpan>> {
    let current = yaml_value_at_path(root, path).ok_or_else(|| {
        anyhow::anyhow!(
            "Internal selector path resolution error for `{selector_text}` in `{}`.",
            file_path.display()
        )
    })?;

    let YamlData::Representation(value, style, _) = &current.data else {
        bail!(
            "Selector `{selector_text}` matched a non-string YAML value in `{}`.",
            file_path.display()
        );
    };

    if matches!(style, ScalarStyle::Literal | ScalarStyle::Folded) {
        bail!(
            "Selector `{selector_text}` matched a YAML block scalar in `{}`. Block scalar version values are not supported.",
            file_path.display()
        );
    }

    if value.as_ref() == next_version {
        return Ok(None);
    }

    // saphyr records exact source spans for inline scalars (including surrounding
    // quotes), so the span end is authoritative and needs no manual scanning. Markers
    // are character offsets, so convert them to byte offsets before slicing.
    let start =
        byte_index_for_char_index(content, current.span.start.index()).ok_or_else(|| {
            anyhow::anyhow!(
                "Could not determine YAML source span for selector `{selector_text}` in `{}`.",
                file_path.display()
            )
        })?;
    let end = byte_index_for_char_index(content, current.span.end.index()).ok_or_else(|| {
        anyhow::anyhow!(
            "Could not determine YAML source span for selector `{selector_text}` in `{}`.",
            file_path.display()
        )
    })?;

    if end < start {
        bail!(
            "Could not determine YAML source span for selector `{selector_text}` in `{}`.",
            file_path.display()
        );
    }

    Ok(Some(SourceSpan { start, end }))
}

/// Parse every YAML document in `content`, keeping the raw scalar representation and
/// style (via `early_parse(false)`) so block scalars can be rejected and spans map
/// back to the original source bytes.
fn load_yaml_documents(content: &str) -> Result<Vec<MarkedYaml<'_>>> {
    let mut loader = YamlLoader::<MarkedYaml>::default();
    loader.early_parse(false);
    let mut parser = Parser::new_from_str(content);
    parser
        .load(&mut loader, true)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(loader.into_documents())
}

fn yaml_mapping_get<'a>(node: &'a MarkedYaml, key: &str) -> Option<&'a MarkedYaml<'a>> {
    let YamlData::Mapping(mapping) = &node.data else {
        return None;
    };
    // YAML mappings resolve duplicate keys to the last occurrence.
    mapping
        .iter()
        .rev()
        .find(|(candidate, _)| yaml_scalar_str(candidate) == Some(key))
        .map(|(_, value)| value)
}

fn yaml_sequence<'a>(node: &'a MarkedYaml) -> Option<&'a [MarkedYaml<'a>]> {
    match &node.data {
        YamlData::Sequence(sequence) => Some(sequence.as_slice()),
        _ => None,
    }
}

fn yaml_scalar_str<'a>(node: &'a MarkedYaml) -> Option<&'a str> {
    match &node.data {
        YamlData::Representation(value, _, _) => Some(value.as_ref()),
        _ => None,
    }
}

fn byte_index_for_char_index(content: &str, character_index: usize) -> Option<usize> {
    if character_index == content.chars().count() {
        return Some(content.len());
    }

    content
        .char_indices()
        .nth(character_index)
        .map(|(byte_index, _)| byte_index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn updates_package_json_version_without_reordering_fields() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.json");
        let original = "{\n  \"name\": \"demo\",\n  \"version\": \"1.0.0\",\n  \"private\": true,\n  \"scripts\": {\n    \"build\": \"vite build\"\n  }\n}\n";
        fs::write(&file_path, original).unwrap();

        let mut updates = BTreeMap::new();
        updates.insert("package.json".to_string(), vec!["version".to_string()]);

        let report =
            apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("package.json")]);
        let expected = "{\n  \"name\": \"demo\",\n  \"version\": \"1.1.0\",\n  \"private\": true,\n  \"scripts\": {\n    \"build\": \"vite build\"\n  }\n}\n";
        assert_eq!(fs::read_to_string(file_path).unwrap(), expected);
    }

    #[test]
    fn updates_nested_json_key_without_reformatting() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.json");
        let original = "{\n  \"package\": {\"version\": \"1.0.0\"},\n  \"name\": \"demo\"\n}\n";
        fs::write(&file_path, original).unwrap();

        let mut updates = BTreeMap::new();
        updates.insert(
            "package.json".to_string(),
            vec!["package.version".to_string()],
        );

        let report =
            apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("package.json")]);
        let expected = "{\n  \"package\": {\"version\": \"1.1.0\"},\n  \"name\": \"demo\"\n}\n";
        assert_eq!(fs::read_to_string(file_path).unwrap(), expected);
    }

    #[test]
    fn updates_json_indexed_value_without_reformatting() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.json");
        let original = "{\n  \"packages\": [\n    {\"name\": \"a\", \"version\": \"1.0.0\"},\n    {\"name\": \"b\", \"version\": \"2.0.0\"}\n  ]\n}\n";
        fs::write(&file_path, original).unwrap();

        let mut updates = BTreeMap::new();
        updates.insert(
            "package.json".to_string(),
            vec!["packages[1].version".to_string()],
        );

        let report =
            apply_version_updates(temp_dir.path(), "9.9.9", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("package.json")]);
        let expected = "{\n  \"packages\": [\n    {\"name\": \"a\", \"version\": \"1.0.0\"},\n    {\"name\": \"b\", \"version\": \"9.9.9\"}\n  ]\n}\n";
        assert_eq!(fs::read_to_string(file_path).unwrap(), expected);
    }

    #[test]
    fn updates_all_json_filter_matches_without_reformatting() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.json");
        let original = "{\n  \"package\": [\n    {\"name\": \"brel\", \"version\": \"1.0.0\"},\n    {\"name\": \"other\", \"version\": \"2.0.0\"},\n    {\"name\": \"brel\", \"version\": \"3.0.0\"}\n  ]\n}\n";
        fs::write(&file_path, original).unwrap();

        let mut updates = BTreeMap::new();
        updates.insert(
            "package.json".to_string(),
            vec!["package[name=brel].version".to_string()],
        );

        let report =
            apply_version_updates(temp_dir.path(), "7.7.7", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("package.json")]);
        let expected = "{\n  \"package\": [\n    {\"name\": \"brel\", \"version\": \"7.7.7\"},\n    {\"name\": \"other\", \"version\": \"2.0.0\"},\n    {\"name\": \"brel\", \"version\": \"7.7.7\"}\n  ]\n}\n";
        assert_eq!(fs::read_to_string(file_path).unwrap(), expected);
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
    fn updates_top_level_yaml_version_without_reformatting() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.yaml");
        let original =
            "# Keep this comment\nname: demo\nversion: 1.0.0 # keep inline\nprivate: true\n";
        fs::write(&file_path, original).unwrap();

        let mut updates = BTreeMap::new();
        updates.insert("package.yaml".to_string(), vec!["version".to_string()]);

        let report =
            apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("package.yaml")]);
        let expected =
            "# Keep this comment\nname: demo\nversion: \"1.1.0\" # keep inline\nprivate: true\n";
        assert_eq!(fs::read_to_string(file_path).unwrap(), expected);
    }

    #[test]
    fn updates_nested_yaml_key_without_reformatting() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.yaml");
        fs::write(&file_path, "package:\n  name: demo\n  version: '1.0.0'\n").unwrap();

        let mut updates = BTreeMap::new();
        updates.insert(
            "package.yaml".to_string(),
            vec!["package.version".to_string()],
        );

        let report =
            apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("package.yaml")]);
        let expected = "package:\n  name: demo\n  version: \"1.1.0\"\n";
        assert_eq!(fs::read_to_string(file_path).unwrap(), expected);
    }

    #[test]
    fn updates_yaml_indexed_value_without_reformatting() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.yaml");
        fs::write(
            &file_path,
            "packages:\n  - name: a\n    version: 1.0.0\n  - name: b\n    version: 2.0.0\n",
        )
        .unwrap();

        let mut updates = BTreeMap::new();
        updates.insert(
            "package.yaml".to_string(),
            vec!["packages[1].version".to_string()],
        );

        let report =
            apply_version_updates(temp_dir.path(), "9.9.9", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("package.yaml")]);
        let expected =
            "packages:\n  - name: a\n    version: 1.0.0\n  - name: b\n    version: \"9.9.9\"\n";
        assert_eq!(fs::read_to_string(file_path).unwrap(), expected);
    }

    #[test]
    fn updates_all_yaml_filter_matches_without_reformatting() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.yaml");
        fs::write(
            &file_path,
            "package:\n  - name: brel\n    version: 1.0.0\n  - name: other\n    version: 2.0.0\n  - name: brel\n    version: 3.0.0\n",
        )
        .unwrap();

        let mut updates = BTreeMap::new();
        updates.insert(
            "package.yaml".to_string(),
            vec!["package[name=brel].version".to_string()],
        );

        let report =
            apply_version_updates(temp_dir.path(), "7.7.7", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("package.yaml")]);
        let expected = "package:\n  - name: brel\n    version: \"7.7.7\"\n  - name: other\n    version: 2.0.0\n  - name: brel\n    version: \"7.7.7\"\n";
        assert_eq!(fs::read_to_string(file_path).unwrap(), expected);
    }

    #[test]
    fn updates_yaml_value_after_multibyte_utf8_without_corruption() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.yaml");
        let original =
            "description: café release\npackage:\n  label: 🔧 tooling\n  version: 1.0.0\n";
        fs::write(&file_path, original).unwrap();

        let mut updates = BTreeMap::new();
        updates.insert(
            "package.yaml".to_string(),
            vec!["package.version".to_string()],
        );

        let report =
            apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("package.yaml")]);
        let expected =
            "description: café release\npackage:\n  label: 🔧 tooling\n  version: \"1.1.0\"\n";
        assert_eq!(fs::read_to_string(file_path).unwrap(), expected);
    }

    #[test]
    fn updates_entire_block_style_plain_yaml_scalar_with_flow_punctuation() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.yaml");
        fs::write(&file_path, "version: 1,0,0\nnext: unchanged\n").unwrap();

        let mut updates = BTreeMap::new();
        updates.insert("package.yaml".to_string(), vec!["version".to_string()]);

        let report =
            apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("package.yaml")]);
        assert_eq!(
            fs::read_to_string(file_path).unwrap(),
            "version: \"1.1.0\"\nnext: unchanged\n"
        );
    }

    #[test]
    fn updates_block_style_plain_yaml_scalar_with_closing_punctuation() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.yaml");
        fs::write(&file_path, "package:\n  version: a]b}c\n").unwrap();

        let mut updates = BTreeMap::new();
        updates.insert(
            "package.yaml".to_string(),
            vec!["package.version".to_string()],
        );

        let report =
            apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("package.yaml")]);
        assert_eq!(
            fs::read_to_string(file_path).unwrap(),
            "package:\n  version: \"1.1.0\"\n"
        );
    }

    #[test]
    fn updates_plain_yaml_scalar_after_comment_with_unbalanced_brackets() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.yaml");
        fs::write(&file_path, "# tracked in [release note\nversion: 1,0,0\n").unwrap();

        let mut updates = BTreeMap::new();
        updates.insert("package.yaml".to_string(), vec!["version".to_string()]);

        let report =
            apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("package.yaml")]);
        assert_eq!(
            fs::read_to_string(file_path).unwrap(),
            "# tracked in [release note\nversion: \"1.1.0\"\n"
        );
    }

    #[test]
    fn updates_flow_style_plain_yaml_scalar_without_consuming_next_field() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.yaml");
        fs::write(&file_path, "{version: 1.0.0, name: demo}\n").unwrap();

        let mut updates = BTreeMap::new();
        updates.insert("package.yaml".to_string(), vec!["version".to_string()]);

        let report =
            apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("package.yaml")]);
        assert_eq!(
            fs::read_to_string(file_path).unwrap(),
            "{version: \"1.1.0\", name: demo}\n"
        );
    }

    #[test]
    fn leaves_yaml_unchanged_when_value_already_matches() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.yaml");
        let original = "name: demo\nversion: 1.1.0\n";
        fs::write(&file_path, original).unwrap();

        let mut updates = BTreeMap::new();
        updates.insert("package.yaml".to_string(), vec!["version".to_string()]);

        let report =
            apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new()).unwrap();

        assert!(report.changed_files.is_empty());
        assert_eq!(fs::read_to_string(file_path).unwrap(), original);
    }

    #[test]
    fn infers_yaml_and_yml_extensions() {
        let temp_dir = tempdir().unwrap();
        fs::write(temp_dir.path().join("a.yaml"), "version: 1.0.0\n").unwrap();
        fs::write(temp_dir.path().join("b.yml"), "version: 2.0.0\n").unwrap();

        let mut updates = BTreeMap::new();
        updates.insert("a.yaml".to_string(), vec!["version".to_string()]);
        updates.insert("b.yml".to_string(), vec!["version".to_string()]);

        let report =
            apply_version_updates(temp_dir.path(), "3.0.0", &updates, &BTreeMap::new()).unwrap();

        assert_eq!(
            report.changed_files,
            vec![PathBuf::from("a.yaml"), PathBuf::from("b.yml")]
        );
        assert_eq!(
            fs::read_to_string(temp_dir.path().join("a.yaml")).unwrap(),
            "version: \"3.0.0\"\n"
        );
        assert_eq!(
            fs::read_to_string(temp_dir.path().join("b.yml")).unwrap(),
            "version: \"3.0.0\"\n"
        );
    }

    #[test]
    fn updates_yaml_with_format_override() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("versions");
        fs::write(&file_path, "version: 1.0.0\n").unwrap();

        let mut updates = BTreeMap::new();
        updates.insert("versions".to_string(), vec!["version".to_string()]);

        let mut overrides = BTreeMap::new();
        overrides.insert("versions".to_string(), VersionFileFormat::Yaml);

        let report = apply_version_updates(temp_dir.path(), "1.1.0", &updates, &overrides).unwrap();

        assert_eq!(report.changed_files, vec![PathBuf::from("versions")]);
        assert_eq!(
            fs::read_to_string(file_path).unwrap(),
            "version: \"1.1.0\"\n"
        );
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
    fn fails_when_yaml_target_is_not_string() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.yaml");
        fs::write(&file_path, "version:\n  major: 1\n").unwrap();

        let mut updates = BTreeMap::new();
        updates.insert("package.yaml".to_string(), vec!["version".to_string()]);

        let err = apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new())
            .unwrap_err();
        let err_text = format!("{err:#}");
        assert!(err_text.contains("non-string YAML value"));
    }

    #[test]
    fn fails_when_yaml_block_scalar_is_targeted() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.yaml");
        fs::write(&file_path, "version: |-\n  1.0.0\n").unwrap();

        let mut updates = BTreeMap::new();
        updates.insert("package.yaml".to_string(), vec!["version".to_string()]);

        let err = apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new())
            .unwrap_err();
        let err_text = format!("{err:#}");
        assert!(err_text.contains("YAML block scalar"));
    }

    #[test]
    fn fails_when_yaml_filter_element_is_not_mapping() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("package.yaml");
        fs::write(
            &file_path,
            "package:\n  - name: brel\n    version: 1.0.0\n  - other\n",
        )
        .unwrap();

        let mut updates = BTreeMap::new();
        updates.insert(
            "package.yaml".to_string(),
            vec!["package[name=brel].version".to_string()],
        );

        let err = apply_version_updates(temp_dir.path(), "1.1.0", &updates, &BTreeMap::new())
            .unwrap_err();
        assert!(err.to_string().contains("YAML mappings"));
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
