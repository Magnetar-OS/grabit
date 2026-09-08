//! Configuration and action-manifest loading.
//!
//! Configuration lives in two places, merged with user files winning on `id`
//! collision:
//!
//! * `$XDG_CONFIG_HOME/grabit/` — user config and actions
//! * `/usr/share/grabit/` — packaged defaults
//!
//! `config.toml` holds daemon settings; every `actions/*.toml` file declares a
//! single action. Splitting actions into one file each keeps them
//! drop-in-installable by third parties, the same way PopClip extensions are.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;

use crate::classify::{Classification, Detect};

/// System-wide data directory for packaged actions.
const SYSTEM_DATA_DIR: &str = "/usr/share/grabit";

/// What to do with an action's captured stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum After {
    /// Discard stdout; the command is run purely for its side effect.
    #[default]
    Ignore,
    /// Place stdout on the clipboard.
    Copy,
    /// Place stdout on the clipboard and paste over the selection.
    Replace,
    /// Show stdout in the bar itself — the result view.
    Show,
}

/// A behavior implemented by grabit itself rather than by a URL or a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Builtin {
    /// Copy the selection, then delete it from the source.
    Cut,
    /// Paste the current clipboard over the selection.
    Paste,
}

/// Popup appearance and timing.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Popup {
    /// How long the primary selection must stay unchanged before the popup is
    /// shown. Drag-selection updates PRIMARY continuously, so without this the
    /// bar would flicker through every intermediate selection.
    pub settle_ms: u64,
    /// Hard cap on buttons shown, after matching and ordering.
    pub max_actions: usize,
    /// Pixel size of action icons.
    pub icon_size: i32,
    /// Offset of the bar relative to the pointer, in logical pixels.
    pub offset_x: i32,
    pub offset_y: i32,
    /// Grace period after the pointer leaves the bar before it hides.
    /// Zero disables leave-triggered dismissal.
    pub dismiss_ms: u64,
    /// Absolute lifetime of the popup regardless of pointer activity.
    /// Zero disables the timeout.
    pub timeout_ms: u64,
    /// Slide the bar into place instead of snapping it there.
    pub animate: bool,
}

impl Default for Popup {
    fn default() -> Self {
        Self {
            settle_ms: 140,
            max_actions: 8,
            icon_size: 16,
            offset_x: 12,
            offset_y: 18,
            dismiss_ms: 900,
            timeout_ms: 8000,
            animate: true,
        }
    }
}

/// Per-application behavior, keyed on Wayland app ids.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Applications {
    /// App ids the bar must never appear in. An entry matches an app id either
    /// exactly or as its last reverse-DNS segment, case-insensitively, so both
    /// `org.mozilla.firefox` and `firefox` work.
    pub exclude: Vec<String>,
}

impl Applications {
    /// Whether `app_id` is on the exclude list.
    pub fn is_excluded(&self, app_id: &str) -> bool {
        let id = app_id.to_ascii_lowercase();
        self.exclude.iter().any(|entry| {
            let entry = entry.to_ascii_lowercase();
            id == entry || id.ends_with(&format!(".{entry}"))
        })
    }
}

/// Which selections are considered interesting.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Selection {
    /// Selections shorter than this (after trimming) are ignored.
    pub min_length: usize,
    /// Selections longer than this are ignored, to avoid popping up when the
    /// user hits Ctrl+A in an editor.
    pub max_length: usize,
    /// Ignore selections that are only whitespace.
    pub ignore_whitespace_only: bool,
}

impl Default for Selection {
    fn default() -> Self {
        Self { min_length: 1, max_length: 20_000, ignore_whitespace_only: true }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub popup: Popup,
    pub selection: Selection,
    pub applications: Applications,
}

/// One configurable option an action declares — PopClip's extension options.
///
/// Values live in the manifest itself, under `value`, written there by the
/// settings window; `default` covers a fresh install. Everything is a string:
/// options exist to be spliced into `url` and `exec` templates through
/// `{{option:NAME}}`, and strings are what templates take.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionSpec {
    /// Human label, shown in the settings window.
    pub label: String,
    /// The value used until one is chosen.
    #[serde(default)]
    pub default: String,
    /// When non-empty, the value must be one of these; rendered as a dropdown.
    #[serde(default)]
    pub choices: Vec<String>,
    /// The chosen value.
    #[serde(default)]
    pub value: Option<String>,
}

impl OptionSpec {
    /// The value expansion should use right now.
    pub fn effective(&self) -> &str {
        self.value.as_deref().unwrap_or(&self.default)
    }
}

/// An action exactly as written in its TOML manifest.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionSpec {
    /// Stable identifier, also the D-Bus handle sent to the GNOME Shell view.
    pub id: String,
    /// Button tooltip.
    pub title: String,
    /// Symbolic icon name from the current icon theme.
    #[serde(default)]
    pub icon: Option<String>,
    /// Short label drawn when no icon is set or the icon is missing.
    #[serde(default)]
    pub label: Option<String>,
    /// Lower sorts first.
    #[serde(default)]
    pub order: i32,
    /// Only offer this action when the selection matches this regex.
    #[serde(default, rename = "match")]
    pub match_re: Option<String>,
    /// Suppress this action when the selection matches this regex.
    #[serde(default)]
    pub not_match: Option<String>,
    /// Only offer this action when the selection was classified as one of
    /// these kinds — `url`, `email`, `path`, `phone`, `color`. Any listed kind
    /// suffices; composes with `match`/`not_match`, which must also hold.
    #[serde(default)]
    pub detects: Vec<Detect>,
    /// A behavior grabit implements itself. Excludes `url` and `exec`.
    #[serde(default)]
    pub builtin: Option<Builtin>,
    /// Configurable options, spliced into templates as `{{option:NAME}}`.
    #[serde(default)]
    pub options: BTreeMap<String, OptionSpec>,
    /// Open this URL template with the desktop handler.
    #[serde(default)]
    pub url: Option<String>,
    /// Run this argv. Not a shell string: each element is a separate argument,
    /// so selected text can never be reinterpreted as shell syntax.
    #[serde(default)]
    pub exec: Option<Vec<String>>,
    /// Feed the selection to the command on stdin instead of expanding it into
    /// the argv. Required for text large enough to blow the ARG_MAX limit.
    #[serde(default)]
    pub stdin: bool,
    /// What to do with stdout once the command exits.
    #[serde(default)]
    pub after: After,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

/// An action with its regexes compiled and its source file remembered.
#[derive(Debug, Clone)]
pub struct Action {
    pub spec: ActionSpec,
    pub match_re: Option<Regex>,
    pub not_match: Option<Regex>,
    pub origin: PathBuf,
}

impl Action {
    /// Whether this action should be offered for `text`, classified as `class`.
    pub fn matches(&self, text: &str, class: &Classification) -> bool {
        if !self.spec.enabled {
            return false;
        }
        if !self.spec.detects.is_empty()
            && !self.spec.detects.iter().any(|&kind| class.get(kind).is_some())
        {
            return false;
        }
        if let Some(re) = &self.match_re
            && !re.is_match(text)
        {
            return false;
        }
        if let Some(re) = &self.not_match
            && re.is_match(text)
        {
            return false;
        }
        true
    }
}

/// Everything loaded from disk, ready to use.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub actions: Vec<Action>,
}

/// `$XDG_CONFIG_HOME/grabit`, falling back to `~/.config/grabit`.
pub fn user_config_dir() -> Result<PathBuf> {
    Ok(dirs::config_dir().context("no XDG config directory available")?.join("grabit"))
}

/// Load config and actions, installing the packaged defaults into the user
/// config directory on first run so the actions are immediately editable.
pub fn load() -> Result<Loaded> {
    let user_dir = user_config_dir()?;
    if !user_dir.exists() {
        install_defaults(&user_dir)
            .with_context(|| format!("seeding default config in {}", user_dir.display()))?;
    }

    let config = load_config(&user_dir.join("config.toml"))?;

    // System actions first so that a user file with the same `id` replaces it.
    let mut by_id: BTreeMap<String, Action> = BTreeMap::new();
    for dir in [PathBuf::from(SYSTEM_DATA_DIR).join("actions"), user_dir.join("actions")] {
        for action in load_actions_dir(&dir)? {
            by_id.insert(action.spec.id.clone(), action);
        }
    }

    let mut actions: Vec<Action> = by_id.into_values().collect();
    actions.sort_by(|a, b| {
        a.spec.order.cmp(&b.spec.order).then_with(|| a.spec.id.cmp(&b.spec.id))
    });

    Ok(Loaded { config, actions })
}

fn load_config(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// Read every `*.toml` in `dir`. A missing directory is not an error — most
/// installs will have only one of the two action directories.
fn load_actions_dir(dir: &Path) -> Result<Vec<Action>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };

    let mut actions = Vec::new();
    for entry in entries {
        let path = entry.with_context(|| format!("reading {}", dir.display()))?.path();
        if path.extension().is_none_or(|e| e != "toml") {
            continue;
        }
        // One bad action file must not take down every other action, so parse
        // errors are reported and skipped rather than propagated.
        match load_action(&path) {
            Ok(action) => actions.push(action),
            Err(e) => log::error!("skipping action {}: {e:#}", path.display()),
        }
    }
    Ok(actions)
}

fn load_action(path: &Path) -> Result<Action> {
    let raw = std::fs::read_to_string(path).context("reading file")?;
    let mut action = parse_manifest(&raw)?;
    action.origin = path.to_path_buf();
    Ok(action)
}

/// Parse and validate one action manifest. Also the test the snippets flow
/// applies to a selection, which is why it is exposed beyond loading.
pub fn parse_manifest(raw: &str) -> Result<Action> {
    let spec: ActionSpec = toml::from_str(raw).context("parsing TOML")?;

    if spec.url.is_some() && spec.exec.is_some() {
        anyhow::bail!("action `{}` sets both `url` and `exec`; pick one", spec.id);
    }
    if spec.builtin.is_some() && (spec.url.is_some() || spec.exec.is_some()) {
        anyhow::bail!("action `{}` is a `builtin`, so `url` and `exec` do not apply", spec.id);
    }
    if spec.builtin.is_some() && spec.after != After::Ignore {
        anyhow::bail!("action `{}` is a `builtin`, so `after` has no effect", spec.id);
    }
    if spec.builtin.is_none()
        && spec.url.is_none()
        && spec.exec.is_none()
        && spec.after == After::Ignore
    {
        // Neither a command nor a URL means "the selection is the result", which
        // is only meaningful if `after` then does something with it.
        anyhow::bail!(
            "action `{}` has no `url`, no `exec` and no `after`, so it would do nothing",
            spec.id
        );
    }
    if spec.url.is_some() && spec.after != After::Ignore {
        anyhow::bail!("action `{}` is a `url` action, so `after` has no effect", spec.id);
    }
    if spec.stdin && spec.exec.is_none() {
        anyhow::bail!("action `{}` sets `stdin` but has no `exec`", spec.id);
    }
    for (name, option) in &spec.options {
        if !option.choices.is_empty() {
            if !option.default.is_empty() && !option.choices.contains(&option.default) {
                anyhow::bail!(
                    "action `{}`: option `{name}` has a default outside its choices",
                    spec.id
                );
            }
            if let Some(value) = &option.value
                && !option.choices.contains(value)
            {
                anyhow::bail!(
                    "action `{}`: option `{name}` has a value outside its choices",
                    spec.id
                );
            }
        }
    }

    let compile = |src: &Option<String>, field: &str| -> Result<Option<Regex>> {
        match src {
            None => Ok(None),
            Some(s) => Regex::new(s)
                .map(Some)
                .with_context(|| format!("action `{}`: bad `{field}` regex", spec.id)),
        }
    };
    let match_re = compile(&spec.match_re, "match")?;
    let not_match = compile(&spec.not_match, "not_match")?;

    Ok(Action { spec, match_re, not_match, origin: PathBuf::new() })
}

/// The action manifests compiled into the binary, used to seed a fresh config.
const DEFAULT_ACTIONS: &[(&str, &str)] = &[
    ("copy.toml", include_str!("../actions/copy.toml")),
    ("cut.toml", include_str!("../actions/cut.toml")),
    ("paste.toml", include_str!("../actions/paste.toml")),
    ("search.toml", include_str!("../actions/search.toml")),
    ("open-link.toml", include_str!("../actions/open-link.toml")),
    ("open-path.toml", include_str!("../actions/open-path.toml")),
    ("mail.toml", include_str!("../actions/mail.toml")),
    ("dial.toml", include_str!("../actions/dial.toml")),
    ("define.toml", include_str!("../actions/define.toml")),
    ("translate.toml", include_str!("../actions/translate.toml")),
    ("upper.toml", include_str!("../actions/upper.toml")),
    ("lower.toml", include_str!("../actions/lower.toml")),
    ("shell.toml", include_str!("../actions/shell.toml")),
];

const DEFAULT_CONFIG: &str = include_str!("../data/config.toml");

fn install_defaults(dir: &Path) -> Result<()> {
    let actions_dir = dir.join("actions");
    std::fs::create_dir_all(&actions_dir)?;
    std::fs::write(dir.join("config.toml"), DEFAULT_CONFIG)?;
    for (name, body) in DEFAULT_ACTIONS {
        std::fs::write(actions_dir.join(name), body)?;
    }
    log::info!("seeded default configuration in {}", dir.display());
    Ok(())
}
