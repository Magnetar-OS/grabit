// SPDX-License-Identifier: GPL-3.0-only
//! One-shot probe of what the running compositor can actually do.
//!
//! grabit needs three separate capabilities and no compositor is guaranteed to
//! have all of them, so they are detected independently rather than inferred
//! from a desktop-environment name.

use anyhow::{Context, Result};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// The primary selection can be observed without holding focus.
    pub data_control: bool,
    /// A surface can be placed on the overlay layer, covering the whole output.
    pub layer_shell: bool,
    /// Keystrokes can be synthesised, which `after = "replace"` needs.
    pub virtual_keyboard: bool,
    /// The focused application can be identified, which per-app rules need.
    pub foreign_toplevel: bool,
}

impl Capabilities {
    /// Whether the native Wayland front-end can run here.
    pub fn supports_layer_frontend(&self) -> bool {
        self.data_control && self.layer_shell
    }
}

pub fn probe() -> Result<Capabilities> {
    let connection = Connection::connect_to_env().context(
        "connecting to the Wayland display (grabit is Wayland-only; \
         WAYLAND_DISPLAY is unset or the socket is unreachable)",
    )?;
    let mut queue = connection.new_event_queue();
    connection.display().get_registry(&queue.handle(), ());

    let mut globals = Globals::default();
    queue.roundtrip(&mut globals).context("listing Wayland globals")?;

    Ok(Capabilities {
        // wlr-data-control gained the primary-selection event in version 2,
        // so version 1 is no more useful here than no protocol at all.
        data_control: globals.has("ext_data_control_manager_v1", 1)
            || globals.has("zwlr_data_control_manager_v1", 2),
        layer_shell: globals.has("zwlr_layer_shell_v1", 1),
        virtual_keyboard: globals.has("zwp_virtual_keyboard_manager_v1", 1),
        foreign_toplevel: globals.has("zwlr_foreign_toplevel_manager_v1", 1),
    })
}

#[derive(Default)]
struct Globals {
    interfaces: Vec<(String, u32)>,
}

impl Globals {
    fn has(&self, interface: &str, min_version: u32) -> bool {
        self.interfaces.iter().any(|(name, v)| name == interface && *v >= min_version)
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Globals {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { interface, version, .. } = event {
            state.interfaces.push((interface, version));
        }
    }
}
