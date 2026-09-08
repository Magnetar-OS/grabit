/**
 * grabit — GNOME Shell companion.
 *
 * Mutter implements neither `ext-data-control` (watching the selection without
 * focus) nor `wlr-layer-shell` (placing a surface at a point on screen), and by
 * policy will not. GNOME Shell itself has both capabilities, so on GNOME this
 * extension owns everything that touches the compositor:
 *
 *   * watching the primary selection,
 *   * drawing the action bar at the pointer,
 *   * synthesising the paste keystroke.
 *
 * It owns no policy. Which actions exist, when they apply and what they do stays
 * in the daemon, which drives this over `org.grabit.Shell`.
 */

import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Meta from 'gi://Meta';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'org.grabit.Shell';
const OBJECT_PATH = '/org/grabit/Shell';

const INTERFACE = `
<node>
  <interface name="org.grabit.Shell1">
    <property name="Version" type="u" access="read"/>
    <method name="ShowPopup">
      <arg type="a(ssss)" direction="in" name="buttons"/>
    </method>
    <method name="HidePopup"/>
    <method name="Paste"/>
    <method name="Delete"/>
    <method name="ShowResult">
      <arg type="s" direction="in" name="title"/>
      <arg type="s" direction="in" name="body"/>
    </method>
    <signal name="SelectionChanged">
      <arg type="s" name="text"/>
    </signal>
    <signal name="SelectionChangedV2">
      <arg type="s" name="text"/>
      <arg type="s" name="app"/>
    </signal>
    <signal name="ActionInvoked">
      <arg type="s" name="id"/>
    </signal>
  </interface>
</node>`;

/**
 * Protocol version. The daemon reads this and only uses what the running
 * extension actually has, so the two update independently.
 *
 *   1 — ShowPopup / HidePopup / Paste / SelectionChanged / ActionInvoked
 *   2 — adds Version, Delete, ShowResult, SelectionChangedV2 (with the focused
 *       app id), Shift-suppression and bar paging
 */
const PROTOCOL_VERSION = 2;

/** Offset of the bar from the pointer, matching the daemon's layer-shell path. */
const OFFSET_X = 12;
const OFFSET_Y = 18;

/** Hide the bar this long after the pointer leaves it, once it has been used. */
const DISMISS_MS = 900;

/** Hide the bar unconditionally after this long. */
const TIMEOUT_MS = 8000;

/** Actions shown per page; more pages behind the chevrons. */
const PER_PAGE = 8;

export default class GrabitExtension extends Extension {
    enable() {
        this._bar = null;
        this._buttons = [];
        this._page = 0;
        this._dismissTimer = 0;
        this._timeoutTimer = 0;
        this._virtualDevice = null;

        this._dbus = Gio.DBusExportedObject.wrapJSObject(INTERFACE, this);
        this._dbus.export(Gio.DBus.session, OBJECT_PATH);
        this._nameId = Gio.bus_own_name(
            Gio.BusType.SESSION,
            BUS_NAME,
            Gio.BusNameOwnerFlags.NONE,
            null,
            null,
            null);

        this._selection = global.display.get_selection();
        this._ownerChangedId = this._selection.connect(
            'owner-changed',
            (_selection, type) => this._onOwnerChanged(type));
    }

    disable() {
        this._hidePopup();

        if (this._ownerChangedId) {
            this._selection.disconnect(this._ownerChangedId);
            this._ownerChangedId = 0;
        }
        this._selection = null;

        if (this._nameId) {
            Gio.bus_unown_name(this._nameId);
            this._nameId = 0;
        }
        if (this._dbus) {
            this._dbus.unexport();
            this._dbus = null;
        }

        // The virtual device holds a Clutter resource and must not survive a
        // lock-screen disable/enable cycle.
        this._virtualDevice = null;
    }

    // --- selection ---------------------------------------------------------

    _onOwnerChanged(type) {
        if (type !== Meta.SelectionType.SELECTION_PRIMARY)
            return;

        // Holding Shift while selecting keeps the bar away — PopClip's
        // suppression gesture. Only the shell can see global modifier state on
        // Wayland, which is why this lives here and not in the daemon.
        const [, , mods] = global.get_pointer();
        const suppressed = (mods & Clutter.ModifierType.SHIFT_MASK) !== 0;

        St.Clipboard.get_default().get_text(
            St.ClipboardType.PRIMARY,
            (_clipboard, text) => {
                if (!this._dbus)
                    return;
                const payload = suppressed ? '' : (text ?? '');
                // An empty payload is how the daemon is told the selection went
                // away, so it is reported rather than dropped. Both signal
                // generations are emitted; the daemon subscribes to the newest
                // one it understands.
                this._dbus.emit_signal(
                    'SelectionChanged',
                    new GLib.Variant('(s)', [payload]));
                this._dbus.emit_signal(
                    'SelectionChangedV2',
                    new GLib.Variant('(ss)', [payload, this._focusedApp()]));
            });
    }

    /** The focused application's id, best effort: the sandboxed/GTK app id
     *  where one exists, the WM class otherwise. */
    _focusedApp() {
        const win = global.display.focus_window;
        if (!win)
            return '';
        return win.get_gtk_application_id() ?? win.get_wm_class() ?? '';
    }

    // --- D-Bus methods -----------------------------------------------------

    get Version() {
        return PROTOCOL_VERSION;
    }

    ShowPopup(buttons) {
        this._hidePopup();
        if (!buttons.length)
            return;

        this._buttons = buttons;
        this._page = 0;
        this._showBar(this._buildBarContent());
    }

    /** Fill a fresh bar for the current page of `this._buttons`. */
    _buildBarContent() {
        const bar = new St.BoxLayout({
            style_class: 'grabit-bar',
            reactive: true,
            track_hover: true,
        });

        const pages = Math.max(1, Math.ceil(this._buttons.length / PER_PAGE));
        const start = this._page * PER_PAGE;
        const visible = this._buttons.slice(start, start + PER_PAGE);

        if (pages > 1)
            bar.add_child(this._buildPager('‹', this._page > 0, -1));

        for (const [id, title, icon, label] of visible)
            bar.add_child(this._buildButton(id, title, icon, label));

        if (pages > 1)
            bar.add_child(this._buildPager('›', this._page + 1 < pages, 1));

        return bar;
    }

    _buildPager(glyph, enabled, offset) {
        const button = new St.Button({
            style_class: 'grabit-button grabit-pager',
            can_focus: false,
            reactive: enabled,
            opacity: enabled ? 255 : 96,
        });
        button.set_child(new St.Label({text: glyph}));
        button.connect('clicked', () => {
            this._page += offset;
            // Rebuild in place so the bar keeps its position and timers.
            const replacement = this._buildBarContent();
            const [x, y] = this._bar.get_position();
            Main.layoutManager.uiGroup.remove_child(this._bar);
            this._bar.destroy();
            this._bar = replacement;
            Main.layoutManager.uiGroup.add_child(replacement);
            replacement.set_position(x, y);
            this._connectHover(replacement);
        });
        return button;
    }

    /** Put `bar` on screen at the pointer with the shared timers armed. */
    _showBar(bar) {
        Main.layoutManager.uiGroup.add_child(bar);
        this._bar = bar;
        this._place(bar);
        this._connectHover(bar);

        this._timeoutTimer = GLib.timeout_add(
            GLib.PRIORITY_DEFAULT, TIMEOUT_MS, () => {
                this._timeoutTimer = 0;
                this._hidePopup();
                return GLib.SOURCE_REMOVE;
            });
    }

    _connectHover(bar) {
        // The bar sits next to the pointer, not under it, so dismissal waits
        // until the pointer has actually been on the bar at least once.
        let hovered = false;
        bar.connect('notify::hover', () => {
            if (bar.hover) {
                hovered = true;
                this._clearDismissTimer();
            } else if (hovered) {
                this._armDismissTimer();
            }
        });
    }

    HidePopup() {
        this._hidePopup();
    }

    /** Show what an action produced, in place of the buttons. */
    ShowResult(title, body) {
        this._hidePopup();

        const box = new St.BoxLayout({
            style_class: 'grabit-bar grabit-result',
            vertical: true,
            reactive: true,
            track_hover: true,
        });

        const header = new St.BoxLayout({style_class: 'grabit-result-header'});
        header.add_child(new St.Label({
            text: title,
            style_class: 'grabit-result-title',
            x_expand: true,
        }));

        const copy = new St.Button({style_class: 'grabit-button', can_focus: false});
        copy.set_child(new St.Icon({icon_name: 'edit-copy-symbolic', icon_size: 14}));
        copy.connect('clicked', () => {
            St.Clipboard.get_default().set_text(St.ClipboardType.CLIPBOARD, body);
            this._hidePopup();
        });
        header.add_child(copy);

        const close = new St.Button({style_class: 'grabit-button', can_focus: false});
        close.set_child(new St.Icon({icon_name: 'window-close-symbolic', icon_size: 14}));
        close.connect('clicked', () => this._hidePopup());
        header.add_child(close);

        box.add_child(header);
        const label = new St.Label({text: body, style_class: 'grabit-result-body'});
        label.clutter_text.line_wrap = true;
        box.add_child(label);

        this._showBar(box);
    }

    /**
     * Send Ctrl+V to whatever holds keyboard focus.
     *
     * The bar is never focusable, so focus is still on the application the user
     * selected in. A Clutter virtual device needs no portal and therefore no
     * consent dialog.
     */
    Paste() {
        this._sendKeys([
            [Clutter.KEY_Control_L, Clutter.KeyState.PRESSED],
            [Clutter.KEY_v, Clutter.KeyState.PRESSED],
            [Clutter.KEY_v, Clutter.KeyState.RELEASED],
            [Clutter.KEY_Control_L, Clutter.KeyState.RELEASED],
        ]);
    }

    /** Send Delete to whatever holds keyboard focus — the second half of the
     *  daemon's built-in Cut, after it has copied the selection. */
    Delete() {
        this._sendKeys([
            [Clutter.KEY_Delete, Clutter.KeyState.PRESSED],
            [Clutter.KEY_Delete, Clutter.KeyState.RELEASED],
        ]);
    }

    _sendKeys(keys) {
        if (!this._virtualDevice) {
            const seat = Clutter.get_default_backend().get_default_seat();
            this._virtualDevice =
                seat.create_virtual_device(Clutter.InputDeviceType.KEYBOARD_DEVICE);
        }

        // Clutter wants microseconds.
        const time = global.get_current_time() * 1000;
        for (const [keyval, state] of keys)
            this._virtualDevice.notify_keyval(time, keyval, state);
    }

    // --- internals ---------------------------------------------------------

    _buildButton(id, title, icon, label) {
        const button = new St.Button({
            style_class: 'grabit-button',
            can_focus: false,
            x_expand: false,
        });

        // Fall back to a short label when the icon theme lacks the requested
        // symbolic name, so the action stays usable either way.
        if (icon && St.IconTheme.new().has_icon(icon)) {
            button.set_child(new St.Icon({icon_name: icon, icon_size: 16}));
        } else {
            button.set_child(new St.Label({text: label || title}));
        }

        button.connect('clicked', () => {
            // Take the bar down before the action runs: it may paste, and the
            // keystroke must not race the bar's own teardown.
            this._hidePopup();
            this._dbus?.emit_signal('ActionInvoked', new GLib.Variant('(s)', [id]));
        });
        return button;
    }

    _place(bar) {
        const [pointerX, pointerY] = global.get_pointer();
        const monitor = Main.layoutManager.currentMonitor;
        const [, width] = bar.get_preferred_width(-1);
        const [, height] = bar.get_preferred_height(-1);

        // Prefer below-right of the pointer, but flip rather than run off-screen.
        let x = pointerX + OFFSET_X;
        let y = pointerY + OFFSET_Y;
        if (x + width > monitor.x + monitor.width)
            x = pointerX - OFFSET_X - width;
        if (y + height > monitor.y + monitor.height)
            y = pointerY - OFFSET_Y - height;

        x = Math.max(monitor.x, Math.min(x, monitor.x + monitor.width - width));
        y = Math.max(monitor.y, Math.min(y, monitor.y + monitor.height - height));
        bar.set_position(Math.round(x), Math.round(y));
    }

    _armDismissTimer() {
        this._clearDismissTimer();
        this._dismissTimer = GLib.timeout_add(
            GLib.PRIORITY_DEFAULT, DISMISS_MS, () => {
                this._dismissTimer = 0;
                if (!this._bar?.hover)
                    this._hidePopup();
                return GLib.SOURCE_REMOVE;
            });
    }

    _clearDismissTimer() {
        if (this._dismissTimer) {
            GLib.Source.remove(this._dismissTimer);
            this._dismissTimer = 0;
        }
    }

    _hidePopup() {
        this._clearDismissTimer();
        if (this._timeoutTimer) {
            GLib.Source.remove(this._timeoutTimer);
            this._timeoutTimer = 0;
        }
        if (this._bar) {
            this._bar.destroy();
            this._bar = null;
        }
    }
}
