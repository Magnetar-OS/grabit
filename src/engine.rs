//! The desktop-independent half of grabit: which actions apply to a selection,
//! and what happens when one is invoked.
//!
//! Both front-ends (the layer-shell popup and the GNOME Shell bridge) drive
//! this. Nothing here knows how the bar is drawn.

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
    Result { title: String, body: String },
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
        }))
    }

    pub fn config(&self) -> Config {
        self.0.loaded.read().expect("config lock poisoned").config.clone()
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
    /// cannot freeze the popup or the compositor's view of our surface.
    pub fn invoke(&self, id: &str, grab: Grab) {
        if id == INSTALL_ID {
            let this = self.clone();
            spawn_worker(id, move || this.install(&grab.text));
            return;
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
            return;
        };

        let this = self.clone();
        spawn_worker(id, move || {
            let outcome = match action.spec.builtin {
                Some(builtin) => this.run_builtin(builtin, &grab),
                None => {
                    let class = classify::classify_full(&grab.text);
                    actions::run(&action, &Expansion { grab: &grab, class: &class })
                        .and_then(|o| this.apply(&action.spec.title, o))
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
        let injector =
            guard.as_mut().context("this session cannot synthesise keystrokes")?;
        keystroke(injector)
    }

    fn apply(&self, title: &str, outcome: Outcome) -> Result<()> {
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
                let feedback = Feedback::Result { title: title.to_owned(), body };
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

        // Anything that executes or expands into a shell-adjacent context is
        // written disabled, whatever the manifest claimed.
        let must_disable = action.spec.exec.is_some() && action.spec.enabled;
        let body = if must_disable { disable_in_manifest(manifest) } else { manifest.to_owned() };

        let dir = config::user_config_dir()?.join("actions");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{id}.toml"));
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        log::info!("installed action `{id}` at {}", path.display());

        self.reload()?;
        Ok((id, must_disable || !action.spec.enabled))
    }
}

/// Rewrite a manifest so it loads disabled, whether or not it already carried
/// an `enabled` key.
fn disable_in_manifest(manifest: &str) -> String {
    let mut lines: Vec<String> = manifest.lines().map(str::to_owned).collect();
    let mut rewritten = false;
    for line in &mut lines {
        if line.trim_start().starts_with("enabled") && line.contains('=') {
            *line = "enabled = false".to_owned();
            rewritten = true;
        }
    }
    if !rewritten {
        lines.push("# Installed from a selection; review it, then enable it.".to_owned());
        lines.push("enabled = false".to_owned());
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

fn spawn_worker(id: &str, work: impl FnOnce() + Send + 'static) {
    let spawned =
        std::thread::Builder::new().name(format!("grabit-action-{id}")).spawn(work);
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

    #[test]
    fn disabling_rewrites_an_existing_enabled_key() {
        let manifest = "id = \"x\"\ntitle = \"X\"\nexec = [\"true\"]\nenabled = true\n";
        let out = disable_in_manifest(manifest);
        assert!(out.contains("enabled = false"));
        assert!(!out.contains("enabled = true"));
    }

    #[test]
    fn disabling_appends_when_no_enabled_key_exists() {
        let manifest = "id = \"x\"\ntitle = \"X\"\nexec = [\"true\"]";
        let out = disable_in_manifest(manifest);
        assert!(out.ends_with("enabled = false\n"));
        // The result must still be a valid manifest.
        assert!(crate::config::parse_manifest(&out).is_ok());
    }
}
