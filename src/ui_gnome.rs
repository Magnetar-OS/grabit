//! The GNOME front-end.
//!
//! Mutter implements neither data-control nor layer-shell, and will not: there
//! is no protocol-level way for a client to watch the selection or to place a
//! surface at a point on screen. The only supported extension point is GNOME
//! Shell itself, so on GNOME the shell extension owns both halves — it watches
//! the selection, draws the bar and synthesises the paste — while this process
//! stays the single source of truth for *which* actions exist and what they do.
//!
//! The extension exports `org.grabit.Shell`; see `gnome-extension/`.

use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;

use crate::control::Command;
use crate::engine::{Engine, Feedback};
use crate::inject::Injector;
use crate::selection::{self, Grab, Raw, Settled};

pub const SHELL_BUS_NAME: &str = "org.grabit.Shell";
pub const SHELL_OBJECT_PATH: &str = "/org/grabit/Shell";
pub const SHELL_INTERFACE: &str = "org.grabit.Shell1";

/// Whether the GNOME Shell extension is loaded and listening.
pub fn is_available() -> bool {
    match shell_owner_exists() {
        Ok(present) => present,
        Err(e) => {
            log::debug!("could not query the session bus: {e:#}");
            false
        }
    }
}

fn shell_owner_exists() -> Result<bool> {
    let connection = zbus::blocking::Connection::session()?;
    let dbus = zbus::blocking::Proxy::new(
        &connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )?;
    let owned: bool = dbus.call("NameHasOwner", &(SHELL_BUS_NAME,))?;
    Ok(owned)
}

/// Paste by asking the shell extension to do it.
///
/// GNOME Shell can drive a `Clutter` virtual input device directly, which needs
/// no portal and therefore no consent dialog.
pub struct ShellInjector {
    connection: zbus::blocking::Connection,
}

impl ShellInjector {
    pub fn new() -> Result<Self> {
        Ok(Self {
            connection: zbus::blocking::Connection::session()
                .context("connecting to the session bus")?,
        })
    }
}

impl Injector for ShellInjector {
    fn paste(&mut self) -> Result<()> {
        proxy(&self.connection)?
            .call::<_, _, ()>("Paste", &())
            .context("asking the GNOME Shell extension to paste")?;
        Ok(())
    }

    fn delete(&mut self) -> Result<()> {
        proxy(&self.connection)?
            .call::<_, _, ()>("Delete", &())
            .context("asking the GNOME Shell extension to delete the selection")?;
        Ok(())
    }
}

fn proxy(connection: &zbus::blocking::Connection) -> Result<zbus::blocking::Proxy<'_>> {
    zbus::blocking::Proxy::new(connection, SHELL_BUS_NAME, SHELL_OBJECT_PATH, SHELL_INTERFACE)
        .context("creating the shell proxy")
}

pub fn run(
    engine: Engine,
    commands: async_channel::Receiver<Command>,
    feedback: async_channel::Receiver<Feedback>,
) -> Result<()> {
    let connection =
        zbus::blocking::Connection::session().context("connecting to the session bus")?;

    let (selection_tx, selection_rx) = async_channel::bounded(16);
    let (action_tx, action_rx) = async_channel::bounded(16);

    // Protocol version of the installed extension. Version 2 adds the focused
    // app id to selection reports (per-app rules), `Delete` and `ShowResult`.
    let version = extension_version(&connection);
    log::info!("shell extension protocol version {version}");

    if version >= 2 {
        let exclusions = engine.clone();
        spawn_signal_relay(&connection, "SelectionChangedV2", move |message| {
            let (text, app): (String, String) = match message.body().deserialize() {
                Ok(pair) => pair,
                Err(e) => {
                    log::warn!("malformed SelectionChangedV2 signal: {e}");
                    return;
                }
            };
            // An excluded app's selection must also take an existing bar down,
            // so it is reported as cleared rather than swallowed.
            let excluded =
                !app.is_empty() && exclusions.config().applications.is_excluded(&app);
            let event = if text.is_empty() || excluded {
                Raw::Cleared
            } else {
                Raw::Selection { generation: 1, grab: Grab::text(text) }
            };
            let _ = selection_tx.send_blocking(event);
        })?;
    } else {
        spawn_signal_relay(&connection, "SelectionChanged", move |message| {
            let text: String = match message.body().deserialize() {
                Ok(text) => text,
                Err(e) => {
                    log::warn!("malformed SelectionChanged signal: {e}");
                    return;
                }
            };
            let event = if text.is_empty() {
                Raw::Cleared
            } else {
                // The shell is the only source of these events and delivers them
                // in order, so a constant generation is accurate.
                Raw::Selection { generation: 1, grab: Grab::text(text) }
            };
            let _ = selection_tx.send_blocking(event);
        })?;
    }

    spawn_signal_relay(&connection, "ActionInvoked", move |message| {
        match message.body().deserialize::<String>() {
            Ok(id) => {
                let _ = action_tx.send_blocking(id);
            }
            Err(e) => log::warn!("malformed ActionInvoked signal: {e}"),
        }
    })?;

    let config = engine.config();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .context("starting the async runtime")?;

    runtime.block_on(async move {
        let mut selections = Box::pin(selection::settled(
            selection_rx,
            config.selection.clone(),
            Duration::from_millis(config.popup.settle_ms),
        ));

        // The selection the visible bar acts on. Single-threaded runtime, so a
        // plain local is enough.
        let mut showing = Grab::default();

        loop {
            tokio::select! {
                Some(event) = selections.next() => match event {
                    Settled::Text(grab) => {
                        let buttons = engine.buttons(&grab.text);
                        if buttons.is_empty() {
                            log::debug!("no action matches this selection");
                            continue;
                        }
                        showing = grab;
                        if let Err(e) = show_popup(&connection, &buttons) {
                            log::error!("{e:#}");
                        }
                    }
                    Settled::Cleared => {
                        showing = Grab::default();
                        if let Err(e) = hide_popup(&connection) {
                            log::warn!("{e:#}");
                        }
                    }
                },

                Ok(id) = action_rx.recv() => {
                    engine.invoke(&id, std::mem::take(&mut showing));
                }

                Ok(feedback) = feedback.recv() => match feedback {
                    Feedback::Result { title, body } => {
                        if version >= 2 {
                            if let Err(e) = show_result(&connection, &title, &body) {
                                log::error!("{e:#}");
                            }
                        } else {
                            // A v1 extension has no result view; a notification
                            // is the closest it can come.
                            log::info!("result from “{title}”: {body}");
                        }
                    }
                },

                Ok(command) = commands.recv() => match command {
                    Command::Show => match crate::control::current_primary_selection() {
                        Ok(text)
                            if selection::is_interesting(&text, &engine.config().selection) =>
                        {
                            let buttons = engine.buttons(&text);
                            showing = Grab::text(text);
                            if let Err(e) = show_popup(&connection, &buttons) {
                                log::error!("{e:#}");
                            }
                        }
                        Ok(_) => log::info!("nothing selected"),
                        Err(e) => log::error!("reading the primary selection: {e:#}"),
                    },
                    Command::Hide => {
                        if let Err(e) = hide_popup(&connection) {
                            log::warn!("{e:#}");
                        }
                    }
                    Command::Reload => {
                        if let Err(e) = engine.reload() {
                            log::error!("{e:#}");
                        }
                    }
                    Command::Quit => {
                        let _ = hide_popup(&connection);
                        return;
                    }
                },

                else => return,
            }
        }
    });

    Ok(())
}

fn show_popup(
    connection: &zbus::blocking::Connection,
    buttons: &[crate::engine::Button],
) -> Result<()> {
    let payload: Vec<(String, String, String, String)> = buttons
        .iter()
        .map(|b| (b.id.clone(), b.title.clone(), b.icon.clone(), b.label.clone()))
        .collect();
    proxy(connection)?
        .call::<_, _, ()>("ShowPopup", &(payload,))
        .context("asking the GNOME Shell extension to show the bar")?;
    Ok(())
}

fn hide_popup(connection: &zbus::blocking::Connection) -> Result<()> {
    proxy(connection)?
        .call::<_, _, ()>("HidePopup", &())
        .context("asking the GNOME Shell extension to hide the bar")?;
    Ok(())
}

fn show_result(connection: &zbus::blocking::Connection, title: &str, body: &str) -> Result<()> {
    proxy(connection)?
        .call::<_, _, ()>("ShowResult", &(title, body))
        .context("asking the GNOME Shell extension to show a result")?;
    Ok(())
}

/// The extension's protocol version, defaulting to 1 for extensions that
/// predate the property. The daemon and the extension update independently, so
/// every capability past the original set is gated on this.
fn extension_version(connection: &zbus::blocking::Connection) -> u32 {
    let version = proxy(connection).and_then(|proxy| {
        proxy.get_property::<u32>("Version").context("reading the Version property")
    });
    match version {
        Ok(version) => version,
        Err(e) => {
            log::debug!("no Version property ({e:#}); assuming protocol 1");
            1
        }
    }
}

/// Forward one D-Bus signal onto a channel from a dedicated thread.
///
/// The blocking signal iterator cannot share a thread with the async runtime, so
/// each subscription gets its own.
fn spawn_signal_relay(
    connection: &zbus::blocking::Connection,
    signal: &'static str,
    on_message: impl Fn(zbus::Message) + Send + 'static,
) -> Result<()> {
    // A proxy borrows its connection, so it is built inside the thread from an
    // owned clone rather than moved across the boundary.
    let connection = connection.clone();

    std::thread::Builder::new()
        .name(format!("grabit-{signal}"))
        .spawn(move || {
            let proxy = match proxy(&connection) {
                Ok(proxy) => proxy,
                Err(e) => {
                    log::error!("cannot reach the shell extension: {e:#}");
                    return;
                }
            };
            let stream = match proxy.receive_signal(signal) {
                Ok(stream) => stream,
                Err(e) => {
                    log::error!("cannot subscribe to {signal}: {e}");
                    return;
                }
            };
            for message in stream {
                on_message(message);
            }
            log::warn!("the {signal} subscription ended; is the extension still enabled?");
        })
        .with_context(|| format!("spawning the {signal} relay"))?;
    Ok(())
}
