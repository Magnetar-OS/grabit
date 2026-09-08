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
use std::io::Read;
use std::os::fd::AsFd;

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
        offers: HashMap::new(),
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
    /// Mime types advertised by each live offer.
    offers: HashMap<ObjectId, HashSet<String>>,
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

    /// Pick the best text mime type an offer advertises.
    fn pick_mime(&self, offer: &ObjectId) -> Option<&'static str> {
        let mimes = self.offers.get(offer)?;
        TEXT_MIMES.iter().copied().find(|m| mimes.contains(*m))
    }

    /// Whether an offer also carries an HTML rendering of the selection.
    fn offers_html(&self, offer: &ObjectId) -> bool {
        self.offers.get(offer).is_some_and(|mimes| mimes.contains(HTML_MIME))
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
            Event::DataOffer { id } => {
                state.offers.insert(id.id(), HashSet::new());
            }
            Event::PrimarySelection { id } => {
                if !std::mem::replace(&mut state.seen_initial, true) {
                    log::debug!("ignoring the primary selection that predates startup");
                    if let Some(offer) = id {
                        state.offers.remove(&offer.id());
                        offer.destroy();
                    }
                    return;
                }
                match id.as_ref().and_then(|offer| state.pick_mime(&offer.id())) {
                    Some(mime) => {
                        let offer = id.expect("mime came from this offer");
                        let want_html = state.offers_html(&offer.id());
                        state.receive(conn, mime, want_html, |mime, fd| {
                            offer.receive(mime.to_owned(), fd);
                        });
                    }
                    None => state.clear(),
                }
            }
            Event::Selection { id: Some(offer) } => {
                // Not used, but the offer must be destroyed or it leaks.
                state.offers.remove(&offer.id());
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
            Event::DataOffer { id } => {
                state.offers.insert(id.id(), HashSet::new());
            }
            Event::PrimarySelection { id } => {
                if !std::mem::replace(&mut state.seen_initial, true) {
                    log::debug!("ignoring the primary selection that predates startup");
                    if let Some(offer) = id {
                        state.offers.remove(&offer.id());
                        offer.destroy();
                    }
                    return;
                }
                match id.as_ref().and_then(|offer| state.pick_mime(&offer.id())) {
                    Some(mime) => {
                        let offer = id.expect("mime came from this offer");
                        let want_html = state.offers_html(&offer.id());
                        state.receive(conn, mime, want_html, |mime, fd| {
                            offer.receive(mime.to_owned(), fd);
                        });
                    }
                    None => state.clear(),
                }
            }
            Event::Selection { id: Some(offer) } => {
                state.offers.remove(&offer.id());
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
            state.offers.entry(offer.id()).or_default().insert(mime_type);
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
            state.offers.entry(offer.id()).or_default().insert(mime_type);
        }
    }
}
