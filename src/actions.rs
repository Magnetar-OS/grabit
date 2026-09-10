// SPDX-License-Identifier: GPL-3.0-only
//! Template expansion and action execution.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::classify::Classification;
use crate::config::{Action, After, OptionSpec};
use crate::selection::Grab;

/// How long an `exec` action may run before it is killed. Actions are meant to
/// be quick transformations; anything slower has almost certainly hung.
const EXEC_TIMEOUT: Duration = Duration::from_secs(15);

/// Interval between `try_wait` polls while waiting for an action to exit.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// What the caller should do once an action has run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing further; the action was self-contained.
    Nothing,
    /// Put this on the clipboard.
    Clipboard(String),
    /// Put this on the clipboard and paste it over the selection.
    Replace(String),
    /// Show this in the bar's result view.
    Show(String),
}

/// Everything placeholder expansion can draw on: the selection itself, its
/// classification, and the options the action declares.
pub struct Expansion<'a> {
    pub grab: &'a Grab,
    pub class: &'a Classification,
    /// The invoking action's `[options]` table, read by `{{option:NAME}}`.
    pub options: &'a BTreeMap<String, OptionSpec>,
}

/// Percent-encode for use in a URL query component.
///
/// Everything outside the RFC 3986 unreserved set is escaped, including `/`
/// and `:`, because expanded text is always a value and never structure.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Expand `{{...}}` placeholders in a template.
///
/// In `url_mode` the plain `{{text}}` placeholder is percent-encoded, since
/// that is what a URL template almost always wants; `{{text_raw}}` opts out for
/// the cases where the selection *is* the URL. Outside `url_mode` it is the
/// other way round, and `{{text_url}}` opts in.
///
/// The detection placeholders — `{{url}}`, `{{email}}`, `{{path}}`,
/// `{{phone}}`, `{{color}}` — expand to the classified value verbatim: they are
/// already in the shape their URI scheme wants, and encoding a URL that *is*
/// the template's value would break it. An action guarded by `detects` always
/// has its value; without the guard an undetected kind expands to nothing.
///
/// `{{option:NAME}}` expands to the action's own option — the value chosen in
/// the settings window, or the option's `default` until one is. It is encoded
/// in `url_mode` exactly as `{{text}}` is: an option holds a value, never
/// URL structure. An option the action does not declare expands to nothing.
pub fn expand(template: &str, ctx: &Expansion<'_>, url_mode: bool) -> String {
    let text = ctx.grab.text.as_str();
    let trimmed = text.trim();
    let first_line = text.lines().next().unwrap_or_default();

    let plain = |s: &str| if url_mode { percent_encode(s) } else { s.to_owned() };
    let detected = |value: Option<&str>, name: &str| {
        value.map(str::to_owned).unwrap_or_else(|| {
            log::warn!("`{{{{{name}}}}}` used but the selection was not detected as one");
            String::new()
        })
    };

    let mut out = String::with_capacity(template.len() + text.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("}}") else {
            // Unterminated placeholder: emit the remainder verbatim rather than
            // silently truncating the template.
            out.push_str(&rest[start..]);
            return out;
        };
        let name = after_open[..end].trim();
        out.push_str(&match name {
            "text" => plain(text),
            "text_raw" => text.to_owned(),
            "text_trimmed" => plain(trimmed),
            "text_trimmed_raw" => trimmed.to_owned(),
            "text_url" => percent_encode(text),
            "text_trimmed_url" => percent_encode(trimmed),
            "text_line" => plain(first_line),
            "url" => detected(ctx.class.url.as_deref(), name),
            "email" => detected(ctx.class.email.as_deref(), name),
            "path" => detected(ctx.class.path.as_deref(), name),
            "phone" => detected(ctx.class.phone.as_deref(), name),
            "color" => detected(ctx.class.color.as_deref(), name),
            // The source's own HTML rendering of the selection, when it offered
            // one; plain text is the honest fallback for sources that did not.
            "html" => ctx.grab.html.clone().unwrap_or_else(|| plain(text)),
            "markdown" => match &ctx.grab.html {
                Some(html) => htmd::convert(html).unwrap_or_else(|e| {
                    log::warn!("html-to-markdown conversion failed: {e}");
                    plain(text)
                }),
                None => plain(text),
            },
            other => match other.strip_prefix("option:") {
                // An option the action never declared expands to nothing, the
                // same way an undetected `{{url}}` does: the template asked for
                // a value that does not exist, and the literal placeholder
                // would be worse in a URL or an argv than an empty string.
                Some(key) => match ctx.options.get(key.trim()) {
                    Some(option) => plain(option.effective()),
                    None => {
                        log::warn!("`{{{{{other}}}}}` used but the action declares no such option");
                        String::new()
                    }
                },
                None => {
                    log::warn!("unknown placeholder `{{{{{other}}}}}` left as-is");
                    format!("{{{{{other}}}}}")
                }
            },
        });
        rest = &after_open[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Run `action` against the selection in `ctx`.
///
/// Runs on a worker thread: `exec` actions block until the child exits.
/// Builtins never reach this function — they need the injector, which the
/// engine owns.
pub fn run(action: &Action, ctx: &Expansion<'_>) -> Result<Outcome> {
    if let Some(url) = &action.spec.url {
        let expanded = expand(url, ctx, true);
        open_uri(&expanded).with_context(|| format!("opening {expanded}"))?;
        return Ok(Outcome::Nothing);
    }

    // No command: the selection itself is the result.
    let output = match &action.spec.exec {
        None => ctx.grab.text.clone(),
        Some(argv) => exec(action, argv, ctx)?,
    };

    Ok(match action.spec.after {
        After::Ignore => Outcome::Nothing,
        After::Copy => Outcome::Clipboard(output),
        After::Replace => Outcome::Replace(output),
        After::Show => Outcome::Show(output),
    })
}

fn exec(action: &Action, argv: &[String], ctx: &Expansion<'_>) -> Result<String> {
    let text = ctx.grab.text.as_str();
    let (program, args) = argv.split_first().context("`exec` is empty")?;
    let program = expand(program, ctx, false);
    let args: Vec<String> = args.iter().map(|a| expand(a, ctx, false)).collect();

    let mut child = Command::new(&program)
        .args(&args)
        .stdin(if action.spec.stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning `{program}`"))?;

    if action.spec.stdin {
        let mut sink = child.stdin.take().expect("stdin was piped");
        let text = text.to_owned();
        // Write on a separate thread: a child that never reads its stdin would
        // otherwise deadlock us once the pipe buffer fills.
        std::thread::spawn(move || {
            if let Err(e) = sink.write_all(text.as_bytes()) {
                log::debug!("action stdin closed early: {e}");
            }
        });
    }

    let deadline = Instant::now() + EXEC_TIMEOUT;
    loop {
        match child.try_wait().context("waiting for action")? {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "action `{}` did not finish within {}s and was killed",
                    action.spec.id,
                    EXEC_TIMEOUT.as_secs()
                );
            }
            None => std::thread::sleep(POLL_INTERVAL),
        }
    }

    let out = child.wait_with_output().context("collecting action output")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("action `{}` exited with {}: {}", action.spec.id, out.status, stderr.trim());
    }
    String::from_utf8(out.stdout)
        .with_context(|| format!("action `{}` produced non-UTF-8 output", action.spec.id))
}

/// Hand a URI to the desktop's handler.
///
/// The OpenURI portal is tried first because it is the mechanism that keeps
/// working inside a Flatpak sandbox; `xdg-open` covers the case where no portal
/// implements it.
fn open_uri(uri: &str) -> Result<()> {
    match open_uri_portal(uri) {
        Ok(()) => Ok(()),
        Err(e) => {
            log::debug!("OpenURI portal unavailable ({e:#}), falling back to xdg-open");
            let status = Command::new("xdg-open")
                .arg(uri)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("running xdg-open")?;
            anyhow::ensure!(status.success(), "xdg-open exited with {status}");
            Ok(())
        }
    }
}

fn open_uri_portal(uri: &str) -> Result<()> {
    use std::collections::HashMap;
    use zbus::zvariant::Value;

    let conn = zbus::blocking::Connection::session().context("connecting to the session bus")?;
    let proxy = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.OpenURI",
    )
    .context("creating OpenURI proxy")?;

    // An empty parent window handle is valid and means "no parent"; grabit has
    // no toplevel to parent a chooser dialog to.
    let options: HashMap<&str, Value> = HashMap::new();
    let _: zbus::zvariant::OwnedObjectPath =
        proxy.call("OpenURI", &("", uri, options)).context("calling OpenURI")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::classify;

    /// Expand against a plain-text selection, classifying it first — what the
    /// engine does for real invocations.
    fn x(template: &str, text: &str, url_mode: bool) -> String {
        let grab = Grab::text(text);
        let class = classify(text);
        let options = BTreeMap::new();
        expand(template, &Expansion { grab: &grab, class: &class, options: &options }, url_mode)
    }

    /// An option table holding one option with the given default and value.
    fn opts(name: &str, default: &str, value: Option<&str>) -> BTreeMap<String, OptionSpec> {
        BTreeMap::from([(
            name.to_owned(),
            OptionSpec {
                label: name.to_owned(),
                default: default.to_owned(),
                choices: Vec::new(),
                value: value.map(str::to_owned),
            },
        )])
    }

    /// Expand against an action that declares options.
    fn xo(template: &str, options: &BTreeMap<String, OptionSpec>, url_mode: bool) -> String {
        let grab = Grab::text("sel");
        let class = classify("sel");
        expand(template, &Expansion { grab: &grab, class: &class, options }, url_mode)
    }

    #[test]
    fn url_mode_encodes_text_and_leaves_raw_alone() {
        assert_eq!(x("q={{text}}", "a b&c", true), "q=a%20b%26c");
        assert_eq!(x("{{text_raw}}", "a b&c", true), "a b&c");
    }

    #[test]
    fn exec_mode_is_literal_unless_url_is_requested() {
        assert_eq!(x("{{text}}", "a b&c", false), "a b&c");
        assert_eq!(x("{{text_url}}", "a b&c", false), "a%20b%26c");
    }

    #[test]
    fn trimmed_and_line_variants() {
        assert_eq!(x("{{text_trimmed_raw}}", "  hi  ", true), "hi");
        assert_eq!(x("{{text_line}}", "one\ntwo", false), "one");
    }

    #[test]
    fn unknown_placeholder_survives_expansion() {
        assert_eq!(x("a{{nope}}b", "x", false), "a{{nope}}b");
    }

    #[test]
    fn unterminated_placeholder_does_not_truncate() {
        assert_eq!(x("a{{text", "x", false), "a{{text");
    }

    #[test]
    fn non_ascii_is_encoded_as_utf8_bytes() {
        assert_eq!(x("{{text}}", "é", true), "%C3%A9");
    }

    #[test]
    fn detection_placeholders_expand_verbatim_even_in_url_mode() {
        assert_eq!(x("{{url}}", "https://a.com/x?y=1.", true), "https://a.com/x?y=1");
        assert_eq!(x("mailto:{{email}}", "kim@example.com,", true), "mailto:kim@example.com");
        assert_eq!(x("tel:{{phone}}", "+30 210 123 4567", true), "tel:+302101234567");
    }

    #[test]
    fn undetected_placeholder_expands_to_nothing() {
        assert_eq!(x("[{{url}}]", "plain words", true), "[]");
    }

    #[test]
    fn html_and_markdown_fall_back_to_the_plain_text() {
        assert_eq!(x("{{html}}", "hi", false), "hi");
        assert_eq!(x("{{markdown}}", "hi", false), "hi");
    }

    #[test]
    fn option_expands_to_its_value_and_falls_back_to_the_default() {
        // No value chosen yet: a fresh install expands the declared default.
        assert_eq!(xo("to={{option:lang}}", &opts("lang", "de", None), false), "to=de");
        // Once chosen, the value wins.
        assert_eq!(xo("to={{option:lang}}", &opts("lang", "de", Some("fr")), false), "to=fr");
    }

    #[test]
    fn option_is_encoded_in_url_mode_like_any_other_value() {
        let o = opts("q", "a b&c", None);
        assert_eq!(xo("s={{option:q}}", &o, true), "s=a%20b%26c");
        assert_eq!(xo("s={{option:q}}", &o, false), "s=a b&c");
    }

    #[test]
    fn undeclared_option_expands_to_nothing() {
        assert_eq!(xo("x={{option:nope}}", &opts("lang", "de", None), false), "x=");
    }

    #[test]
    fn option_name_is_trimmed_and_unknown_placeholders_still_survive() {
        assert_eq!(xo("{{ option:lang }}", &opts("lang", "de", None), false), "de");
        assert_eq!(xo("{{nonsense}}", &opts("lang", "de", None), false), "{{nonsense}}");
    }

    #[test]
    fn html_and_markdown_use_the_html_capture_when_present() {
        let grab = Grab { text: "bold".into(), html: Some("<b>bold</b>".into()) };
        let class = classify("bold");
        let options = BTreeMap::new();
        let ctx = Expansion { grab: &grab, class: &class, options: &options };
        assert_eq!(expand("{{html}}", &ctx, false), "<b>bold</b>");
        assert_eq!(expand("{{markdown}}", &ctx, false), "**bold**");
    }
}
