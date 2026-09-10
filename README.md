# grabit

Select text anywhere, get a small bar of actions over it — copy, search,
translate, open, transform, run. PopClip's idea, built for Wayland.

Wayland-only by design. There is no X11 backend and none is planned.

```
$ grabit doctor
Wayland capabilities
  selection monitoring (data-control) : yes
  overlay placement (layer-shell)     : yes
  key injection (virtual-keyboard)    : yes
  GNOME Shell extension               : no

front-end: layer-shell
```

## Supported sessions

| Session | Selection | Popup | Paste |
| --- | --- | --- | --- |
| COSMIC (cosmic-comp) | `ext-data-control-v1` | `wlr-layer-shell` | `virtual-keyboard` |
| KDE Plasma (KWin) | `ext-data-control-v1` | `wlr-layer-shell` | `virtual-keyboard` |
| Sway, Hyprland, Niri, river | `ext`/`wlr-data-control` | `wlr-layer-shell` | `virtual-keyboard` |
| GNOME (Mutter) | GNOME Shell extension | GNOME Shell extension | GNOME Shell extension |

The layer-shell front-end is built on [libcosmic][libcosmic], so it is themed
with the rest of COSMIC. Nothing about it is COSMIC-specific at the protocol
level — it speaks plain `wlr-layer-shell`, which is why KWin and the wlroots
compositors work too.

[libcosmic]: https://github.com/pop-os/libcosmic

Mutter implements no protocol that lets a client watch the selection or place a
surface at a point on screen, and [will not][mutter-524]. On GNOME the companion
shell extension provides all three; the daemon still owns the actions.

[mutter-524]: https://gitlab.gnome.org/GNOME/mutter/-/work_items/524

## Install

Needs [`just`](https://github.com/casey/just), the build runner the COSMIC
ecosystem uses.

```sh
just                # cargo build --release
sudo just install
systemctl --user enable --now grabit.service
```

On GNOME, additionally:

```sh
just install-extension
# log out and back in, then
gnome-extensions enable grabit@grabit.local
```

`just --list` shows the rest. `just check` runs the tests and a clippy pass with
warnings denied; `just doctor` reports what the current session supports.

## Using it

Select text with the mouse. The bar appears next to the pointer.

**The bar appears on the first mouse movement after you select**, not the
instant you release. That is not a delay that was chosen — Wayland compositors
only re-evaluate which surface the pointer is over when the pointer *moves*, so
an overlay cannot ask where the cursor is; it has to be told. In practice you
are still holding the mouse, so the bar arrives as you start moving toward it.

A selection made with the keyboard shows nothing until you nudge the mouse. If
you want a keyboard trigger, bind a shortcut to `grabit show`.

Since neither COSMIC nor most wlroots compositors implement the
`GlobalShortcuts` portal, the binding is made in the compositor's own settings:

* COSMIC — Settings → Keyboard → Custom Shortcuts → `grabit show`
* KDE — System Settings → Shortcuts → Add Command → `grabit show`
* Sway/Hyprland — `bindsym $mod+g exec grabit show`

## Commands

| | |
| --- | --- |
| `grabit run` | run the daemon (the default) |
| `grabit show` | show the bar for the current selection |
| `grabit hide` | take the bar down |
| `grabit reload` | re-read the configuration |
| `grabit quit` | stop the daemon |
| `grabit actions` | list loaded actions and where they came from |
| `grabit doctor` | report what this session supports |

## Configuration

`~/.config/grabit/config.toml` is created on first run:

```toml
[popup]
settle_ms = 140     # how long the selection must stop changing before the bar shows
max_actions = 8
icon_size = 16
offset_x = 12       # bar position relative to the pointer
offset_y = 18
dismiss_ms = 900    # hide this long after the pointer leaves the bar. 0 disables
timeout_ms = 8000   # hide unconditionally after this long. 0 disables

[selection]
min_length = 1
max_length = 20000
ignore_whitespace_only = true
```

## Actions

One TOML file per action in `~/.config/grabit/actions/`, so an action is a file
you can drop in, edit or delete. Packaged actions live in
`/usr/share/grabit/actions/`; a user file with the same `id` replaces the
packaged one.

```toml
id = "search"                 # required, unique
title = "Search the web"      # required, shown as the tooltip
icon = "system-search-symbolic"
label = "Search"              # drawn when the icon theme has no such icon
order = 20                    # lower sorts first
enabled = true

match = '\S'                  # only offer this action when the selection matches
not_match = '^\s*https?://'   # suppress it when the selection matches

url = "https://duckduckgo.com/?q={{text}}"
```

An action is either a `url` or an `exec`, never both:

```toml
id = "upper"
title = "UPPERCASE"
exec = ["tr", "[:lower:]", "[:upper:]"]
stdin = true                  # pipe the selection in instead of expanding it
after = "replace"             # what to do with stdout
```

`exec` is an argv, not a shell line, so selected text can never be reinterpreted
as shell syntax. Set `stdin = true` for anything that might be long.

`after` decides what happens to the output:

| | |
| --- | --- |
| `ignore` | discard it (default) |
| `copy` | put it on the clipboard |
| `replace` | put it on the clipboard and paste over the selection |

An action with neither `url` nor `exec` treats the selection itself as the
result, which is all the built-in `copy` action is.

### Placeholders

| | |
| --- | --- |
| `{{text}}` | the selection — percent-encoded inside `url`, literal inside `exec` |
| `{{text_raw}}` | never encoded |
| `{{text_trimmed}}`, `{{text_trimmed_raw}}` | with surrounding whitespace removed |
| `{{text_url}}`, `{{text_trimmed_url}}` | always percent-encoded |
| `{{text_line}}` | the first line only |

After editing anything: `grabit reload`.

## Notes and limits

* **`Run as shell command` ships disabled.** It hands the highlighted text to a
  shell, so one mis-click runs whatever you selected. Enable it in
  `~/.config/grabit/actions/shell.toml` if you want it.
* The overlay accepts clicks only where the bar is. While it is waiting to learn
  the pointer position it accepts them everywhere in principle, but focus does
  not move to it until the pointer does — so a click made without moving still
  reaches the application underneath.
* Some compositors expose `wlr-data-control` only at version 1, which has no
  primary-selection event. `grabit doctor` reports that as no selection
  monitoring.
* `after = "replace"` needs key injection. Where that is missing the replacement
  still reaches the clipboard, and grabit says so rather than failing quietly.

## How the popup is positioned

Worth knowing before changing anything in `ui_cosmic.rs`, because the design is
entirely dictated by two Wayland facts.

Layer-shell anchors surfaces to screen edges, not to coordinates, and no
protocol tells a client where the pointer is. So there are three surfaces:

* **dummy** — one pixel on the background layer with an empty input region,
  never destroyed. It exists only so the process always owns a surface.
* **hunter** — full-screen, transparent, accepts all pointer input. Created when
  a selection settles, destroyed the moment it learns where the pointer is.
* **bar** — the buttons, anchored top-left with margins set to the pointer
  position. Its input region is its own bounds.

The hunter has to *wait* to be told the pointer position, because compositors
re-evaluate which surface the pointer is over on motion, not when a surface
appears beneath it. This was measured on cosmic-comp: a full-screen overlay
accepting all input received no pointer event whatsoever until the mouse moved.
The same property is what makes the hunter safe — focus does not transfer until
the pointer moves, so a click made without moving still reaches the application
underneath.

## Translations

Fluent catalogues live in `i18n/`, one directory per language. Very little is
translatable, and that is the point: every label and tooltip in the popup comes
from the user's own action manifests, so it is their text rather than ours. What
`i18n/en/grabit.ftl` holds is the application identity — which `build.rs` expands
into the desktop entry and the AppStream metainfo, one localized line per
language — plus the failure notification and `grabit doctor`.

Log output is deliberately not translated. It is read while debugging, and a
translated log line is harder to search for rather than easier.

## Further reading

[`ROADMAP.md`](ROADMAP.md) lays out the path to feature parity with PopClip and
full COSMIC integration, milestone by milestone.

[`docs/cosmic-conventions.md`](docs/cosmic-conventions.md) records the patterns
the COSMIC projects share — build and install layout, generated XDG metadata,
localization, and the layer-surface rules that shaped this front-end — along with
which of them grabit follows and which it deliberately does not.

## Development

```sh
just check          # tests plus clippy with warnings denied
RUST_LOG=grabit=debug cargo run
```

`WAYLAND_DEBUG=1` alongside `RUST_LOG=grabit=debug` is the fastest way to see
what the compositor is actually being told:

```sh
WAYLAND_DEBUG=1 RUST_LOG=grabit=debug cargo run 2>&1 \
  | grep -E 'get_layer_surface|set_input_region|wl_pointer'
```

## License

GPL-3.0-only. See [LICENSE](LICENSE).
