// SPDX-License-Identifier: GPL-3.0-only
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
    zbus::block_on(async {
        let connection = zbus::Connection::session().await?;
        let dbus = zbus::Proxy::new(
            &connection,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .await?;
        let owned: bool = dbus.call("NameHasOwner", &(SHELL_BUS_NAME,)).await?;
        Ok(owned)
    })
}

/// Paste by asking the shell extension to do it.
///
/// GNOME Shell can drive a `Clutter` virtual input device directly, which needs
/// no portal and therefore no consent dialog.
///
/// Called from engine worker threads, outside any async runtime.
pub struct ShellInjector {
    connection: zbus::Connection,
}

impl ShellInjector {
    pub fn new() -> Result<Self> {
        Ok(Self {
            connection: zbus::block_on(zbus::Connection::session())
                .context("connecting to the session bus")?,
        })
    }
}

impl Injector for ShellInjector {
    fn paste(&mut self) -> Result<()> {
        zbus::block_on(async {
            proxy(&self.connection)
                .await?
                .call::<_, _, ()>("Paste", &())
                .await
                .context("asking the GNOME Shell extension to paste")
        })
    }

    fn delete(&mut self) -> Result<()> {
        zbus::block_on(async {
            proxy(&self.connection)
                .await?
                .call::<_, _, ()>("Delete", &())
                .await
                .context("asking the GNOME Shell extension to delete the selection")
        })
    }
}

async fn proxy(connection: &zbus::Connection) -> Result<zbus::Proxy<'static>> {
    zbus::Proxy::new(connection, SHELL_BUS_NAME, SHELL_OBJECT_PATH, SHELL_INTERFACE)
        .await
        .context("creating the shell proxy")
}

pub fn run(
    engine: Engine,
    commands: async_channel::Receiver<Command>,
    feedback: async_channel::Receiver<Feedback>,
) -> Result<()> {
    // The IO driver is for the D-Bus connection, which is built on this
    // runtime so that the loop below drives it.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("starting the async runtime")?;

    let connection =
        runtime.block_on(zbus::Connection::session()).context("connecting to the session bus")?;

    let (selection_tx, selection_rx) = async_channel::bounded(16);
    let (action_tx, action_rx) = async_channel::bounded(16);

    // Protocol version of the installed extension. Version 2 adds the focused
    // app id to selection reports (per-app rules), `Delete` and `ShowResult`.
    let version = runtime.block_on(extension_version(&connection));
    log::info!("shell extension protocol version {version}");

    // The app the extension last reported a selection in. The primary
    // selection belongs to it, so `grabit show` — which reads that selection —
    // is judged against the same exclusion.
    let selection_app = std::sync::Arc::new(std::sync::Mutex::new(String::new()));

    if version >= 2 {
        let exclusions = engine.clone();
        let reported_app = std::sync::Arc::clone(&selection_app);
        spawn_signal_relay(
            &runtime,
            &connection,
            "SelectionChangedV2",
            selection_tx,
            move |message| {
                let (text, app): (String, String) = match message.body().deserialize() {
                    Ok(pair) => pair,
                    Err(e) => {
                        log::warn!("malformed SelectionChangedV2 signal: {e}");
                        return None;
                    }
                };
                // An excluded app's selection must also take an existing bar down,
                // so it is reported as cleared rather than swallowed.
                let excluded =
                    !app.is_empty() && exclusions.config().applications.is_excluded(&app);
                *reported_app.lock().expect("selection app lock poisoned") = app;
                let event = if text.is_empty() || excluded {
                    Raw::Cleared
                } else {
                    Raw::Selection { generation: 1, grab: Grab::text(text) }
                };
                Some(event)
            },
        );
    } else {
        spawn_signal_relay(&runtime, &connection, "SelectionChanged", selection_tx, |message| {
            let text: String = match message.body().deserialize() {
                Ok(text) => text,
                Err(e) => {
                    log::warn!("malformed SelectionChanged signal: {e}");
                    return None;
                }
            };
            let event = if text.is_empty() {
                Raw::Cleared
            } else {
                // The shell is the only source of these events and delivers them
                // in order, so a constant generation is accurate.
                Raw::Selection { generation: 1, grab: Grab::text(text) }
            };
            Some(event)
        });
    }

    spawn_signal_relay(&runtime, &connection, "ActionInvoked", action_tx, |message| match message
        .body()
        .deserialize::<String>()
    {
        Ok(id) => Some(id),
        Err(e) => {
            log::warn!("malformed ActionInvoked signal: {e}");
            None
        }
    });

    runtime.block_on(async move {
        let limits = engine.clone();
        let mut selections =
            Box::pin(selection::settled(selection_rx, move || limits.selection_limits()));

        // The selection the visible bar acts on. Single-threaded runtime, so a
        // plain local is enough.
        let mut showing = Grab::default();
        // The ticket of the last invocation from the visible bar; a result
        // with any other ticket belongs to a bar that has since been replaced.
        let mut awaiting: Option<u64> = None;

        loop {
            tokio::select! {
                Some(event) = selections.next() => match event {
                    Settled::Text(grab) => {
                        let buttons = engine.buttons(&grab.text);
                        if buttons.is_empty() {
                            // The bar showing is for an older selection; leaving
                            // it up would act on text that is no longer selected.
                            log::debug!("no action matches this selection");
                            showing = Grab::default();
                            awaiting = None;
                            if let Err(e) = hide_popup(&connection).await {
                                log::warn!("{e:#}");
                            }
                            continue;
                        }
                        showing = grab;
                        awaiting = None;
                        if let Err(e) = show_popup(&connection, &buttons).await {
                            log::error!("{e:#}");
                        }
                    }
                    Settled::Cleared => {
                        showing = Grab::default();
                        awaiting = None;
                        if let Err(e) = hide_popup(&connection).await {
                            log::warn!("{e:#}");
                        }
                    }
                },

                Ok(id) = action_rx.recv() => {
                    awaiting = Some(engine.invoke(&id, std::mem::take(&mut showing)));
                }

                Ok(feedback) = feedback.recv() => match feedback {
                    Feedback::Result { ticket, title, body } => {
                        if awaiting != Some(ticket) {
                            log::info!("result from “{title}” arrived after its bar closed");
                        } else if version >= 2 {
                            if let Err(e) = show_result(&connection, &title, &body).await {
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
                        Ok(_) if is_excluded(&engine, &selection_app) => {
                            log::info!("the selection is in an excluded app");
                        }
                        Ok(text)
                            if selection::is_interesting(&text, &engine.config().selection) =>
                        {
                            let buttons = engine.buttons(&text);
                            showing = Grab::text(text);
                            awaiting = None;
                            if let Err(e) = show_popup(&connection, &buttons).await {
                                log::error!("{e:#}");
                            }
                        }
                        Ok(_) => log::info!("nothing selected"),
                        Err(e) => log::error!("reading the primary selection: {e:#}"),
                    },
                    Command::Hide => {
                        if let Err(e) = hide_popup(&connection).await {
                            log::warn!("{e:#}");
                        }
                    }
                    Command::Reload => {
                        if let Err(e) = engine.reload() {
                            log::error!("{e:#}");
                        }
                    }
                    Command::Quit => {
                        let _ = hide_popup(&connection).await;
                        return;
                    }
                },

                else => return,
            }
        }
    });

    Ok(())
}

/// Whether the app the last selection was made in is on the exclude list.
fn is_excluded(engine: &Engine, app: &std::sync::Mutex<String>) -> bool {
    let app = app.lock().expect("selection app lock poisoned");
    !app.is_empty() && engine.config().applications.is_excluded(&app)
}

async fn show_popup(
    connection: &zbus::Connection,
    buttons: &[crate::engine::Button],
) -> Result<()> {
    let payload: Vec<(String, String, String, String)> = buttons
        .iter()
        .map(|b| (b.id.clone(), b.title.clone(), b.icon.clone(), b.label.clone()))
        .collect();
    proxy(connection)
        .await?
        .call::<_, _, ()>("ShowPopup", &(payload,))
        .await
        .context("asking the GNOME Shell extension to show the bar")?;
    Ok(())
}

async fn hide_popup(connection: &zbus::Connection) -> Result<()> {
    proxy(connection)
        .await?
        .call::<_, _, ()>("HidePopup", &())
        .await
        .context("asking the GNOME Shell extension to hide the bar")?;
    Ok(())
}

async fn show_result(connection: &zbus::Connection, title: &str, body: &str) -> Result<()> {
    proxy(connection)
        .await?
        .call::<_, _, ()>("ShowResult", &(title, body))
        .await
        .context("asking the GNOME Shell extension to show a result")?;
    Ok(())
}

/// The extension's protocol version, defaulting to 1 for extensions that
/// predate the property. The daemon and the extension update independently, so
/// every capability past the original set is gated on this.
async fn extension_version(connection: &zbus::Connection) -> u32 {
    let version = async {
        proxy(connection)
            .await?
            .get_property::<u32>("Version")
            .await
            .context("reading the Version property")
    }
    .await;
    match version {
        Ok(version) => version,
        Err(e) => {
            log::debug!("no Version property ({e:#}); assuming protocol 1");
            1
        }
    }
}

/// Forward one D-Bus signal onto a channel from a task on `runtime`.
///
/// `parse` turns a signal into the item to forward, or `None` to drop it. The
/// task shares the runtime's one thread with the loop that drains `tx`, so it
/// waits for room in the channel rather than blocking on it.
fn spawn_signal_relay<T: Send + 'static>(
    runtime: &tokio::runtime::Runtime,
    connection: &zbus::Connection,
    signal: &'static str,
    tx: async_channel::Sender<T>,
    parse: impl Fn(zbus::Message) -> Option<T> + Send + 'static,
) {
    let connection = connection.clone();

    runtime.spawn(async move {
        let proxy = match proxy(&connection).await {
            Ok(proxy) => proxy,
            Err(e) => {
                log::error!("cannot reach the shell extension: {e:#}");
                return;
            }
        };
        let mut stream = match proxy.receive_signal(signal).await {
            Ok(stream) => stream,
            Err(e) => {
                log::error!("cannot subscribe to {signal}: {e}");
                return;
            }
        };
        while let Some(message) = stream.next().await {
            if let Some(item) = parse(message) {
                let _ = tx.send(item).await;
            }
        }
        log::warn!("the {signal} subscription ended; is the extension still enabled?");
    });
}
