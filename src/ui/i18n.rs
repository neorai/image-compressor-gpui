//! Runtime localization backed by the bundled gettext catalog.
//!
//! English source strings are the catalog keys, so English needs no lookup and
//! a missing Spanish entry falls back to readable English text.

use std::collections::HashMap;
use std::fmt::{Display, Write as _};
use std::sync::LazyLock;

const SPANISH_CATALOG: &str = include_str!("../../translations/es/LC_MESSAGES/image-compressor.po");

static SPANISH: LazyLock<HashMap<String, String>> = LazyLock::new(|| parse_po(SPANISH_CATALOG));

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Language {
    #[default]
    English,
    Spanish,
}

impl Language {
    pub const ALL: [Self; 2] = [Self::English, Self::Spanish];

    /// The language's own name, so a reader can find it whatever the current
    /// interface language is.
    pub fn native_name(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::Spanish => "Español",
        }
    }
}

/// Translates a source string.
pub fn tr(language: Language, source: &str) -> String {
    match language {
        Language::English => source.to_owned(),
        Language::Spanish => SPANISH
            .get(source)
            .cloned()
            .unwrap_or_else(|| source.to_owned()),
    }
}

/// Translates a source string and fills its `{}` placeholders in order.
pub fn trf(language: Language, source: &str, args: &[&dyn Display]) -> String {
    let template = tr(language, source);
    let mut output = String::with_capacity(template.len());
    let mut args = args.iter();
    let mut pieces = template.split("{}");
    if let Some(first) = pieces.next() {
        output.push_str(first);
    }
    for piece in pieces {
        if let Some(arg) = args.next() {
            // Writing into a String cannot fail.
            let _ = write!(output, "{arg}");
        } else {
            output.push_str("{}");
        }
        output.push_str(piece);
    }
    output
}

/// Parses the subset of the PO format the catalog uses: `msgid`/`msgstr`
/// pairs with optional continuation lines and C-style escapes.
fn parse_po(source: &str) -> HashMap<String, String> {
    enum Field {
        None,
        Id,
        Text,
    }

    let mut catalog = HashMap::new();
    let mut id = String::new();
    let mut text = String::new();
    let mut field = Field::None;
    let mut flush = |id: &mut String, text: &mut String| {
        if !id.is_empty() && !text.is_empty() {
            catalog.insert(std::mem::take(id), std::mem::take(text));
        }
        id.clear();
        text.clear();
    };

    for line in source.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("msgid ") {
            flush(&mut id, &mut text);
            id.push_str(&unquote(rest));
            field = Field::Id;
        } else if let Some(rest) = line.strip_prefix("msgstr ") {
            text.push_str(&unquote(rest));
            field = Field::Text;
        } else if line.starts_with('"') {
            match field {
                Field::Id => id.push_str(&unquote(line)),
                Field::Text => text.push_str(&unquote(line)),
                Field::None => {}
            }
        } else if line.is_empty() || line.starts_with('#') {
            field = Field::None;
        }
    }
    flush(&mut id, &mut text);
    catalog
}

fn unquote(value: &str) -> String {
    let inner = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value);
    let mut output = String::with_capacity(inner.len());
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        match characters.next() {
            Some('n') => output.push('\n'),
            Some('t') => output.push('\t'),
            Some(other) => output.push(other),
            None => output.push('\\'),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{Language, SPANISH, parse_po, tr, trf};

    /// Sources that call `tr`/`trf` with literal keys.
    const SOURCES: [&str; 2] = [include_str!("messages.rs"), include_str!("view.rs")];

    /// Finds the literal key of every `tr(language, "…")`, `trf(language, "…", …)`
    /// and `self.tr("…")` call, however rustfmt wrapped it.
    fn source_keys() -> Vec<String> {
        let is_identifier = |character: char| character.is_alphanumeric() || character == '_';
        let mut keys = Vec::new();
        for source in SOURCES {
            for (index, token) in source
                .match_indices("tr(")
                .chain(source.match_indices("trf("))
            {
                if source[..index].ends_with(is_identifier) {
                    continue;
                }
                let mut rest = source[index + token.len()..].trim_start();
                if !rest.starts_with('"') {
                    // Skip the language argument: `language`, `self.language`…
                    rest = rest
                        .trim_start_matches(|character| {
                            is_identifier(character) || character == '.'
                        })
                        .trim_start();
                    let Some(after_comma) = rest.strip_prefix(',') else {
                        continue;
                    };
                    rest = after_comma.trim_start();
                }
                let Some(literal) = rest.strip_prefix('"') else {
                    continue;
                };
                if let Some(end) = literal.find('"') {
                    keys.push(literal[..end].to_owned());
                }
            }
        }
        keys
    }

    #[test]
    fn every_interface_string_has_a_spanish_translation() {
        let keys = source_keys();
        assert!(keys.len() > 50, "the key scanner found too few keys");
        let missing: Vec<_> = keys
            .iter()
            .filter(|key| !SPANISH.contains_key(key.as_str()))
            .collect();
        assert!(missing.is_empty(), "untranslated strings: {missing:?}");
    }

    #[test]
    fn catalog_has_no_unused_entries() {
        let keys = source_keys();
        let unused: Vec<_> = SPANISH
            .keys()
            .filter(|entry| !keys.iter().any(|key| key == *entry))
            .collect();
        assert!(unused.is_empty(), "unused catalog entries: {unused:?}");
    }

    #[test]
    fn placeholders_are_filled_in_order() {
        assert_eq!(
            trf(Language::English, "{} of {}", &[&1, &"two"]),
            "1 of two"
        );
        assert_eq!(trf(Language::English, "{} and {}", &[&1]), "1 and {}");
        assert_eq!(
            trf(Language::Spanish, "Local data: {}", &[&"C:\\data"]),
            "Datos locales: C:\\data"
        );
        assert_eq!(
            tr(Language::Spanish, "not in the catalog"),
            "not in the catalog"
        );
    }

    #[test]
    fn parser_handles_continuations_escapes_and_crlf() {
        let catalog = parse_po(
            "msgid \"\"\r\nmsgstr \"Header\\n\"\r\n\r\nmsgid \"a \\\"b\\\"\"\r\nmsgstr \"\"\r\n\"c \"\r\n\"d\"\r\n",
        );
        assert_eq!(catalog.get("a \"b\"").map(String::as_str), Some("c d"));
        assert_eq!(catalog.len(), 1);
    }
}
