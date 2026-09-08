//! The D-Bus control surface.
//!
//! Everything a user can trigger from outside — a compositor keybinding, a
//! script, the `grabit` CLI itself — comes through here, so there is exactly one
//! way into a running daemon regardless of which front-end it is using.

use std::io::Read;

use anyhow::{Context, Result};

pub const BUS_NAME: &str = "org.grabit.Daemon";
pub const OBJECT_PATH: &str = "/org/grabit/Daemon";
pub const INTERFACE: &str = "org.grabit.Daemon1";

/// A request from outside the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Pop the bar up for whatever is currently selected.
    Show,
    Hide,
    Reload,
    Quit,
}

struct Control {
    tx: async_channel::Sender<Command>,
}

impl Control {
    fn dispatch(&self, command: Command) {
        // Never block the bus thread: a full queue means the main loop is wedged,
        // and stalling here would take D-Bus down with it.
        if self.tx.try_send(command).is_err() {
            log::warn!("dropping {command:?}: the daemon is not keeping up");
        }
    }
}

#[zbus::interface(name = "org.grabit.Daemon1")]
impl Control {
    /// Show the action bar for the current primary selection.
    fn show(&self) {
        self.dispatch(Command::Show);
    }

    /// Hide the action bar if it is up.
    fn hide(&self) {
        self.dispatch(Command::Hide);
    }

    /// Re-read `config.toml` and every action manifest.
    fn reload(&self) {
        self.dispatch(Command::Reload);
    }

    /// Shut the daemon down.
    fn quit(&self) {
        self.dispatch(Command::Quit);
    }
}

/// Claim the bus name and start serving. Returns the command stream.
pub fn serve() -> Result<async_channel::Receiver<Command>> {
    let (tx, rx) = async_channel::bounded(16);

    let connection = zbus::blocking::connection::Builder::session()
        .context("connecting to the session bus")?
        .serve_at(OBJECT_PATH, Control { tx })
        .context("exporting the control interface")?
        .build()
        .context("starting the D-Bus service")?;

    // Requested explicitly rather than through the builder so that a name which
    // is already taken is an immediate error. The builder's default is to wait
    // in the queue, which would silently leave a second daemon running with no
    // way to reach it.
    let reply = connection
        .request_name_with_flags(BUS_NAME, zbus::fdo::RequestNameFlags::DoNotQueue.into())
        .with_context(|| format!("claiming {BUS_NAME}"))?;
    use zbus::fdo::RequestNameReply;
    anyhow::ensure!(
        matches!(reply, RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner),
        "another grabit already owns {BUS_NAME}; stop it with `grabit quit` first"
    );

    // The connection has to outlive this function or the name is released
    // immediately; a parked thread owns it for the lifetime of the process.
    std::thread::Builder::new()
        .name("grabit-dbus".into())
        .spawn(move || {
            let _connection = connection;
            loop {
                std::thread::park();
            }
        })
        .context("spawning the D-Bus thread")?;

    Ok(rx)
}

/// Call a method on an already-running daemon.
pub fn call(method: &str) -> Result<()> {
    let connection =
        zbus::blocking::Connection::session().context("connecting to the session bus")?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE)
        .context("creating the control proxy")?;
    proxy
        .call::<_, _, ()>(method, &())
        .with_context(|| format!("calling {method} (is grabit running?)"))?;
    Ok(())
}

/// Read the current primary selection as text.
///
/// Used by the manual trigger. An empty primary selection is not an error — it
/// just means there is nothing to act on.
pub fn current_primary_selection() -> Result<String> {
    use wl_clipboard_rs::paste::{ClipboardType, Error, MimeType, Seat, get_contents};

    match get_contents(ClipboardType::Primary, Seat::Unspecified, MimeType::Text) {
        Ok((mut reader, _mime)) => {
            let mut text = String::new();
            reader.read_to_string(&mut text).context("reading the primary selection")?;
            Ok(text)
        }
        Err(Error::ClipboardEmpty | Error::NoMimeType) => Ok(String::new()),
        Err(e) => Err(e).context("requesting the primary selection"),
    }
}
