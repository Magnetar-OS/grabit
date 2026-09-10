// SPDX-License-Identifier: GPL-3.0-only
//! Tracking which application is focused, for the per-app rules.
//!
//! Wayland tells an ordinary client nothing about other clients' windows; the
//! closest thing to "who is focused" a daemon can get is
//! `zwlr_foreign_toplevel_management_v1`, whose `state` event marks the
//! activated toplevel and whose `app_id` event names it. cosmic-comp, KWin and
//! the wlroots compositors implement it; GNOME does not, and there the shell
//! extension reports the focused app alongside the selection instead.
//!
//! Where the protocol is missing the feature is absent, not broken: `spawn`
//! fails, the caller keeps no handle, and no exclusion is applied.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1 as handle_v1, zwlr_foreign_toplevel_manager_v1 as manager_v1,
};

/// A live view of the focused application's app id.
#[derive(Clone)]
pub struct Handle(Arc<RwLock<Option<String>>>);

impl Handle {
    /// The app id of the currently activated toplevel, if one is known.
    pub fn current(&self) -> Option<String> {
        self.0.read().expect("focus lock poisoned").clone()
    }
}

/// Start watching toplevel activation on a dedicated thread.
pub fn spawn() -> Result<Handle> {
    let conn = Connection::connect_to_env().context("connecting to the Wayland display")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let current = Arc::new(RwLock::new(None));
    let mut state =
        State { current: Arc::clone(&current), manager: None, toplevels: HashMap::new() };
    queue.roundtrip(&mut state).context("listing Wayland globals")?;
    anyhow::ensure!(
        state.manager.is_some(),
        "this compositor does not expose zwlr_foreign_toplevel_management_v1"
    );

    std::thread::Builder::new()
        .name("grabit-focus".into())
        .spawn(move || {
            loop {
                if let Err(e) = queue.blocking_dispatch(&mut state) {
                    log::warn!("focus tracking stopped: {e}");
                    return;
                }
            }
        })
        .context("spawning the focus tracker")?;

    Ok(Handle(current))
}

/// What one toplevel has told us so far. `app_id` and `state` arrive as
/// separate events in unspecified order, so both halves are remembered.
#[derive(Default)]
struct Toplevel {
    app_id: String,
    activated: bool,
}

struct State {
    current: Arc<RwLock<Option<String>>>,
    manager: Option<manager_v1::ZwlrForeignToplevelManagerV1>,
    toplevels: HashMap<ObjectId, Toplevel>,
}

impl State {
    fn set_current(&self, app_id: Option<String>) {
        *self.current.write().expect("focus lock poisoned") = app_id;
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
        if let wl_registry::Event::Global { name, interface, version } = event
            && interface == "zwlr_foreign_toplevel_manager_v1"
            && state.manager.is_none()
        {
            state.manager = Some(registry.bind(name, version.min(3), qh, ()));
        }
    }
}

impl Dispatch<manager_v1::ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &manager_v1::ZwlrForeignToplevelManagerV1,
        event: manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            manager_v1::Event::Toplevel { toplevel } => {
                state.toplevels.insert(toplevel.id(), Toplevel::default());
            }
            manager_v1::Event::Finished => {
                log::warn!("the compositor revoked our foreign-toplevel manager");
            }
            _ => {}
        }
    }

    event_created_child!(State, manager_v1::ZwlrForeignToplevelManagerV1, [
        manager_v1::EVT_TOPLEVEL_OPCODE => (handle_v1::ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<handle_v1::ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        toplevel: &handle_v1::ZwlrForeignToplevelHandleV1,
        event: handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = toplevel.id();
        match event {
            handle_v1::Event::AppId { app_id } => {
                if let Some(entry) = state.toplevels.get_mut(&id) {
                    entry.app_id = app_id.clone();
                    if entry.activated {
                        state.set_current(Some(app_id));
                    }
                }
            }
            handle_v1::Event::State { state: raw } => {
                // The state is an array of u32 flags on the wire.
                let activated = raw
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| u32::from_ne_bytes(*b))
                    .any(|flag| flag == handle_v1::State::Activated as u32);
                if let Some(entry) = state.toplevels.get_mut(&id) {
                    entry.activated = activated;
                    if activated && !entry.app_id.is_empty() {
                        let app_id = entry.app_id.clone();
                        state.set_current(Some(app_id));
                    }
                }
            }
            handle_v1::Event::Closed => {
                let was_current = state
                    .toplevels
                    .remove(&id)
                    .is_some_and(|t| t.activated && !t.app_id.is_empty());
                if was_current {
                    // Something else activates next; until then nothing is.
                    state.set_current(None);
                }
                toplevel.destroy();
            }
            _ => {}
        }
    }
}
