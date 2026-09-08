//! Synthesising the paste keystroke.
//!
//! "Replace the selection" is implemented as *put the new text on the clipboard,
//! then send Ctrl+V to whatever still has keyboard focus*. Typing the text out
//! character by character would be slower, would mangle text the target app
//! auto-formats, and would need a keymap entry per codepoint. Pasting needs
//! exactly one key.
//!
//! The popup never takes keyboard focus (its layer surface asks for none), so
//! the application the user selected in is still focused when this fires.

use std::io::Write;

use anyhow::{Context, Result};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

/// Something that can synthesise keystrokes into the focused application.
pub trait Injector: Send {
    /// Send Ctrl+V, pasting the clipboard.
    fn paste(&mut self) -> Result<()>;
    /// Send Delete, removing the selection. What `builtin = "cut"` uses after
    /// the copy half has happened.
    fn delete(&mut self) -> Result<()>;
}

/// XKB keycode of our Control key. The wire protocol wants evdev keycodes,
/// which are XKB keycodes minus 8.
const XKB_CONTROL: u32 = 9;
/// XKB keycode of our `v` key.
const XKB_V: u32 = 10;
/// XKB keycode of our Delete key.
const XKB_DELETE: u32 = 11;
/// The `Control` modifier bit in an XKB modifier mask.
const MOD_CONTROL: u32 = 1 << 2;

/// A keymap containing exactly the three keys we need.
///
/// `include "complete"` pulls the stock types and compat rules from the
/// compositor's XKB data, which is what makes `modifier_map` behave.
const KEYMAP: &str = r#"xkb_keymap {
xkb_keycodes "grabit" {
    minimum = 8;
    maximum = 255;
    <CTRL> = 9;
    <AV>   = 10;
    <ADEL> = 11;
};
xkb_types "grabit" { include "complete" };
xkb_compat "grabit" { include "complete" };
xkb_symbols "grabit" {
    key <CTRL> { [ Control_L ] };
    key <AV>   { [ v ] };
    key <ADEL> { [ Delete ] };
    modifier_map Control { <CTRL> };
};
};
"#;

/// Paste via `zwp_virtual_keyboard_v1`.
///
/// Supported by cosmic-comp, KWin and wlroots compositors. Mutter does not
/// expose it; GNOME sessions paste through the shell extension instead.
pub struct VirtualKeyboard {
    conn: Connection,
    keyboard: ZwpVirtualKeyboardV1,
    /// Kept alive so the objects created from its handle stay valid.
    _queue: wayland_client::EventQueue<Bind>,
    /// Event timestamps must increase monotonically or compositors drop keys.
    time: u32,
}

impl VirtualKeyboard {
    pub fn new() -> Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to the Wayland display")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());

        let mut state = Bind { seat: None, manager: None };
        queue.roundtrip(&mut state).context("Wayland roundtrip")?;

        let manager = state.manager.context(
            "this compositor does not expose zwp_virtual_keyboard_manager_v1, \
             so `after = \"replace\"` actions cannot paste",
        )?;
        let seat = state.seat.context("compositor advertised no wl_seat")?;
        let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());

        let mut this = Self { conn, keyboard, _queue: queue, time: 1 };
        this.upload_keymap().context("uploading the virtual keyboard keymap")?;
        Ok(this)
    }

    fn upload_keymap(&mut self) -> Result<()> {
        let memfd = memfd::MemfdOptions::default()
            .close_on_exec(true)
            .create("grabit-keymap")
            .context("creating the keymap memfd")?;
        let mut file = memfd.into_file();
        // The compositor reads up to `size` bytes and expects the buffer to be
        // NUL-terminated.
        file.write_all(KEYMAP.as_bytes()).context("writing the keymap")?;
        file.write_all(b"\0").context("terminating the keymap")?;
        file.flush()?;

        let size = (KEYMAP.len() + 1) as u32;
        self.keyboard.keymap(
            wayland_client::protocol::wl_keyboard::KeymapFormat::XkbV1 as u32,
            std::os::fd::AsFd::as_fd(&file),
            size,
        );
        self.conn.flush().context("flushing the keymap")?;
        Ok(())
    }

    fn tick(&mut self) -> u32 {
        self.time = self.time.wrapping_add(10).max(1);
        self.time
    }
}

impl Injector for VirtualKeyboard {
    fn paste(&mut self) -> Result<()> {
        // Press Control both as a key and as an explicit modifier mask:
        // compositors differ on which one they derive their state from.
        let t = self.tick();
        self.keyboard.key(t, XKB_CONTROL - 8, KEY_PRESSED);
        self.keyboard.modifiers(MOD_CONTROL, 0, 0, 0);

        let t = self.tick();
        self.keyboard.key(t, XKB_V - 8, KEY_PRESSED);
        let t = self.tick();
        self.keyboard.key(t, XKB_V - 8, KEY_RELEASED);

        self.keyboard.modifiers(0, 0, 0, 0);
        let t = self.tick();
        self.keyboard.key(t, XKB_CONTROL - 8, KEY_RELEASED);

        self.conn.flush().context("flushing the paste keystroke")?;
        Ok(())
    }

    fn delete(&mut self) -> Result<()> {
        let t = self.tick();
        self.keyboard.key(t, XKB_DELETE - 8, KEY_PRESSED);
        let t = self.tick();
        self.keyboard.key(t, XKB_DELETE - 8, KEY_RELEASED);

        self.conn.flush().context("flushing the delete keystroke")?;
        Ok(())
    }
}

const KEY_PRESSED: u32 = 1;
const KEY_RELEASED: u32 = 0;

/// Registry state used only while binding.
struct Bind {
    seat: Option<wl_seat::WlSeat>,
    manager: Option<ZwpVirtualKeyboardManagerV1>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Bind {
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
            "zwp_virtual_keyboard_manager_v1" if state.manager.is_none() => {
                state.manager = Some(registry.bind(name, 1, qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Bind {
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

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for Bind {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: <ZwpVirtualKeyboardManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for Bind {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: <ZwpVirtualKeyboardV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
