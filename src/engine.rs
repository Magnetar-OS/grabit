// SPDX-License-Identifier: GPL-3.0-only
//! The desktop-independent half of grabit: which actions apply to a selection,
//! and what happens when one is invoked.
//!
//! Both front-ends (the layer-shell popup and the GNOME Shell bridge) drive
//! this. Nothing here knows how the bar is drawn.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};

use crate::actions::{self, Expansion, Outcome};
use crate::classify;
use crate::clipboard;
use crate::config::{self, After, Builtin, Config, Loaded};
use crate::inject::Injector;
use crate::selection::Grab;

/// The id of the synthetic action the snippets flow offers when the selection
/// itself is an action manifest. Reserved: a manifest may not claim it.
pub const INSTALL_ID: &str = "grabit.install";

/// The subset of an action a front-end needs in order to draw a button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    pub id: String,
    pub title: String,
    pub icon: String,
    pub label: String,
}

/// Something an action produced that the front-end should present, arriving
/// after the invocation returned — the result view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Feedback {
    /// Show this text in the bar, titled with the action that produced it.
    /// `ticket` is the one [`Engine::invoke`] returned for the invocation, so
    /// a front-end can tell a result for its current bar from a late one.
    Result { ticket: u64, title: String, body: String },
}

struct Inner {
    loaded: RwLock<Loaded>,
    /// `None` when the session has no way to synthesise keystrokes; actions
    /// that need one then fail loudly rather than silently doing half the job.
    injector: Mutex<Option<Box<dyn Injector>>>,
    /// Whether an injector exists, readable without taking the mutex — the
    /// button pass needs it on every selection.
    can_inject: bool,
    feedback: async_channel::Sender<Feedback>,
    /// The last ticket handed out by `invoke`.
    tickets: AtomicU64,
}

#[derive(Clone)]
pub struct Engine(Arc<Inner>);

impl Engine {
    pub fn new(
        loaded: Loaded,
        injector: Option<Box<dyn Injector>>,
        feedback: async_channel::Sender<Feedback>,
    ) -> Self {
        let can_inject = injector.is_some();
        Self(Arc::new(Inner {
            loaded: RwLock::new(loaded),
            injector: Mutex::new(injector),
            can_inject,
            feedback,
            tickets: AtomicU64::new(0),
        }))
    }

    pub fn config(&self) -> Config {
        self.0.loaded.read().expect("config lock poisoned").config.clone()
    }

    /// What decides whether a selection is worth a bar, and how long it must
    /// hold still first — read live, so a reload applies to the next one.
    pub fn selection_limits(&self) -> (config::Selection, std::time::Duration) {
        let loaded = self.0.loaded.read().expect("config lock poisoned");
        let config = &loaded.config;
        (config.selection.clone(), std::time::Duration::from_millis(config.popup.settle_ms))
    }

    /// Re-read config and actions from disk.
    pub fn reload(&self) -> Result<()> {
        let fresh = config::load().context("reloading configuration")?;
        let count = fresh.actions.len();
        *self.0.loaded.write().expect("config lock poisoned") = fresh;
        log::info!("reloaded configuration ({count} actions)");
        Ok(())
    }

    /// Actions that apply to `text`, ordered. The front-end owns any paging.
    ///
    /// Builtins are keystroke injection; where the session has none they are
    /// omitted entirely rather than offered and left to fail. When the
    /// selection is itself an action manifest, the install offer leads.
    pub fn buttons(&self, text: &str) -> Vec<Button> {
        let class = classify::classify_full(text);
        let loaded = self.0.loaded.read().expect("config lock poisoned");

        let mut buttons = Vec::new();
        if class.manifest {
            buttons.push(Button {
                id: INSTALL_ID.to_owned(),
                title: crate::fl!("install-action"),
                icon: "document-save-symbolic".to_owned(),
                label: crate::fl!("install-action-label"),
            });
        }

        buttons.extend(
            loaded
                .actions
                .iter()
                .filter(|a| a.matches(text, &class))
                .filter(|a| a.spec.builtin.is_none() || self.0.can_inject)
                .map(|a| Button {
                    id: a.spec.id.clone(),
                    title: a.spec.title.clone(),
                    icon: a.spec.icon.clone().unwrap_or_default(),
                    label: a
                        .spec
                        .label
                        .clone()
                        .unwrap_or_else(|| a.spec.title.chars().take(2).collect()),
                }),
        );
        buttons
    }

    /// The `after` mode of the action with `id`, so a front-end can decide
    /// whether to keep the bar up for a result.
    pub fn action_after(&self, id: &str) -> Option<After> {
        let loaded = self.0.loaded.read().expect("config lock poisoned");
        loaded.actions.iter().find(|a| a.spec.id == id).map(|a| a.spec.after)
    }

    /// Run the action with `id` against the selection.
    ///
    /// Returns immediately: the action runs on a worker thread so a slow command
    /// cannot freeze the popup or the compositor's view of our surface. The
    /// returned ticket is carried by any [`Feedback`] the invocation produces.
    pub fn invoke(&self, id: &str, grab: Grab) -> u64 {
        let ticket = self.0.tickets.fetch_add(1, Ordering::Relaxed) + 1;
        if id == INSTALL_ID {
            let this = self.clone();
            spawn_worker(id, move || this.install(&grab.text));
            return ticket;
        }

        let Some(action) = self
            .0
            .loaded
            .read()
            .expect("config lock poisoned")
            .actions
            .iter()
            .find(|a| a.spec.id == id)
            .cloned()
        else {
            log::warn!("no action with id `{id}`");
            return ticket;
        };

        let this = self.clone();
        spawn_worker(id, move || {
            let outcome = match action.spec.builtin {
                Some(builtin) => this.run_builtin(builtin, &grab),
                None => {
                    let class = classify::classify_full(&grab.text);
                    actions::run(
                        &action,
                        &Expansion { grab: &grab, class: &class, options: &action.spec.options },
                    )
                    .and_then(|o| this.apply(ticket, &action.spec.title, o))
                }
            };
            match outcome {
                Ok(()) => log::debug!("action `{}` finished", action.spec.id),
                Err(e) => {
                    log::error!("action `{}` failed: {e:#}", action.spec.id);
                    notify(
                        &crate::fl!("action-failed", action = action.spec.title.clone()),
                        &format!("{e:#}"),
                    );
                }
            }
        });
        ticket
    }

    fn run_builtin(&self, builtin: Builtin, grab: &Grab) -> Result<()> {
        match builtin {
            Builtin::Cut => {
                clipboard::set(&grab.text)?;
                self.inject(|injector| injector.delete())
                    .context("the selection was copied but could not be deleted")
            }
            Builtin::Paste => {
                self.inject(|injector| injector.paste()).context("pasting the clipboard")
            }
        }
    }

    fn inject(&self, keystroke: impl FnOnce(&mut Box<dyn Injector>) -> Result<()>) -> Result<()> {
        let mut guard = self.0.injector.lock().expect("injector lock poisoned");
        let injector = guard.as_mut().context("this session cannot synthesise keystrokes")?;
        keystroke(injector)
    }

    fn apply(&self, ticket: u64, title: &str, outcome: Outcome) -> Result<()> {
        match outcome {
            Outcome::Nothing => Ok(()),
            Outcome::Clipboard(text) => clipboard::set(&text),
            Outcome::Replace(text) => {
                clipboard::set(&text)?;
                let mut guard = self.0.injector.lock().expect("injector lock poisoned");
                let injector = guard.as_mut().context(
                    "this session cannot synthesise keystrokes, so the replacement text \
                     was put on the clipboard but not pasted",
                )?;
                // The target application needs the clipboard offer to be live
                // before it asks for it; the flush inside `set` has happened,
                // but the compositor still has to route the new selection.
                std::thread::sleep(std::time::Duration::from_millis(40));
                injector.paste().context("pasting the replacement text")
            }
            Outcome::Show(body) => {
                let feedback = Feedback::Result { ticket, title: title.to_owned(), body };
                self.0
                    .feedback
                    .send_blocking(feedback)
                    .context("the front-end stopped taking results")
            }
        }
    }

    /// The snippets flow: the selection is an action manifest; write it into
    /// the user's actions directory and load it.
    ///
    /// A manifest that executes anything is installed disabled — one click must
    /// never turn selected text into something that runs.
    fn install(&self, manifest: &str) {
        match self.try_install(manifest) {
            Ok((id, disabled)) => {
                let body = if disabled {
                    crate::fl!("install-done-disabled", id = id.clone())
                } else {
                    crate::fl!("install-done", id = id.clone())
                };
                notify(&crate::fl!("install-title"), &body);
            }
            Err(e) => {
                log::error!("installing the selected action failed: {e:#}");
                notify(&crate::fl!("install-failed"), &format!("{e:#}"));
            }
        }
    }

    fn try_install(&self, manifest: &str) -> Result<(String, bool)> {
        let action = config::parse_manifest(manifest).context("the selection stopped parsing")?;
        let id = action.spec.id.clone();
        anyhow::ensure!(id != INSTALL_ID, "`{INSTALL_ID}` is reserved");
        anyhow::ensure!(
            id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'),
            "action id `{id}` is not a safe file name"
        );
        // Installing never replaces anything: not a customised user action,
        // and not a packaged one, which a same-id user file would override.
        let taken = self
            .0
            .loaded
            .read()
            .expect("config lock poisoned")
            .actions
            .iter()
            .any(|a| a.spec.id == id);
        anyhow::ensure!(
            !taken,
            "an action with the id `{id}` already exists; change the id in the selection, \
             or edit the existing action instead"
        );

        // Anything that executes or expands into a shell-adjacent context is
        // written disabled, whatever the manifest claimed.
        let must_disable = action.spec.exec.is_some() && action.spec.enabled;
        let body = if must_disable { disable_in_manifest(manifest)? } else { manifest.to_owned() };

        let dir = config::user_config_dir()?.join("actions");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{id}.toml"));
        // A file of that name may hold an action with another id, or one that
        // failed to load; it is not ours to replace either.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        std::io::Write::write_all(&mut file, body.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        log::info!("installed action `{id}` at {}", path.display());

        self.reload()?;
        Ok((id, must_disable || !action.spec.enabled))
    }
}

/// Rewrite a manifest so it loads disabled, whether or not it already carried
/// an `enabled` key.
///
/// Edited as TOML rather than as lines: `enabled` is a top-level key, and a
/// manifest that declares options ends in an `[options.*]` table, so a line
/// appended at the end would land inside that table instead.
fn disable_in_manifest(manifest: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = manifest.parse().context("parsing the manifest")?;
    let fresh = !doc.contains_key("enabled");
    doc["enabled"] = toml_edit::value(false);
    if fresh && let Some(mut key) = doc.as_table_mut().key_mut("enabled") {
        key.leaf_decor_mut()
            .set_prefix("# Installed from a selection; review it, then enable it.\n");
    }
    Ok(doc.to_string())
}

fn spawn_worker(id: &str, work: impl FnOnce() + Send + 'static) {
    let spawned = std::thread::Builder::new().name(format!("grabit-action-{id}")).spawn(work);
    if let Err(e) = spawned {
        log::error!("could not spawn worker for action `{id}`: {e}");
    }
}

/// Surface a failure to the user. An action that silently does nothing is
/// indistinguishable from a bug in the popup itself.
fn notify(summary: &str, body: &str) {
    if let Err(e) = try_notify(summary, body) {
        log::debug!("could not post a desktop notification: {e:#}");
    }
}

fn try_notify(summary: &str, body: &str) -> Result<()> {
    use std::collections::HashMap;
    use zbus::zvariant::Value;

    let conn = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )?;
    let hints: HashMap<&str, Value> = HashMap::new();
    let _: u32 = proxy.call(
        "Notify",
        &("grabit", 0u32, "dialog-error-symbolic", summary, body, &[] as &[&str], hints, 5000i32),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Installing a selected manifest must not silently replace an action the
    /// user already has.
    #[test]
    fn installing_refuses_an_id_that_is_already_taken() {
        let existing = crate::config::parse_manifest(
            "id = \"search\"\ntitle = \"Mine\"\nurl = \"https://example.org/{{text}}\"\n",
        )
        .expect("a valid manifest");
        let loaded =
            Loaded { config: Config::default(), actions: vec![existing], skipped: Vec::new() };
        let (feedback, _) = async_channel::bounded(1);
        let engine = Engine::new(loaded, None, feedback);

        let error = engine
            .try_install(
                "id = \"search\"\ntitle = \"Theirs\"\nurl = \"https://evil.example/{{text}}\"\n",
            )
            .expect_err("a taken id was installed over");
        assert!(format!("{error:#}").contains("already exists"));
    }

    #[test]
    fn disabling_rewrites_an_existing_enabled_key() {
        let manifest = "id = \"x\"\ntitle = \"X\"\nexec = [\"true\"]\nenabled = true\n";
        let out = disable_in_manifest(manifest).expect("a manifest that parsed");
        assert!(out.contains("enabled = false"));
        assert!(!out.contains("enabled = true"));
    }

    #[test]
    fn disabling_appends_when_no_enabled_key_exists() {
        let manifest = "id = \"x\"\ntitle = \"X\"\nexec = [\"true\"]";
        let out = disable_in_manifest(manifest).expect("a manifest that parsed");
        assert!(out.ends_with("enabled = false\n"));
        // The result must still be a valid manifest.
        assert!(crate::config::parse_manifest(&out).is_ok());
    }

    /// A manifest that declares options ends in an `[options.*]` table, so a
    /// line appended at the end lands inside that table rather than at the top
    /// level.
    #[test]
    fn disabling_a_manifest_that_ends_in_a_table_still_disables_it() {
        let manifest = "id = \"greet\"\ntitle = \"Greet\"\nexec = [\"echo\", \"{{option:word}}\"]\n\
                        after = \"copy\"\n\n[options.word]\nlabel = \"Word\"\ndefault = \"hi\"\n";
        let out = disable_in_manifest(manifest).expect("a manifest that parsed");
        let action = crate::config::parse_manifest(&out).expect("still a loadable manifest");
        assert!(!action.spec.enabled);
        assert_eq!(action.spec.options["word"].default, "hi");
    }
}
