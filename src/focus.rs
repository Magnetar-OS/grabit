// SPDX-License-Identifier: GPL-3.0-only
//! Tracking which application is focused, for the per-app rules.
//!
//! Wayland tells an ordinary client nothing about other clients' windows. Two
//! protocol families come close enough to say "who is focused":
//!
//! - COSMIC: `ext_foreign_toplevel_list_v1` lists the toplevels and names each
//!   one's `app_id`; `zcosmic_toplevel_info_v1` (version 2 and later) extends
//!   each listed handle with a `state` event that marks the activated one.
//!   cosmic-comp exposes these and not the wlroots protocol below.
//! - wlroots, KWin: `zwlr_foreign_toplevel_manager_v1`, whose handles carry
//!   both `app_id` and `state` themselves.
//!
//! GNOME has neither; there the shell extension reports the focused app
//! alongside the selection instead.
//!
//! Where no source exists the feature is absent, not broken: `spawn` fails,
//! the caller keeps no handle, `grabit doctor` says so and the settings window
//! says the rules have no effect in this session.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use cosmic::cctk::cosmic_protocols::toplevel_info::v1::client::{
    zcosmic_toplevel_handle_v1 as cosmic_handle, zcosmic_toplevel_info_v1 as cosmic_info,
};
use cosmic::cctk::wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1 as ext_handle, ext_foreign_toplevel_list_v1 as ext_list,
};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1 as wlr_handle, zwlr_foreign_toplevel_manager_v1 as wlr_manager,
};

/// Where the focused application is learned from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `ext_foreign_toplevel_list_v1` plus `zcosmic_toplevel_info_v1`.
    Cosmic,
    /// `zwlr_foreign_toplevel_manager_v1`.
    Wlr,
}

impl Source {
    /// The first `get_cosmic_toplevel`-capable version of the COSMIC extension;
    /// version 1 predates the ext list and cannot be paired with it.
    const COSMIC_INFO_MIN: u32 = 2;

    /// The best source among the advertised globals, as `(interface, version)`.
    pub fn pick<'a>(globals: impl IntoIterator<Item = (&'a str, u32)> + Clone) -> Option<Self> {
        let has = |interface: &str, min: u32| {
            globals.clone().into_iter().any(|(name, v)| name == interface && v >= min)
        };
        if has("ext_foreign_toplevel_list_v1", 1)
            && has("zcosmic_toplevel_info_v1", Self::COSMIC_INFO_MIN)
        {
            Some(Self::Cosmic)
        } else if has("zwlr_foreign_toplevel_manager_v1", 1) {
            Some(Self::Wlr)
        } else {
            None
        }
    }

    /// The protocol names, for `grabit doctor`.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Cosmic => "zcosmic_toplevel_info_v1",
            Self::Wlr => "zwlr_foreign_toplevel_manager_v1",
        }
    }
}

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
    let registry = conn.display().get_registry(&qh, ());

    let current = Arc::new(RwLock::new(None));
    let mut state = State {
        current: Arc::clone(&current),
        globals: Vec::new(),
        cosmic_info: None,
        toplevels: Toplevels::default(),
        extensions: HashMap::new(),
    };
    queue.roundtrip(&mut state).context("listing Wayland globals")?;

    let source = Source::pick(state.globals.iter().map(|(_, i, v)| (i.as_str(), *v))).context(
        "this compositor exposes neither zcosmic_toplevel_info_v1 (with \
         ext_foreign_toplevel_list_v1) nor zwlr_foreign_toplevel_manager_v1",
    )?;
    // Bound after the registry pass, so the COSMIC extension object exists
    // before the list announces its first toplevel.
    let global = |interface: &str| {
        state.globals.iter().find(|(_, i, _)| i == interface).map(|(name, _, v)| (*name, *v))
    };
    match source {
        Source::Cosmic => {
            let (name, version) = global("zcosmic_toplevel_info_v1").context("the info global")?;
            let info: cosmic_info::ZcosmicToplevelInfoV1 =
                registry.bind(name, version.min(3), &qh, ());
            let (name, _) = global("ext_foreign_toplevel_list_v1").context("the list global")?;
            let _list: ext_list::ExtForeignToplevelListV1 = registry.bind(name, 1, &qh, ());
            state.cosmic_info = Some(info);
        }
        Source::Wlr => {
            let (name, version) =
                global("zwlr_foreign_toplevel_manager_v1").context("the manager global")?;
            let _manager: wlr_manager::ZwlrForeignToplevelManagerV1 =
                registry.bind(name, version.min(3), &qh, ());
        }
    }
    log::info!("per-app rules follow the focused app through {}", source.describe());
    // One roundtrip for the toplevels and their app ids, a second for the
    // COSMIC extension objects requested while handling them, so the focused
    // app is known before the caller first asks.
    for _ in 0..2 {
        queue.roundtrip(&mut state).context("reading the initial toplevels")?;
    }

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

/// What one toplevel has told us so far. `app_id` and the activated state
/// arrive as separate events in unspecified order — on COSMIC even on
/// separate objects — so both halves are remembered.
#[derive(Default)]
struct Toplevel {
    app_id: String,
    activated: bool,
}

/// Every known toplevel, keyed by whatever identifies it on the wire.
///
/// The focused app is derived from the whole set after every change rather
/// than tracked incrementally, so a toplevel that loses activation stops being
/// the focused one even before another gains it.
struct Toplevels<K> {
    by_id: HashMap<K, Toplevel>,
}

impl<K> Default for Toplevels<K> {
    fn default() -> Self {
        Self { by_id: HashMap::new() }
    }
}

impl<K: Hash + Eq> Toplevels<K> {
    fn add(&mut self, id: K) {
        self.by_id.entry(id).or_default();
    }

    fn set_app_id(&mut self, id: &K, app_id: String) {
        if let Some(entry) = self.by_id.get_mut(id) {
            entry.app_id = app_id;
        }
    }

    fn set_activated(&mut self, id: &K, activated: bool) {
        if let Some(entry) = self.by_id.get_mut(id) {
            entry.activated = activated;
        }
    }

    fn remove(&mut self, id: &K) {
        self.by_id.remove(id);
    }

    /// The app id of the activated toplevel, once it has named itself.
    fn focused(&self) -> Option<String> {
        self.by_id.values().find(|t| t.activated && !t.app_id.is_empty()).map(|t| t.app_id.clone())
    }
}

/// Whether a `state` array (u32 flags on the wire) contains `activated`.
fn has_flag(raw: &[u8], flag: u32) -> bool {
    raw.as_chunks::<4>().0.iter().map(|b| u32::from_ne_bytes(*b)).any(|f| f == flag)
}

struct State {
    current: Arc<RwLock<Option<String>>>,
    /// `(name, interface, version)` from the registry pass.
    globals: Vec<(u32, String, u32)>,
    cosmic_info: Option<cosmic_info::ZcosmicToplevelInfoV1>,
    toplevels: Toplevels<ObjectId>,
    /// The COSMIC extension object of each listed toplevel, destroyed with it.
    extensions: HashMap<ObjectId, cosmic_handle::ZcosmicToplevelHandleV1>,
}

impl State {
    fn publish(&self) {
        *self.current.write().expect("focus lock poisoned") = self.toplevels.focused();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            state.globals.push((name, interface, version));
        }
    }
}

// --- COSMIC: ext_foreign_toplevel_list_v1 + zcosmic_toplevel_info_v1 -------

impl Dispatch<ext_list::ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ext_list::ExtForeignToplevelListV1,
        event: ext_list::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_list::Event::Toplevel { toplevel } => {
                let id = toplevel.id();
                state.toplevels.add(id.clone());
                if let Some(info) = &state.cosmic_info {
                    let extension = info.get_cosmic_toplevel(&toplevel, qh, id.clone());
                    state.extensions.insert(id, extension);
                }
            }
            ext_list::Event::Finished => {
                log::warn!("the compositor stopped listing toplevels");
            }
            _ => {}
        }
    }

    event_created_child!(State, ext_list::ExtForeignToplevelListV1, [
        ext_list::EVT_TOPLEVEL_OPCODE => (ext_handle::ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ext_handle::ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        toplevel: &ext_handle::ExtForeignToplevelHandleV1,
        event: ext_handle::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = toplevel.id();
        match event {
            ext_handle::Event::AppId { app_id } => state.toplevels.set_app_id(&id, app_id),
            ext_handle::Event::Closed => {
                state.toplevels.remove(&id);
                if let Some(extension) = state.extensions.remove(&id) {
                    extension.destroy();
                }
                toplevel.destroy();
            }
            _ => return,
        }
        state.publish();
    }
}

impl Dispatch<cosmic_info::ZcosmicToplevelInfoV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &cosmic_info::ZcosmicToplevelInfoV1,
        _: cosmic_info::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Only `done` reaches a version-2 client; state is applied per event.
    }
}

/// The extension object carries the id of the list handle it extends.
impl Dispatch<cosmic_handle::ZcosmicToplevelHandleV1, ObjectId> for State {
    fn event(
        state: &mut Self,
        _: &cosmic_handle::ZcosmicToplevelHandleV1,
        event: cosmic_handle::Event,
        toplevel: &ObjectId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let cosmic_handle::Event::State { state: raw } = event {
            let activated = has_flag(&raw, cosmic_handle::State::Activated as u32);
            state.toplevels.set_activated(toplevel, activated);
            state.publish();
        }
    }
}

// --- wlroots / KWin: zwlr_foreign_toplevel_manager_v1 ------------------------

impl Dispatch<wlr_manager::ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &wlr_manager::ZwlrForeignToplevelManagerV1,
        event: wlr_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wlr_manager::Event::Toplevel { toplevel } => state.toplevels.add(toplevel.id()),
            wlr_manager::Event::Finished => {
                log::warn!("the compositor revoked our foreign-toplevel manager");
            }
            _ => {}
        }
    }

    event_created_child!(State, wlr_manager::ZwlrForeignToplevelManagerV1, [
        wlr_manager::EVT_TOPLEVEL_OPCODE => (wlr_handle::ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<wlr_handle::ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        toplevel: &wlr_handle::ZwlrForeignToplevelHandleV1,
        event: wlr_handle::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = toplevel.id();
        match event {
            wlr_handle::Event::AppId { app_id } => state.toplevels.set_app_id(&id, app_id),
            wlr_handle::Event::State { state: raw } => {
                let activated = has_flag(&raw, wlr_handle::State::Activated as u32);
                state.toplevels.set_activated(&id, activated);
            }
            wlr_handle::Event::Closed => {
                state.toplevels.remove(&id);
                toplevel.destroy();
            }
            _ => return,
        }
        state.publish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_ne_bytes()).collect()
    }

    /// cosmic-comp advertises the COSMIC pair and not the wlroots manager;
    /// that pair has to be recognised, or per-app rules never apply there.
    #[test]
    fn cosmic_globals_pick_the_cosmic_source() {
        let cosmic = [
            ("zcosmic_toplevel_info_v1", 3),
            ("zcosmic_toplevel_manager_v1", 4),
            ("ext_foreign_toplevel_list_v1", 1),
        ];
        assert_eq!(Source::pick(cosmic), Some(Source::Cosmic));
    }

    #[test]
    fn source_picking_needs_a_usable_pair() {
        // Version 1 of the COSMIC extension cannot be paired with the ext list.
        let old = [("zcosmic_toplevel_info_v1", 1), ("ext_foreign_toplevel_list_v1", 1)];
        assert_eq!(Source::pick(old), None);
        // The ext list alone names apps but never says which is activated.
        assert_eq!(Source::pick([("ext_foreign_toplevel_list_v1", 1)]), None);
        assert_eq!(Source::pick([("zwlr_foreign_toplevel_manager_v1", 3)]), Some(Source::Wlr));
        let both = [
            ("zwlr_foreign_toplevel_manager_v1", 3),
            ("zcosmic_toplevel_info_v1", 2),
            ("ext_foreign_toplevel_list_v1", 1),
        ];
        assert_eq!(Source::pick(both), Some(Source::Cosmic));
    }

    #[test]
    fn the_activated_named_toplevel_is_focused_whatever_the_event_order() {
        let mut toplevels = Toplevels::default();
        toplevels.add(1);
        toplevels.set_activated(&1, true);
        assert_eq!(toplevels.focused(), None, "no app id yet");
        toplevels.set_app_id(&1, "org.keepassxc.KeePassXC".into());
        assert_eq!(toplevels.focused().as_deref(), Some("org.keepassxc.KeePassXC"));
    }

    /// Focus moving to something grabit cannot see (a layer surface, the
    /// desktop) deactivates the old toplevel without activating another; the
    /// old app must not stay "focused".
    #[test]
    fn deactivation_clears_the_focused_app() {
        let mut toplevels = Toplevels::default();
        toplevels.add(1);
        toplevels.set_app_id(&1, "firefox".into());
        toplevels.set_activated(&1, true);
        toplevels.set_activated(&1, false);
        assert_eq!(toplevels.focused(), None);
    }

    #[test]
    fn focus_follows_activation_and_closing() {
        let mut toplevels = Toplevels::default();
        for (id, app) in [(1, "firefox"), (2, "com.system76.CosmicTerm")] {
            toplevels.add(id);
            toplevels.set_app_id(&id, app.into());
        }
        toplevels.set_activated(&1, true);
        toplevels.set_activated(&1, false);
        toplevels.set_activated(&2, true);
        assert_eq!(toplevels.focused().as_deref(), Some("com.system76.CosmicTerm"));
        toplevels.remove(&2);
        assert_eq!(toplevels.focused(), None);
    }

    #[test]
    fn activated_is_read_from_the_state_array() {
        let activated = cosmic_handle::State::Activated as u32;
        assert!(has_flag(&flags(&[0, activated]), activated));
        assert!(!has_flag(&flags(&[0, 3]), activated));
        assert!(!has_flag(&[], activated));
    }
}
