// SPDX-License-Identifier: GPL-3.0-only
//! Classifying a selection before actions are matched against it.
//!
//! PopClip's built-in intelligence is that the bar already knows *what* was
//! selected — a link, an address, a path — before any extension looks at it.
//! This module is that pass: one cheap scan per settled selection, whose result
//! every action can consult through `detects = [...]` and the `{{url}}`-style
//! placeholders.
//!
//! Classification is deliberately syntactic. It never touches the filesystem or
//! the network: a selected path may belong to another machine, and a selected
//! URL must not be resolved just to be recognised.

use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;

/// A kind of selection an action can require via `detects = [...]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Detect {
    Url,
    Email,
    Path,
    Phone,
    Color,
}

/// What one selection was recognised as. Every field is independent; a
/// selection can be several things at once (`mailto:x@y` is a URL and an
/// email address).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Classification {
    /// A single web URL, normalised: `www.` selections gain `https://`,
    /// trailing sentence punctuation is dropped.
    pub url: Option<String>,
    /// A single email address, without any `mailto:` prefix.
    pub email: Option<String>,
    /// A single absolute or `~/` filesystem path.
    pub path: Option<String>,
    /// A single phone number, compacted to digits and a leading `+` so it can
    /// go straight into a `tel:` URI.
    pub phone: Option<String>,
    /// A single CSS-style colour literal.
    pub color: Option<String>,
    /// The selection parses as a grabit action manifest — the snippets flow.
    pub manifest: bool,
}

impl Classification {
    /// The detected value for `kind`, if any.
    pub fn get(&self, kind: Detect) -> Option<&str> {
        match kind {
            Detect::Url => self.url.as_deref(),
            Detect::Email => self.email.as_deref(),
            Detect::Path => self.path.as_deref(),
            Detect::Phone => self.phone.as_deref(),
            Detect::Color => self.color.as_deref(),
        }
    }
}

static URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:https?|ftp)://\S+$").expect("static regex")
});
static WWW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^www\.[\w-]+(?:\.[\w-]+)+(?:[/?#]\S*)?$").expect("static regex")
});
static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[\w.+-]+@[\w-]+(?:\.[\w-]+)+$").expect("static regex")
});
static PHONE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\+?[0-9(][0-9 ().\-/]{4,24}[0-9]$").expect("static regex")
});
static COLOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:#(?:[0-9a-f]{3}|[0-9a-f]{4}|[0-9a-f]{6}|[0-9a-f]{8})|rgba?\([0-9., %]+\)|hsla?\([0-9., %deg]+\))$",
    )
    .expect("static regex")
});

/// Classify one settled selection.
pub fn classify(text: &str) -> Classification {
    let candidate = text.trim();
    // Multi-line selections are prose, not a single recognisable value. The
    // manifest check is the one exception — manifests are inherently multi-line.
    if candidate.contains('\n') {
        return Classification { manifest: is_manifest(candidate), ..Default::default() };
    }

    let clipped = trim_trailing_punctuation(candidate);

    let url = if URL.is_match(clipped) {
        Some(clipped.to_owned())
    } else if WWW.is_match(clipped) {
        Some(format!("https://{clipped}"))
    } else if let Some(address) = clipped.strip_prefix("mailto:") {
        EMAIL.is_match(address).then(|| clipped.to_owned())
    } else {
        None
    };

    let email = clipped
        .strip_prefix("mailto:")
        .unwrap_or(clipped)
        .split('?') // mailto:x@y?subject=… carries a query part.
        .next()
        .filter(|a| EMAIL.is_match(a))
        .map(str::to_owned);

    let path = (candidate.len() > 1
        && (candidate.starts_with('/') || candidate.starts_with("~/"))
        && !candidate.contains('\u{0}'))
    .then(|| candidate.to_owned());

    let phone = PHONE.is_match(candidate).then(|| compact_phone(candidate)).filter(|digits| {
        let count = digits.trim_start_matches('+').len();
        (6..=15).contains(&count)
    });

    let color = COLOR.is_match(candidate).then(|| candidate.to_owned());

    Classification { url, email, path, phone, color, manifest: false }
}

/// Drop sentence punctuation that clings to a selected value: the period after
/// a URL that ended a sentence, the comma after an address in a list. Closing
/// brackets are removed only when unbalanced, so `https://en.wikipedia.org/wiki/Rust_(film)`
/// keeps its parenthesis while `(https://example.com)` loses the stray one.
fn trim_trailing_punctuation(text: &str) -> &str {
    let mut out = text;
    loop {
        let Some(last) = out.chars().last() else { return out };
        let trimmed = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\'' | '…' => &out[..out.len() - last.len_utf8()],
            ')' if unbalanced(out, '(', ')') => &out[..out.len() - 1],
            ']' if unbalanced(out, '[', ']') => &out[..out.len() - 1],
            '}' if unbalanced(out, '{', '}') => &out[..out.len() - 1],
            _ => return out,
        };
        out = trimmed;
    }
}

fn unbalanced(text: &str, open: char, close: char) -> bool {
    let opens = text.chars().filter(|&c| c == open).count();
    let closes = text.chars().filter(|&c| c == close).count();
    closes > opens
}

/// Reduce a formatted phone number to what a `tel:` URI wants.
fn compact_phone(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, c) in text.chars().enumerate() {
        match c {
            '+' if i == 0 => out.push(c),
            '0'..='9' => out.push(c),
            _ => {}
        }
    }
    out
}

/// Whether a selection is itself a grabit action manifest — PopClip's snippets
/// flow, where selecting an extension's text offers to install it.
///
/// The cheap substring check keeps the TOML parser off the hot path for
/// ordinary selections.
fn is_manifest(text: &str) -> bool {
    if !(text.contains("id") && text.contains("title") && text.contains('=')) {
        return false;
    }
    crate::config::parse_manifest(text).is_ok()
}

/// Classify, including the manifest check for single-line selections (a
/// one-line manifest is legal TOML, if unusual).
pub fn classify_full(text: &str) -> Classification {
    let mut class = classify(text);
    if !class.manifest {
        class.manifest = is_manifest(text.trim());
    }
    class
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_detected_and_sentence_punctuation_is_dropped() {
        assert_eq!(classify("https://example.com").url.as_deref(), Some("https://example.com"));
        assert_eq!(
            classify("  https://example.com/a?b=c.  ").url.as_deref(),
            Some("https://example.com/a?b=c")
        );
        assert_eq!(classify("http://example.com,").url.as_deref(), Some("http://example.com"));
        assert!(classify("just words").url.is_none());
        assert!(classify("https://a.com and https://b.com").url.is_none());
    }

    #[test]
    fn balanced_parentheses_survive_but_stray_ones_do_not() {
        assert_eq!(
            classify("https://en.wikipedia.org/wiki/Rust_(film)").url.as_deref(),
            Some("https://en.wikipedia.org/wiki/Rust_(film)")
        );
        assert_eq!(classify("https://example.com/a)").url.as_deref(), Some("https://example.com/a"));
    }

    #[test]
    fn bare_www_domains_gain_a_scheme() {
        assert_eq!(classify("www.example.com").url.as_deref(), Some("https://www.example.com"));
        assert_eq!(
            classify("www.example.co.uk/path").url.as_deref(),
            Some("https://www.example.co.uk/path")
        );
        assert!(classify("wwwexample.com").url.is_none());
    }

    #[test]
    fn emails_are_detected_with_and_without_mailto() {
        assert_eq!(classify("kim@example.com").email.as_deref(), Some("kim@example.com"));
        assert_eq!(classify("mailto:kim@example.com").email.as_deref(), Some("kim@example.com"));
        assert_eq!(
            classify("mailto:kim@example.com?subject=hi").email.as_deref(),
            Some("kim@example.com")
        );
        assert_eq!(classify("kim@example.com.").email.as_deref(), Some("kim@example.com"));
        assert!(classify("kim@nodot").email.is_none());
        assert!(classify("not an email").email.is_none());
    }

    #[test]
    fn paths_must_be_rooted_and_single_line() {
        assert_eq!(classify("/usr/share/grabit").path.as_deref(), Some("/usr/share/grabit"));
        assert_eq!(classify("~/notes.txt").path.as_deref(), Some("~/notes.txt"));
        assert!(classify("relative/path").path.is_none());
        assert!(classify("/").path.is_none());
        assert!(classify("/a\n/b").path.is_none());
    }

    #[test]
    fn phone_numbers_compact_to_tel_form() {
        assert_eq!(classify("+30 210 123 4567").phone.as_deref(), Some("+302101234567"));
        assert_eq!(classify("(555) 867-5309").phone.as_deref(), Some("5558675309"));
        assert_eq!(classify("210.1234567").phone.as_deref(), Some("2101234567"));
        // Too few digits to be dialable, too many to be E.164.
        assert!(classify("12 345").phone.is_none());
        assert!(classify("1234567890123456789").phone.is_none());
        // Dates and versions must not read as phone numbers.
        assert!(classify("2024").phone.is_none());
    }

    #[test]
    fn colors_cover_hex_and_functional_notation() {
        assert_eq!(classify("#a1b2c3").color.as_deref(), Some("#a1b2c3"));
        assert_eq!(classify("#FFF").color.as_deref(), Some("#FFF"));
        assert_eq!(classify("rgb(1, 2, 3)").color.as_deref(), Some("rgb(1, 2, 3)"));
        assert_eq!(classify("hsl(120, 50%, 50%)").color.as_deref(), Some("hsl(120, 50%, 50%)"));
        assert!(classify("#12345").color.is_none());
        assert!(classify("red").color.is_none());
    }

    #[test]
    fn a_selection_can_be_several_things_at_once() {
        let class = classify("mailto:kim@example.com");
        assert!(class.url.is_some());
        assert!(class.email.is_some());
    }

    #[test]
    fn prose_is_nothing_at_all() {
        let class = classify("The quick brown fox jumps over the lazy dog.");
        assert_eq!(class, Classification::default());
    }

    #[test]
    fn an_action_manifest_is_recognised() {
        let manifest = r#"
id = "shout"
title = "SHOUT"
exec = ["tr", "[:lower:]", "[:upper:]"]
stdin = true
after = "copy"
"#;
        assert!(classify(manifest).manifest);
        assert!(!classify("id = title = nonsense").manifest);
        assert!(!classify("plain text mentioning id and title =").manifest);
    }
}
