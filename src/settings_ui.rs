// SPDX-License-Identifier: GPL-3.0-only
//! The settings window (`grabit settings`).
//!
//! TOML-first is the design: everything this window changes is written into
//! the same files the user could have edited by hand — `config.toml` through
//! `toml_edit`, so comments and formatting survive, and each action's own
//! manifest for the enable/disable toggles and the ordering. There is no
//! second configuration store to drift out of sync.
//!
//! Every change also pokes a running daemon over D-Bus, so edits apply live;
//! when no daemon is running the poke is quietly skipped and the files are
//! simply there for the next start.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use cosmic::app::{Core, Settings, Task};
use cosmic::iced::Length;
use cosmic::widget::{self, settings};
use cosmic::{Element, theme};
use toml_edit::{DocumentMut, value};

use crate::config::{self, Action, Config};

pub fn run() -> Result<()> {
    let settings = Settings::default().size(cosmic::iced::Size::new(560.0, 720.0));
    cosmic::app::run::<App>(settings, ()).context("running the settings window")
}

#[derive(Debug, Clone)]
pub enum Message {
    SettleMs(u32),
    PerPage(u32),
    IconSize(u32),
    OffsetX(i32),
    OffsetY(i32),
    DismissMs(u32),
    TimeoutMs(u32),
    Animate(bool),
    MinLength(u32),
    MaxLength(u32),
    IgnoreWhitespace(bool),
    ExcludeInput(String),
    ExcludeAdd,
    ExcludeRemove(usize),
    ActionEnabled(usize, bool),
    ActionMove(usize, i32),
    ActionEdit(usize),
    ActionOptionInput(usize, String, String),
    ActionOptionCommit(usize, String),
    ActionOptionChoice(usize, String, String),
    OpenFolder,
}

struct App {
    core: Core,
    config: Config,
    actions: Vec<Action>,
    exclude_input: String,
    /// Half-typed free-text option values, keyed by action id and option name.
    ///
    /// A manifest write per keystroke would rewrite the file and poke the
    /// daemon on every character, so a free-text option is buffered here and
    /// committed on Enter. Dropdowns commit immediately: there is nothing to
    /// half-type.
    option_edits: BTreeMap<(String, String), String>,
}

impl App {
    fn refresh(&mut self) {
        match config::load() {
            Ok(loaded) => {
                self.config = loaded.config;
                self.actions = loaded.actions;
            }
            Err(e) => log::error!("reloading configuration: {e:#}"),
        }
    }

    /// Write one option's chosen value into the action's manifest and apply it.
    ///
    /// The value goes in as `[options.NAME] value`, which is where `config.rs`
    /// reads it from and what `{{option:NAME}}` then expands to.
    fn set_action_option(&mut self, index: usize, name: &str, chosen: &str) {
        let Some(action) = self.actions.get(index) else {
            return;
        };
        if let Err(e) = set_action_option(action, name, chosen) {
            log::error!("setting option `{name}` on `{}`: {e:#}", action.spec.id);
            return;
        }
        poke_daemon();
        self.refresh();
    }

    /// Persist the daemon settings and apply them to a running daemon.
    fn save_config(&self) {
        if let Err(e) = write_config(&self.config) {
            log::error!("saving config.toml: {e:#}");
            return;
        }
        poke_daemon();
    }

    fn popup_section(&self) -> Element<'_, Message> {
        let popup = &self.config.popup;
        settings::section()
            .title(crate::fl!("settings-popup"))
            .add(settings::item(
                crate::fl!("settings-settle"),
                widget::spin_button("", "", popup.settle_ms as u32, 20, 0, 2000, Message::SettleMs),
            ))
            .add(settings::item(
                crate::fl!("settings-per-page"),
                widget::spin_button("", "", popup.max_actions as u32, 1, 1, 24, Message::PerPage),
            ))
            .add(settings::item(
                crate::fl!("settings-icon-size"),
                widget::spin_button("", "", popup.icon_size as u32, 2, 8, 64, Message::IconSize),
            ))
            .add(settings::item(
                crate::fl!("settings-offset-x"),
                widget::spin_button("", "", popup.offset_x, 2, -64, 64, Message::OffsetX),
            ))
            .add(settings::item(
                crate::fl!("settings-offset-y"),
                widget::spin_button("", "", popup.offset_y, 2, -64, 64, Message::OffsetY),
            ))
            .add(settings::item(
                crate::fl!("settings-dismiss"),
                widget::spin_button(
                    "",
                    "",
                    popup.dismiss_ms as u32,
                    100,
                    0,
                    10_000,
                    Message::DismissMs,
                ),
            ))
            .add(settings::item(
                crate::fl!("settings-timeout"),
                widget::spin_button(
                    "",
                    "",
                    popup.timeout_ms as u32,
                    500,
                    0,
                    60_000,
                    Message::TimeoutMs,
                ),
            ))
            .add(settings::item(
                crate::fl!("settings-animate"),
                widget::toggler(popup.animate).on_toggle(Message::Animate),
            ))
            .into()
    }

    fn selection_section(&self) -> Element<'_, Message> {
        let selection = &self.config.selection;
        settings::section()
            .title(crate::fl!("settings-selection"))
            .add(settings::item(
                crate::fl!("settings-min-length"),
                widget::spin_button(
                    "",
                    "",
                    selection.min_length as u32,
                    1,
                    1,
                    1000,
                    Message::MinLength,
                ),
            ))
            .add(settings::item(
                crate::fl!("settings-max-length"),
                widget::spin_button(
                    "",
                    "",
                    selection.max_length as u32,
                    1000,
                    100,
                    1_000_000,
                    Message::MaxLength,
                ),
            ))
            .add(settings::item(
                crate::fl!("settings-ignore-whitespace"),
                widget::toggler(selection.ignore_whitespace_only)
                    .on_toggle(Message::IgnoreWhitespace),
            ))
            .into()
    }

    fn applications_section(&self) -> Element<'_, Message> {
        let mut section = settings::section()
            .title(crate::fl!("settings-apps"))
            .add(widget::text::caption(crate::fl!("settings-apps-hint")))
            .add(
                widget::row::with_capacity(2)
                    .spacing(theme::spacing().space_xxs)
                    .push(
                        widget::text_input(
                            crate::fl!("settings-exclude-placeholder"),
                            &self.exclude_input,
                        )
                        .on_input(Message::ExcludeInput)
                        .on_submit(|_| Message::ExcludeAdd),
                    )
                    .push(
                        widget::button::text(crate::fl!("settings-exclude-add"))
                            .on_press(Message::ExcludeAdd),
                    ),
            );

        for (index, entry) in self.config.applications.exclude.iter().enumerate() {
            section = section.add(settings::item(
                entry.clone(),
                widget::tooltip(
                    widget::button::custom(widget::icon::from_name("user-trash-symbolic").size(14))
                        .class(theme::Button::Icon)
                        .on_press(Message::ExcludeRemove(index)),
                    widget::text::body(crate::fl!("settings-remove")),
                    widget::tooltip::Position::Left,
                ),
            ));
        }
        section.into()
    }

    fn actions_section(&self) -> Element<'_, Message> {
        let spacing = theme::spacing();
        let mut section = settings::section()
            .title(crate::fl!("settings-actions"))
            .add(widget::text::caption(crate::fl!("settings-actions-hint")))
            .add(settings::item(
                crate::fl!("settings-open-folder"),
                widget::button::custom(widget::icon::from_name("folder-open-symbolic").size(14))
                    .class(theme::Button::Icon)
                    .on_press(Message::OpenFolder),
            ));

        let last = self.actions.len().saturating_sub(1);
        for (index, action) in self.actions.iter().enumerate() {
            let icon_button = |name: &str, tooltip: String, message: Option<Message>| {
                let mut button =
                    widget::button::custom(widget::icon::from_name(name.to_owned()).size(14))
                        .class(theme::Button::Icon);
                if let Some(message) = message {
                    button = button.on_press(message);
                }
                widget::tooltip(button, widget::text::body(tooltip), widget::tooltip::Position::Top)
            };

            let title = widget::column::with_capacity(2)
                .push(widget::text::body(action.spec.title.clone()))
                .push(widget::text::caption(action.spec.id.clone()));

            let controls = widget::row::with_capacity(4)
                .spacing(spacing.space_xxs)
                .align_y(cosmic::iced::Alignment::Center)
                .push(icon_button(
                    "go-up-symbolic",
                    crate::fl!("settings-move-up"),
                    (index > 0).then_some(Message::ActionMove(index, -1)),
                ))
                .push(icon_button(
                    "go-down-symbolic",
                    crate::fl!("settings-move-down"),
                    (index < last).then_some(Message::ActionMove(index, 1)),
                ))
                .push(icon_button(
                    "document-edit-symbolic",
                    crate::fl!("settings-edit-file"),
                    Some(Message::ActionEdit(index)),
                ))
                .push(
                    widget::toggler(action.spec.enabled)
                        .on_toggle(move |on| Message::ActionEnabled(index, on)),
                );

            section = section.add(
                widget::row::with_capacity(3)
                    .spacing(spacing.space_xxs)
                    .align_y(cosmic::iced::Alignment::Center)
                    .push(title)
                    .push(widget::space::horizontal().width(Length::Fill))
                    .push(controls),
            );

            // An action's own options, indented beneath it. `choices` is what
            // makes an option a dropdown; everything else is free text.
            for (name, option) in &action.spec.options {
                let control: Element<'_, Message> = if option.choices.is_empty() {
                    let key = (action.spec.id.clone(), name.clone());
                    let shown =
                        self.option_edits.get(&key).map_or(option.effective(), String::as_str);
                    let (input_name, commit_name) = (name.clone(), name.clone());
                    widget::text_input("", shown)
                        .on_input(move |text| {
                            Message::ActionOptionInput(index, input_name.clone(), text)
                        })
                        .on_submit(move |_| Message::ActionOptionCommit(index, commit_name.clone()))
                        .width(Length::Fixed(180.0))
                        .into()
                } else {
                    let selected = option.choices.iter().position(|c| c == option.effective());
                    let choices = option.choices.clone();
                    let name = name.clone();
                    widget::dropdown(&option.choices, selected, move |picked| {
                        Message::ActionOptionChoice(index, name.clone(), choices[picked].clone())
                    })
                    .into()
                };

                section = section.add(
                    widget::row::with_capacity(3)
                        .spacing(spacing.space_xxs)
                        .align_y(cosmic::iced::Alignment::Center)
                        .push(
                            widget::space::horizontal()
                                .width(Length::Fixed(f32::from(spacing.space_m))),
                        )
                        .push(widget::text::caption(option.label.clone()))
                        .push(widget::space::horizontal().width(Length::Fill))
                        .push(control),
                );
            }
        }
        section.into()
    }
}

impl cosmic::Application for App {
    type Executor = cosmic::executor::single::Executor;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = "io.github.idominikos.GrabitSettings";

    fn init(core: Core, (): ()) -> (Self, Task<Message>) {
        let loaded = config::load().unwrap_or_else(|e| {
            log::error!("loading configuration: {e:#}");
            config::Loaded { config: Config::default(), actions: Vec::new() }
        });
        let app = App {
            core,
            config: loaded.config,
            actions: loaded.actions,
            exclude_input: String::new(),
            option_edits: BTreeMap::new(),
        };
        (app, Task::none())
    }

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn view(&self) -> Element<'_, Message> {
        let spacing = theme::spacing();
        widget::scrollable(
            widget::container(settings::view_column(vec![
                self.popup_section(),
                self.selection_section(),
                self.applications_section(),
                self.actions_section(),
            ]))
            .padding(spacing.space_m)
            .max_width(640.0),
        )
        .width(Length::Fill)
        .into()
    }

    #[allow(clippy::too_many_lines)]
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SettleMs(v) => {
                self.config.popup.settle_ms = u64::from(v);
                self.save_config();
            }
            Message::PerPage(v) => {
                self.config.popup.max_actions = v as usize;
                self.save_config();
            }
            Message::IconSize(v) => {
                self.config.popup.icon_size = v as i32;
                self.save_config();
            }
            Message::OffsetX(v) => {
                self.config.popup.offset_x = v;
                self.save_config();
            }
            Message::OffsetY(v) => {
                self.config.popup.offset_y = v;
                self.save_config();
            }
            Message::DismissMs(v) => {
                self.config.popup.dismiss_ms = u64::from(v);
                self.save_config();
            }
            Message::TimeoutMs(v) => {
                self.config.popup.timeout_ms = u64::from(v);
                self.save_config();
            }
            Message::Animate(v) => {
                self.config.popup.animate = v;
                self.save_config();
            }
            Message::MinLength(v) => {
                self.config.selection.min_length = v as usize;
                self.save_config();
            }
            Message::MaxLength(v) => {
                self.config.selection.max_length = v as usize;
                self.save_config();
            }
            Message::IgnoreWhitespace(v) => {
                self.config.selection.ignore_whitespace_only = v;
                self.save_config();
            }
            Message::ExcludeInput(input) => self.exclude_input = input,
            Message::ExcludeAdd => {
                let entry = self.exclude_input.trim().to_owned();
                if !entry.is_empty() && !self.config.applications.exclude.contains(&entry) {
                    self.config.applications.exclude.push(entry);
                    self.exclude_input.clear();
                    self.save_config();
                }
            }
            Message::ExcludeRemove(index) => {
                if index < self.config.applications.exclude.len() {
                    self.config.applications.exclude.remove(index);
                    self.save_config();
                }
            }
            Message::ActionEnabled(index, enabled) => {
                if let Some(action) = self.actions.get(index) {
                    if let Err(e) = set_action_enabled(action, enabled) {
                        log::error!("toggling `{}`: {e:#}", action.spec.id);
                    }
                    poke_daemon();
                    self.refresh();
                }
            }
            Message::ActionMove(index, offset) => {
                let target = index as i32 + offset;
                if target >= 0 && (target as usize) < self.actions.len() {
                    self.actions.swap(index, target as usize);
                    if let Err(e) = renumber_actions(&self.actions) {
                        log::error!("reordering actions: {e:#}");
                    }
                    poke_daemon();
                    self.refresh();
                }
            }
            Message::ActionEdit(index) => {
                if let Some(action) = self.actions.get(index) {
                    // Editing a packaged action means editing the user's copy;
                    // materialise one if only the packaged file exists.
                    match user_copy_of(action) {
                        Ok(path) => open_path(&path),
                        Err(e) => log::error!("preparing `{}` for editing: {e:#}", action.spec.id),
                    }
                }
            }
            Message::ActionOptionInput(index, name, text) => {
                if let Some(action) = self.actions.get(index) {
                    self.option_edits.insert((action.spec.id.clone(), name), text);
                }
            }
            Message::ActionOptionCommit(index, name) => {
                if let Some(action) = self.actions.get(index) {
                    let key = (action.spec.id.clone(), name.clone());
                    if let Some(text) = self.option_edits.remove(&key) {
                        self.set_action_option(index, &name, &text);
                    }
                }
            }
            Message::ActionOptionChoice(index, name, choice) => {
                self.set_action_option(index, &name, &choice);
            }
            Message::OpenFolder => match config::user_config_dir() {
                Ok(dir) => open_path(&dir.join("actions")),
                Err(e) => log::error!("{e:#}"),
            },
        }
        Task::none()
    }
}

/// Write the `[popup]`, `[selection]` and `[applications]` tables into the
/// user's `config.toml`, preserving whatever else the file holds.
fn write_config(config: &Config) -> Result<()> {
    let path = config::user_config_dir()?.join("config.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: DocumentMut =
        raw.parse().with_context(|| format!("parsing {}", path.display()))?;

    let popup = &config.popup;
    doc["popup"]["settle_ms"] = value(popup.settle_ms as i64);
    doc["popup"]["max_actions"] = value(popup.max_actions as i64);
    doc["popup"]["icon_size"] = value(i64::from(popup.icon_size));
    doc["popup"]["offset_x"] = value(i64::from(popup.offset_x));
    doc["popup"]["offset_y"] = value(i64::from(popup.offset_y));
    doc["popup"]["dismiss_ms"] = value(popup.dismiss_ms as i64);
    doc["popup"]["timeout_ms"] = value(popup.timeout_ms as i64);
    doc["popup"]["animate"] = value(popup.animate);

    let selection = &config.selection;
    doc["selection"]["min_length"] = value(selection.min_length as i64);
    doc["selection"]["max_length"] = value(selection.max_length as i64);
    doc["selection"]["ignore_whitespace_only"] = value(selection.ignore_whitespace_only);

    let mut exclude = toml_edit::Array::new();
    for entry in &config.applications.exclude {
        exclude.push(entry.as_str());
    }
    doc["applications"]["exclude"] = value(exclude);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, doc.to_string()).with_context(|| format!("writing {}", path.display()))
}

/// Flip `enabled` inside an action's manifest, writing the user's copy.
fn set_action_enabled(action: &Action, enabled: bool) -> Result<()> {
    let raw = std::fs::read_to_string(&action.origin)
        .with_context(|| format!("reading {}", action.origin.display()))?;
    let mut doc: DocumentMut = raw.parse().context("parsing the manifest")?;
    doc["enabled"] = value(enabled);
    write_user_manifest(action, &doc.to_string())
}

/// Write `chosen` as an option's `value` in the action's manifest.
fn set_action_option(action: &Action, name: &str, chosen: &str) -> Result<()> {
    let raw = std::fs::read_to_string(&action.origin)
        .with_context(|| format!("reading {}", action.origin.display()))?;
    let mut doc: DocumentMut = raw.parse().context("parsing the manifest")?;
    // The option is declared in the manifest already — validation refuses an
    // undeclared one — so this only ever fills in its `value`.
    doc["options"][name]["value"] = value(chosen);
    write_user_manifest(action, &doc.to_string())
}

/// Rewrite every action's `order` to match its position in `actions`.
///
/// Positions are spaced by ten, the same convention the packaged actions use,
/// so a hand-written manifest can still slot between two without a renumber.
fn renumber_actions(actions: &[Action]) -> Result<()> {
    for (position, action) in actions.iter().enumerate() {
        let order = ((position + 1) * 10) as i64;
        let raw = std::fs::read_to_string(&action.origin)
            .with_context(|| format!("reading {}", action.origin.display()))?;
        let mut doc: DocumentMut = raw.parse().context("parsing the manifest")?;
        if doc.get("order").and_then(toml_edit::Item::as_integer) == Some(order) {
            continue;
        }
        doc["order"] = value(order);
        write_user_manifest(action, &doc.to_string())?;
    }
    Ok(())
}

/// Write `body` as the user's manifest for `action` — the origin file when it
/// already lives in the user directory, a fresh user copy when it is packaged.
fn write_user_manifest(action: &Action, body: &str) -> Result<()> {
    let path = user_manifest_path(action)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))
}

/// The user-directory path an action's manifest belongs at.
fn user_manifest_path(action: &Action) -> Result<PathBuf> {
    let dir = config::user_config_dir()?.join("actions");
    let file = action
        .origin
        .file_name()
        .map(std::path::Path::new)
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(format!("{}.toml", action.spec.id)));
    Ok(dir.join(file))
}

/// Make sure a user-editable copy of the manifest exists, and return its path.
fn user_copy_of(action: &Action) -> Result<PathBuf> {
    let path = user_manifest_path(action)?;
    if !path.exists() {
        let raw = std::fs::read_to_string(&action.origin)
            .with_context(|| format!("reading {}", action.origin.display()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(path)
}

fn open_path(path: &std::path::Path) {
    let uri = format!("file://{}", path.display());
    if let Err(e) = std::process::Command::new("xdg-open")
        .arg(&uri)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        log::error!("opening {uri}: {e}");
    }
}

/// Tell a running daemon to re-read what was just written. No daemon running
/// is not an error — the files are simply there for the next start.
fn poke_daemon() {
    if let Err(e) = crate::control::call("Reload") {
        log::debug!("no running daemon to reload: {e:#}");
    }
}
