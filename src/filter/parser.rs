use crate::gelf::message::Level;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ParsedFilter {
    All,
    None,
    Qualifiers(Vec<Qualifier>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Qualifier {
    BarePattern(Pattern),
    SelectorPattern(Selector, Pattern),
    LevelFilter { op: LevelOp, level: Level },
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Substring(String), // stored lowercase for case-insensitive matching
    Regex {
        source: String,
        compiled: regex::Regex,
        case_insensitive: bool,
    },
}

// Custom Serialize/Deserialize for Pattern since regex::Regex doesn't implement them
impl Serialize for Pattern {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Pattern::Substring(s) => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "substring")?;
                map.serialize_entry("value", s)?;
                map.end()
            }
            Pattern::Regex {
                source,
                case_insensitive,
                ..
            } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "regex")?;
                map.serialize_entry("source", source)?;
                map.serialize_entry("case_insensitive", case_insensitive)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Pattern {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{self, MapAccess, Visitor};
        use std::fmt;

        struct PatternVisitor;

        impl<'de> Visitor<'de> for PatternVisitor {
            type Value = Pattern;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a Pattern map")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut r#type: Option<String> = None;
                let mut value: Option<String> = None;
                let mut source: Option<String> = None;
                let mut case_insensitive: Option<bool> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "type" => r#type = Some(map.next_value()?),
                        "value" => value = Some(map.next_value()?),
                        "source" => source = Some(map.next_value()?),
                        "case_insensitive" => case_insensitive = Some(map.next_value()?),
                        _ => {
                            let _ = map.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }

                match r#type.as_deref() {
                    Some("substring") => {
                        let v = value.ok_or_else(|| de::Error::missing_field("value"))?;
                        Ok(Pattern::Substring(v))
                    }
                    Some("regex") => {
                        let src = source.ok_or_else(|| de::Error::missing_field("source"))?;
                        let ci = case_insensitive.unwrap_or(false);
                        let pattern = if ci {
                            format!("(?i){}", src)
                        } else {
                            src.clone()
                        };
                        let compiled = regex::Regex::new(&pattern)
                            .map_err(|e| de::Error::custom(e.to_string()))?;
                        Ok(Pattern::Regex {
                            source: src,
                            compiled,
                            case_insensitive: ci,
                        })
                    }
                    other => Err(de::Error::custom(format!(
                        "unknown pattern type: {:?}",
                        other
                    ))),
                }
            }
        }

        deserializer.deserialize_map(PatternVisitor)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Selector {
    Message,                   // m
    FullMessage,               // fm
    MessageOrFull,             // mfm
    Host,                      // h
    Facility,                  // fa
    File,                      // fi
    Line,                      // ln
    AdditionalField(String),   // anything else
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum LevelOp {
    Eq,
    Gte,
    Lte,
}

#[derive(Debug, Error)]
pub enum FilterParseError {
    #[error("empty filter string")]
    Empty,
    #[error("invalid regex: {0}")]
    InvalidRegex(#[from] regex::Error),
    #[error("unknown level: {0}")]
    UnknownLevel(String),
    #[error("invalid level filter syntax: {0}")]
    InvalidLevelSyntax(String),
    #[error("unclosed quote")]
    UnclosedQuote,
}

/// Split the input on commas, but respect double-quoted strings (don't split inside quotes).
fn split_on_commas(input: &str) -> Result<Vec<String>, FilterParseError> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;

    for ch in input.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                current.push(ch);
            }
            ',' if !in_quote => {
                tokens.push(current.clone());
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    if in_quote {
        return Err(FilterParseError::UnclosedQuote);
    }

    tokens.push(current);
    Ok(tokens)
}

/// Parse a pattern string (could be a regex `/pattern/[i]` or a plain substring).
fn parse_pattern(s: &str) -> Result<Pattern, FilterParseError> {
    if let Some(inner) = s.strip_prefix('/') {
        // Regex: find the closing /
        let (source, case_insensitive) = if let Some(pos) = inner.rfind('/') {
            let src = &inner[..pos];
            let flags = &inner[pos + 1..];
            let ci = flags.contains('i');
            (src.to_string(), ci)
        } else {
            // No closing slash — treat the whole thing (minus leading /) as pattern without flags
            (inner.to_string(), false)
        };

        let pattern = if case_insensitive {
            format!("(?i){}", source)
        } else {
            source.clone()
        };
        let compiled = regex::Regex::new(&pattern)?;

        Ok(Pattern::Regex {
            source,
            compiled,
            case_insensitive,
        })
    } else {
        // Substring: store lowercased
        Ok(Pattern::Substring(s.to_lowercase()))
    }
}

/// Map a selector string to a Selector enum variant.
fn parse_selector(s: &str) -> Selector {
    match s {
        "m" => Selector::Message,
        "fm" => Selector::FullMessage,
        "mfm" => Selector::MessageOrFull,
        "h" => Selector::Host,
        "fa" => Selector::Facility,
        "fi" => Selector::File,
        "ln" => Selector::Line,
        other => Selector::AdditionalField(other.to_string()),
    }
}

/// Parse a single token into a Qualifier.
fn parse_token(token: &str) -> Result<Qualifier, FilterParseError> {
    let token = token.trim();

    // Level filter: l>=X, l<=X, l=X  (must check before general = handling)
    if let Some(rest) = token.strip_prefix("l>=") {
        let level = Level::from_name(rest)
            .ok_or_else(|| FilterParseError::UnknownLevel(rest.to_string()))?;
        return Ok(Qualifier::LevelFilter {
            op: LevelOp::Gte,
            level,
        });
    }
    if let Some(rest) = token.strip_prefix("l<=") {
        let level = Level::from_name(rest)
            .ok_or_else(|| FilterParseError::UnknownLevel(rest.to_string()))?;
        return Ok(Qualifier::LevelFilter {
            op: LevelOp::Lte,
            level,
        });
    }
    if let Some(rest) = token.strip_prefix("l=") {
        let level = Level::from_name(rest)
            .ok_or_else(|| FilterParseError::UnknownLevel(rest.to_string()))?;
        return Ok(Qualifier::LevelFilter {
            op: LevelOp::Eq,
            level,
        });
    }

    // Quoted bare pattern: "..."
    if token.starts_with('"') && token.ends_with('"') && token.len() >= 2 {
        let inner = &token[1..token.len() - 1];
        // Quoted patterns are stored as-is (lowercased for case-insensitive matching)
        return Ok(Qualifier::BarePattern(Pattern::Substring(
            inner.to_lowercase(),
        )));
    }

    // Bare regex: /pattern/[i]
    if token.starts_with('/') {
        return Ok(Qualifier::BarePattern(parse_pattern(token)?));
    }

    // Selector=pattern: look for the first `=` sign
    if let Some(eq_pos) = token.find('=') {
        let lhs = token[..eq_pos].trim();
        let rhs = token[eq_pos + 1..].trim();

        // lhs must not be empty; if it's empty treat as bare substring
        if !lhs.is_empty() {
            let selector = parse_selector(lhs);
            // Strip surrounding quotes from pattern value
            let rhs = if rhs.starts_with('"') && rhs.ends_with('"') && rhs.len() >= 2 {
                &rhs[1..rhs.len() - 1]
            } else {
                rhs
            };
            let pattern = parse_pattern(rhs)?;
            return Ok(Qualifier::SelectorPattern(selector, pattern));
        }
    }

    // Bare substring (lowercased)
    Ok(Qualifier::BarePattern(Pattern::Substring(
        token.to_lowercase(),
    )))
}

pub fn parse_filter(input: &str) -> Result<ParsedFilter, FilterParseError> {
    let input = input.trim();

    if input.is_empty() {
        return Err(FilterParseError::Empty);
    }

    if input == "ALL" {
        return Ok(ParsedFilter::All);
    }

    if input == "NONE" {
        return Ok(ParsedFilter::None);
    }

    let tokens = split_on_commas(input)?;
    let mut qualifiers = Vec::with_capacity(tokens.len());

    for token in tokens {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        qualifiers.push(parse_token(token)?);
    }

    if qualifiers.is_empty() {
        return Err(FilterParseError::Empty);
    }

    Ok(ParsedFilter::Qualifiers(qualifiers))
}
