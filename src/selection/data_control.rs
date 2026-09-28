// SPDX-License-Identifier: GPL-3.0-only
//! Primary-selection monitoring via `ext-data-control-v1` / `wlr-data-control-v1`.
//!
//! These are the only Wayland protocols that let an unfocused client observe
//! the selection, which is exactly what a selection-triggered popup needs.
//! `ext-data-control` is the standardised successor and is preferred; the
//! wlroots original is accepted at version 2 or above, which is where primary
//! selection support was added.
//!
//! Mutter implements neither, by policy — GNOME sessions are served by
//! [`crate::selection::shell`] instead.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd};

use anyhow::{Context, Result};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client as ext;
use wayland_protocols_wlr::data_control::v1::client as wlr;

use super::{Grab, Raw};

/// Mime types we will accept for a text selection, best first.
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];

/// The HTML rendering browsers and rich editors offer alongside the text.
const HTML_MIME: &str = "text/html";

/// Cap on how much of a selection is read. Selections above the configured
/// maximum are discarded anyway, and this stops a hostile or buggy source from
/// streaming unbounded data into the daemon.
const MAX_READ: u64 = 4 * 1024 * 1024;

/// Run the watcher loop. Blocks until the Wayland connection drops.
pub fn run(tx: async_channel::Sender<Raw>) -> Result<()> {
    let conn = Connection::connect_to_env().context("connecting to the Wayland display")?;
    let display = conn.display();
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    display.get_registry(&qh, ());

    let mut state = State {
        tx,
        seat: None,
        ext_manager: None,
        wlr_manager: None,
        offers: Offers::default(),
        generation: 0,
        bound: false,
        seen_initial: false,
    };

    // First roundtrip delivers the globals, the second the seat capabilities.
    queue.roundtrip(&mut state).context("initial Wayland roundtrip")?;
    state.bind_device(&qh)?;
    queue.roundtrip(&mut state).context("binding the data-control device")?;

    loop {
        queue.blocking_dispatch(&mut state).context("dispatching Wayland events")?;
    }
}

struct State {
    tx: async_channel::Sender<Raw>,
    seat: Option<wl_seat::WlSeat>,
    ext_manager: Option<ext::ext_data_control_manager_v1::ExtDataControlManagerV1>,
    wlr_manager: Option<wlr::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
    /// Mime types advertised by each offer not yet used.
    offers: Offers<ObjectId>,
    /// Incremented per selection so that a slow read from an old selection can
    /// be discarded instead of overwriting a newer one.
    generation: u64,
    bound: bool,
    /// The compositor reports the current primary selection as soon as the
    /// device is bound. Acting on it would make grabit pop up over whatever the
    /// user last highlighted, possibly hours ago, so the first report is
    /// swallowed and only genuine changes are forwarded.
    seen_initial: bool,
}

impl State {
    fn bind_device(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let seat = self.seat.clone().context("compositor advertised no wl_seat")?;
        if let Some(manager) = &self.ext_manager {
            manager.get_data_device(&seat, qh, ());
            log::info!("selection backend: ext-data-control-v1");
        } else if let Some(manager) = &self.wlr_manager {
            manager.get_data_device(&seat, qh, ());
            log::info!("selection backend: wlr-data-control-v1 v2");
        } else {
            anyhow::bail!(
                "this compositor exposes neither ext-data-control-v1 nor \
                 wlr-data-control-v1 v2, so the primary selection cannot be watched"
            );
        }
        self.bound = true;
        Ok(())
    }

    /// Act on a new primary selection, then destroy its offer.
    ///
    /// Every offer is used by exactly one selection event, and the protocol
    /// leaves destroying it to the client: an offer kept alive after its
    /// transfer is requested costs a server-side object per selection for as
    /// long as the daemon runs. The pipes handed over in `receive` keep the
    /// transfer going without it.
    fn primary_selection<O: Proxy>(
        &mut self,
        conn: &Connection,
        offer: Option<O>,
        receive: impl Fn(&O, String, BorrowedFd<'_>),
        destroy: impl FnOnce(&O),
    ) {
        let Some(offer) = offer else {
            if std::mem::replace(&mut self.seen_initial, true) {
                self.clear();
            }
            return;
        };
        let text = self.offers.take(&offer.id());
        if !std::mem::replace(&mut self.seen_initial, true) {
            log::debug!("ignoring the primary selection that predates startup");
        } else if let Some(text) = text {
            self.receive(conn, text.mime, text.html, |mime, fd| {
                receive(&offer, mime.to_owned(), fd);
            });
        } else {
            self.clear();
        }
        destroy(&offer);
    }

    /// Read an offer's contents on a worker thread and forward the selection.
    /// When the source also offers `text/html`, both representations are
    /// requested up front and read together.
    ///
    /// The reads must not happen inline: the source client only writes once it
    /// sees the request, which it cannot do while we are blocking our own event
    /// loop waiting for its bytes.
    fn receive(
        &mut self,
        conn: &Connection,
        mime: &'static str,
        want_html: bool,
        request: impl Fn(&str, std::os::fd::BorrowedFd<'_>),
    ) {
        let pipe = |label: &str| match std::io::pipe() {
            Ok(pair) => Some(pair),
            Err(e) => {
                log::warn!("cannot create {label} pipe for selection transfer: {e}");
                None
            }
        };
        let Some((mut text_reader, text_writer)) = pipe("text") else { return };
        request(mime, text_writer.as_fd());

        let html_reader = want_html.then(|| pipe("html")).flatten().map(|(reader, writer)| {
            request(HTML_MIME, writer.as_fd());
            reader
        });

        // The compositor now owns duplicates of the write ends. Ours must go,
        // or the reads below would never see EOF. `text_writer` drops here;
        // the HTML writer already dropped inside the closure above.
        drop(text_writer);
        if let Err(e) = conn.flush() {
            log::warn!("flushing the selection request failed: {e}");
            return;
        }

        self.generation += 1;
        let generation = self.generation;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Err(e) = text_reader.by_ref().take(MAX_READ).read_to_end(&mut buf) {
                log::debug!("selection transfer failed: {e}");
                return;
            }
            let Ok(text) = String::from_utf8(buf) else {
                log::debug!("selection was not valid UTF-8; ignoring");
                return;
            };

            // The pipes are independent, so draining them one after the other
            // cannot deadlock; a source that never writes the HTML half just
            // ends this read at EOF when it closes the fd.
            let html = html_reader.and_then(|mut reader| {
                let mut buf = Vec::new();
                match reader.by_ref().take(MAX_READ).read_to_end(&mut buf) {
                    Ok(_) => String::from_utf8(buf).ok().filter(|s| !s.is_empty()),
                    Err(e) => {
                        log::debug!("html transfer failed: {e}");
                        None
                    }
                }
            });

            let _ = tx.send_blocking(Raw::Selection { generation, grab: Grab { text, html } });
        });
    }

    fn clear(&mut self) {
        self.generation += 1;
        let _ = self.tx.send_blocking(Raw::Cleared);
    }
}

/// The mime types each announced offer advertises, until a selection event
/// uses the offer.
struct Offers<K> {
    by_id: HashMap<K, HashSet<String>>,
}

impl<K> Default for Offers<K> {
    fn default() -> Self {
        Self { by_id: HashMap::new() }
    }
}

/// How to read an offer's text.
#[derive(Debug, PartialEq, Eq)]
struct TextChoice {
    /// The best text mime type the offer advertises.
    mime: &'static str,
    /// Whether it also carries an HTML rendering of the selection.
    html: bool,
}

impl<K: Hash + Eq> Offers<K> {
    fn announce(&mut self, id: K) {
        self.by_id.insert(id, HashSet::new());
    }

    fn advertise(&mut self, id: K, mime: String) {
        self.by_id.entry(id).or_default().insert(mime);
    }

    /// Forget an offer, returning how to read its text, if it has any.
    fn take(&mut self, id: &K) -> Option<TextChoice> {
        let mimes = self.by_id.remove(id)?;
        let mime = TEXT_MIMES.iter().copied().find(|m| mimes.contains(*m))?;
        Some(TextChoice { mime, html: mimes.contains(HTML_MIME) })
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global { name, interface, version } = event else {
            return;
        };
        match interface.as_str() {
            "wl_seat" if state.seat.is_none() => {
                state.seat = Some(registry.bind(name, version.min(7), qh, ()));
            }
            "ext_data_control_manager_v1" if state.ext_manager.is_none() => {
                state.ext_manager = Some(registry.bind(name, version.min(1), qh, ()));
            }
            // Version 1 has no primary-selection event, so it is of no use here.
            "zwlr_data_control_manager_v1" if state.wlr_manager.is_none() && version >= 2 => {
                state.wlr_manager = Some(registry.bind(name, version.min(2), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ext::ext_data_control_manager_v1::ExtDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ext::ext_data_control_manager_v1::ExtDataControlManagerV1,
        _: <ext::ext_data_control_manager_v1::ExtDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wlr::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &wlr::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
        _: <wlr::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ext::ext_data_control_device_v1::ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ext::ext_data_control_device_v1::ExtDataControlDeviceV1,
        event: ext::ext_data_control_device_v1::Event,
        _: &(),
        conn: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext::ext_data_control_device_v1::Event;
        match event {
            Event::DataOffer { id } => state.offers.announce(id.id()),
            Event::PrimarySelection { id } => state.primary_selection(
                conn,
                id,
                |offer, mime, fd| offer.receive(mime, fd),
                |offer| offer.destroy(),
            ),
            Event::Selection { id: Some(offer) } => {
                // Not used, but the offer must be destroyed or it leaks.
                state.offers.take(&offer.id());
                offer.destroy();
            }
            Event::Finished => log::warn!("the compositor revoked our data-control device"),
            _ => {}
        }
    }

    event_created_child!(State, ext::ext_data_control_device_v1::ExtDataControlDeviceV1, [
        ext::ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE
            => (ext::ext_data_control_offer_v1::ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<wlr::zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &wlr::zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
        event: wlr::zwlr_data_control_device_v1::Event,
        _: &(),
        conn: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wlr::zwlr_data_control_device_v1::Event;
        match event {
            Event::DataOffer { id } => state.offers.announce(id.id()),
            Event::PrimarySelection { id } => state.primary_selection(
                conn,
                id,
                |offer, mime, fd| offer.receive(mime, fd),
                |offer| offer.destroy(),
            ),
            Event::Selection { id: Some(offer) } => {
                state.offers.take(&offer.id());
                offer.destroy();
            }
            Event::Finished => log::warn!("the compositor revoked our data-control device"),
            _ => {}
        }
    }

    event_created_child!(State, wlr::zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
        wlr::zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE
            => (wlr::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ext::ext_data_control_offer_v1::ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ext::ext_data_control_offer_v1::ExtDataControlOfferV1,
        event: ext::ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext::ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offers.advertise(offer.id(), mime_type);
        }
    }
}

impl Dispatch<wlr::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &wlr::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
        event: wlr::zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wlr::zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offers.advertise(offer.id(), mime_type);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(offers: &mut Offers<u32>, id: u32, mimes: &[&str]) {
        offers.announce(id);
        for mime in mimes {
            offers.advertise(id, (*mime).to_owned());
        }
    }

    /// One offer arrives per selection change — per pointer motion during a
    /// drag — for the whole session, so a used offer must not stay behind.
    #[test]
    fn a_used_offer_is_forgotten() {
        let mut offers = Offers::default();
        offer(&mut offers, 1, &["text/html", "UTF8_STRING", "text/plain;charset=utf-8"]);
        assert_eq!(
            offers.take(&1),
            Some(TextChoice { mime: "text/plain;charset=utf-8", html: true })
        );
        assert!(offers.by_id.is_empty());
    }

    #[test]
    fn an_offer_without_text_is_forgotten_too() {
        let mut offers = Offers::default();
        offer(&mut offers, 1, &["image/png"]);
        assert_eq!(offers.take(&1), None);
        assert!(offers.by_id.is_empty());
    }
}
