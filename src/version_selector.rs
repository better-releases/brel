use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionSelector {
    pub segments: Vec<SelectorSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorSegment {
    pub key: String,
    pub qualifier: Option<SegmentQualifier>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentQualifier {
    Index(usize),
    Filter { field: String, value: String },
}

pub fn parse_selector(value: &str) -> Result<VersionSelector> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("Version selector cannot be empty.");
    }

    let mut segments = Vec::new();
    let mut segment_start = 0usize;
    let mut in_brackets = false;

    for (idx, ch) in trimmed.char_indices() {
        match ch {
            '[' => {
                if in_brackets {
                    bail!("Invalid version selector `{trimmed}`: nested `[` is not supported.");
                }
                in_brackets = true;
            }
            ']' => {
                if !in_brackets {
                    bail!("Invalid version selector `{trimmed}`: unmatched `]`.");
                }
                in_brackets = false;
            }
            '.' if !in_brackets => {
                let raw_segment = &trimmed[segment_start..idx];
                segments.push(parse_segment(raw_segment, trimmed)?);
                segment_start = idx + 1;
            }
            _ => {}
        }
    }

    if in_brackets {
        bail!("Invalid version selector `{trimmed}`: unmatched `[`.");
    }

    let raw_segment = &trimmed[segment_start..];
    segments.push(parse_segment(raw_segment, trimmed)?);

    Ok(VersionSelector { segments })
}

fn parse_segment(raw_segment: &str, selector: &str) -> Result<SelectorSegment> {
    let segment = raw_segment.trim();
    if segment.is_empty() {
        bail!("Invalid version selector `{selector}`: empty path segment.");
    }

    let Some(open_idx) = segment.find('[') else {
        let key = parse_token(segment, "segment key", selector)?;
        return Ok(SelectorSegment {
            key,
            qualifier: None,
        });
    };

    if !segment.ends_with(']') {
        bail!(
            "Invalid version selector `{selector}`: segment `{segment}` must end with `]` when \
             using a qualifier."
        );
    }

    let key = parse_token(&segment[..open_idx], "segment key", selector)?;
    let qualifier_raw = &segment[open_idx + 1..segment.len() - 1];
    let qualifier = parse_qualifier(qualifier_raw, selector)?;

    Ok(SelectorSegment {
        key,
        qualifier: Some(qualifier),
    })
}

fn parse_qualifier(raw: &str, selector: &str) -> Result<SegmentQualifier> {
    let qualifier = raw.trim();
    if qualifier.is_empty() {
        bail!("Invalid version selector `{selector}`: empty segment qualifier.");
    }

    if qualifier.chars().all(|ch| ch.is_ascii_digit()) {
        let index = qualifier.parse::<usize>().with_context(|| {
            format!("Invalid version selector `{selector}`: invalid array index.")
        })?;
        return Ok(SegmentQualifier::Index(index));
    }

    let Some((field_raw, value_raw)) = qualifier.split_once('=') else {
        bail!(
            "Invalid version selector `{selector}`: qualifier `{qualifier}` must be either an \
             array index or `field=value`."
        );
    };

    let field = parse_token(field_raw, "filter field", selector)?;
    let value = parse_filter_value(value_raw, selector)?;

    Ok(SegmentQualifier::Filter { field, value })
}

fn parse_token(raw: &str, label: &str, selector: &str) -> Result<String> {
    let token = raw.trim();
    if token.is_empty() {
        bail!("Invalid version selector `{selector}`: empty {label}.");
    }

    if token.contains('.') || token.contains('[') || token.contains(']') {
        bail!(
            "Invalid version selector `{selector}`: {label} `{token}` contains an unsupported \
             character."
        );
    }

    Ok(token.to_string())
}

fn parse_filter_value(raw: &str, selector: &str) -> Result<String> {
    let value = raw.trim();
    if value.is_empty() {
        bail!("Invalid version selector `{selector}`: empty filter value.");
    }

    let normalized = strip_wrapping_quotes(value);
    if normalized.is_empty() {
        bail!("Invalid version selector `{selector}`: empty filter value.");
    }

    if normalized.contains('[') || normalized.contains(']') {
        bail!(
            "Invalid version selector `{selector}`: filter value `{normalized}` contains an \
             unsupported character."
        );
    }

    Ok(normalized.to_string())
}

fn strip_wrapping_quotes(value: &str) -> &str {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let first = bytes[0];
        let last = bytes[value.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }

    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_key_selector() {
        let selector = parse_selector("version").unwrap();
        assert_eq!(
            selector,
            VersionSelector {
                segments: vec![SelectorSegment {
                    key: "version".to_string(),
                    qualifier: None
                }]
            }
        );
    }

    #[test]
    fn parses_nested_selector() {
        let selector = parse_selector("package.version").unwrap();
        assert_eq!(
            selector,
            VersionSelector {
                segments: vec![
                    SelectorSegment {
                        key: "package".to_string(),
                        qualifier: None
                    },
                    SelectorSegment {
                        key: "version".to_string(),
                        qualifier: None
                    }
                ]
            }
        );
    }

    #[test]
    fn parses_selector_with_index() {
        let selector = parse_selector("packages[0].version").unwrap();
        assert_eq!(
            selector,
            VersionSelector {
                segments: vec![
                    SelectorSegment {
                        key: "packages".to_string(),
                        qualifier: Some(SegmentQualifier::Index(0))
                    },
                    SelectorSegment {
                        key: "version".to_string(),
                        qualifier: None
                    }
                ]
            }
        );
    }

    #[test]
    fn parses_selector_with_filter() {
        let selector = parse_selector("package[name=brel].version").unwrap();
        assert_eq!(
            selector,
            VersionSelector {
                segments: vec![
                    SelectorSegment {
                        key: "package".to_string(),
                        qualifier: Some(SegmentQualifier::Filter {
                            field: "name".to_string(),
                            value: "brel".to_string()
                        })
                    },
                    SelectorSegment {
                        key: "version".to_string(),
                        qualifier: None
                    }
                ]
            }
        );
    }

    #[test]
    fn parses_selector_with_quoted_filter_value() {
        let selector = parse_selector("package[name=\"brel\"].version").unwrap();
        assert_eq!(
            selector,
            VersionSelector {
                segments: vec![
                    SelectorSegment {
                        key: "package".to_string(),
                        qualifier: Some(SegmentQualifier::Filter {
                            field: "name".to_string(),
                            value: "brel".to_string()
                        })
                    },
                    SelectorSegment {
                        key: "version".to_string(),
                        qualifier: None
                    }
                ]
            }
        );
    }

    #[test]
    fn rejects_empty_selector() {
        let err = parse_selector(" ").unwrap_err();
        assert!(err.to_string().contains("cannot be empty"));
    }

    #[test]
    fn rejects_empty_path_segment() {
        let err = parse_selector("package..version").unwrap_err();
        assert!(err.to_string().contains("empty path segment"));
    }

    #[test]
    fn rejects_malformed_brackets() {
        let err = parse_selector("package[name=brel.version").unwrap_err();
        assert!(err.to_string().contains("unmatched `[`"));
    }

    #[test]
    fn rejects_missing_filter_equals() {
        let err = parse_selector("package[name].version").unwrap_err();
        assert!(
            err.to_string()
                .contains("must be either an array index or `field=value`")
        );
    }

    #[test]
    fn rejects_negative_index() {
        let err = parse_selector("package[-1].version").unwrap_err();
        assert!(
            err.to_string()
                .contains("must be either an array index or `field=value`")
        );
    }

    #[test]
    fn rejects_non_numeric_index() {
        let err = parse_selector("package[abc].version").unwrap_err();
        assert!(
            err.to_string()
                .contains("must be either an array index or `field=value`")
        );
    }
}
