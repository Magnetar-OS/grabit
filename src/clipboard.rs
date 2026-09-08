//! Writing to the Wayland clipboard.
//!
//! Wayland has no clipboard daemon: whoever owns a selection must stay alive to
//! serve it. `prepare_copy` + `serve` does exactly that on a thread we own,
//! which is why the forking `copy` convenience is avoided — forking a process
//! that already has Wayland and GTK threads running is not safe.

use anyhow::{Context, Result};
use wl_clipboard_rs::copy::{MimeType, Options, ServeRequests, Source};

/// Put `text` on the regular clipboard and keep serving it until another client
/// takes ownership.
pub fn set(text: &str) -> Result<()> {
    let mut options = Options::new();
    options.foreground(true).serve_requests(ServeRequests::Unlimited);

    let prepared = options
        .prepare_copy(Source::Bytes(text.as_bytes().into()), MimeType::Text)
        .context("claiming the clipboard")?;

    std::thread::Builder::new()
        .name("grabit-clipboard".into())
        .spawn(move || {
            // Returns once another client claims the selection, which is the
            // normal end of life for a clipboard owner.
            if let Err(e) = prepared.serve() {
                log::debug!("clipboard serving ended: {e}");
            }
        })
        .context("spawning the clipboard serving thread")?;

    Ok(())
}
