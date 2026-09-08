# Roadmap

The direction: grabit should be able to stand next to [PopClip] feature for
feature, while being the best-behaved citizen a COSMIC session can host —
pixel-perfect against the COSMIC design language, instant to appear, and
delightful to use. PopClip is the reference because it is the app grabit's
README already names as the idea; where a PopClip feature cannot exist on
Wayland, this document says so rather than pretending.

[PopClip]: https://www.popclip.app/

Milestones are ordered by dependency, not by date. Each one ends with
acceptance criteria; a milestone is done when they pass, not when its code
merges.

---

## Where grabit stands today

Working: selection monitoring (`ext`/`wlr-data-control`), the three-surface
layer-shell popup on cosmic-comp/KWin/wlroots, the GNOME Shell extension
front-end, drop-in TOML actions with regex matching and `url`/`exec` forms,
placeholder expansion, `copy`/`replace` output handling, paste via
`zwp_virtual_keyboard_v1`, D-Bus control, `grabit doctor`, Fluent i18n with
generated XDG metadata, the COSMIC `justfile` conventions.

Not yet: most of PopClip's built-in intelligence, any GUI for configuration,
per-app behavior, action overflow, packaging, CI, and the last layer of visual
polish ([docs/cosmic-conventions.md] tracks the known gaps — spacing tokens
chief among them).

[docs/cosmic-conventions.md]: docs/cosmic-conventions.md

---

## PopClip parity matrix

The target feature set, mapped to what Wayland allows.

| PopClip feature | grabit today | Feasible on Wayland | Milestone |
| --- | --- | --- | --- |
| Bar appears on mouse selection | Yes (first pointer motion — compositor fact, see README) | — | done |
| Copy | Yes | — | done |
| Search (configurable engine) | Yes (action) | — | done |
| Open link | Yes (action) | — | done |
| Cut | No | Yes — copy + injected `Delete` | M2 |
| Paste over selection | Partial (`after = "replace"`) | Yes | done |
| Paste as its own bar action | No | Yes — inject `Ctrl+V` | M2 |
| URL / email / path / phone detection with contextual defaults | Regex per action only | Yes — a detection pass over the selection | M2 |
| Dictionary / spelling lookup | No | Yes — packaged `exec` actions (hunspell, dictd) | M2 |
| Inline result preview (e.g. translation shown in the bar) | No | Yes — a second bar state | M2 |
| Extension directory, one-file install | Drop-in TOML files | Yes by design | done |
| Snippets — select an extension's text, get an install offer | No | Yes — detect a grabit manifest in the selection | M3 |
| Per-extension options with a UI | No | Yes | M3 |
| Bar overflow / paging when actions exceed the width | Truncates at `max_actions` | Yes | M1 |
| Excluded / per-app behavior | No | Compositor-dependent — toplevel-info protocols; the GNOME extension knows the focused app natively | M4 |
| Modifier to suppress the bar (Shift in PopClip) | No | Yes — keyboard state via the seat | M2 |
| Rich text / HTML capture | Plain text only | Where the source offers `text/html` through data-control | M4 |
| Menu-bar app for settings | TOML only | Yes — settings window + COSMIC applet | M3 |
| Appears without any mouse movement | No | **No** — compositors re-evaluate pointer focus on motion only; `grabit show` is the keyboard path | — |

---

## M1 — Pixel-perfect, delightful bar

The bar is the whole product surface; it must look designed, not assembled.

- **Spacing tokens.** Replace every literal padding/margin in `ui_cosmic.rs`
  with `theme.cosmic().space_*()` values, per the conventions doc. The bar
  must read as a COSMIC `Dropdown` at any scale factor.
- **Motion.** Fade/scale-in on appear, fade-out on dismiss, using libcosmic's
  animation support. Target: appear animation under 150 ms, no first-frame
  flash of an unstyled or half-laid-out bar.
- **Hover and press states** on every action, from the theme's `Button::Icon`
  states — verified against light, dark, and non-default accent themes.
- **Overflow.** When matched actions exceed the configured width, page the bar
  (chevron at the end) instead of silently truncating at `max_actions`.
- **Keyboard.** Once the bar has pointer focus, arrow keys move between
  actions, `Return` activates, `Escape` dismisses.
- **Placement intelligence.** Flip below the pointer when the bar would leave
  the top edge; clamp at output edges; correct behavior on multi-output and
  fractional scaling.
- **RTL** layouts follow the locale.

*Done when:* side-by-side screenshots against cosmic-launcher show matching
corner radii, spacing rhythm, and state colors in light and dark; overflow,
keyboard navigation, and edge placement each have a test or a scripted
verification; no literal spacing values remain in the UI code.

## M2 — PopClip parity: built-in intelligence

What makes PopClip feel smart is that the bar already knows what you selected.

- **Detection pass.** Before matching actions, classify the selection: URL,
  email address, file path, phone number, color literal. Expose the result to
  manifests as a `detects = ["url"]` matcher (regex `match` stays; detection
  composes with it) and as placeholders (`{{url}}`, `{{email}}`).
- **Cut and Paste** as packaged actions, built on the existing injection path:
  cut is copy plus an injected `Delete`; paste injects `Ctrl+V`. Both degrade
  with a visible notice where virtual-keyboard is absent, as `replace` does
  today.
- **Packaged action set** expanded to PopClip's defaults: dictionary lookup and
  spell-correct (via `exec`, using whatever the host provides — hunspell,
  `dict`), open path in file manager, compose email, dial `tel:` URIs.
- **Inline results.** An action may declare `after = "show"`: the bar swaps to
  a result view (the translation, the definition) with copy/dismiss. This is
  the single biggest step toward PopClip's feel.
- **Suppression modifier.** Holding the configured modifier during selection
  keeps the bar away; read from the seat's keyboard state at settle time.
- **Selection quality.** Smart trimming of trailing punctuation for URL
  detection, configurable per action, matching PopClip's behavior.

*Done when:* every parity-matrix row marked M2 works on cosmic-comp and KWin;
each detection type has unit tests over a corpus of true and false positives;
the packaged actions ship enabled (except anything shell-equivalent, which
stays opt-in like `shell.toml`).

## M3 — Configuration surfaces: settings window, applet, snippets

TOML-first stays the design — the settings UI edits the same files the user
can edit by hand, and never grows a second store.

- **Settings window** (`grabit settings`): a libcosmic application styled like
  a cosmic-settings page. Popup behavior, selection limits, and the action
  list — enable/disable, reorder (writes `order`), edit fields, delete —
  all writing the existing TOML.
- **Daemon settings on `cosmic-config`** with the `dbus-config` feature, so
  `config.toml`'s `[popup]`/`[selection]` values apply live from COSMIC
  Settings. Actions stay as drop-in files, per the conventions doc — that
  rejection stands.
- **COSMIC panel applet**: a status item to pause/resume grabit, open
  settings, and show at a glance whether the session supports it (`doctor` in
  icon form).
- **Per-extension options.** A manifest may declare `[options]` (string, bool,
  choice); values are stored beside the action file and exposed as
  placeholders. The settings window renders them; PopClip's extension options
  panel is the model.
- **Snippets.** When the selection itself parses as a grabit action manifest,
  offer a single Install action — PopClip's snippets flow. Install is
  explicit, shows the manifest, and lands the file in
  `~/.config/grabit/actions/` disabled if it contains `exec`.

*Done when:* a user can go from default install to a customized action set
without opening an editor; every settings-window change is observable in the
TOML and vice versa after `reload`; the applet builds against the COSMIC
applet API and ships in the same binary or a sibling crate.

## M4 — Deep session integration

- **Per-app rules.** Exclude list and per-app action overrides, keyed on app
  id. Sources, in order of preference: `zcosmic_toplevel_info` on COSMIC,
  `ext-foreign-toplevel-list`/`wlr-foreign-toplevel-management` elsewhere, the
  shell extension's native knowledge on GNOME. `doctor` reports which is
  available; without any, the feature is absent, not broken.
- **Rich text capture.** When the selection source offers `text/html`, read it
  alongside the plain text and expose `{{html}}` / `{{markdown}}` (converted)
  placeholders for actions that want them.
- **GNOME front-end parity.** Everything M1–M3 adds must either work through
  the extension or be reported by `doctor` as unavailable on GNOME — the
  extension is a front-end, not a second-class port.
- **GlobalShortcuts portal**, once COSMIC implements it, as the preferred
  binding path for `grabit show`, with the current compositor-settings
  instructions as fallback.

*Done when:* `doctor` accurately reports per-app support on COSMIC, KDE, a
wlroots compositor, and GNOME; excluding an app verifiably keeps the bar away
in that app only; HTML capture round-trips from a browser selection into a
markdown-consuming action.

## M5 — Architecture, quality, performance

Standing discipline rather than a final phase — but with concrete deliverables.

- **Crate split** once the settings window and applet exist: `grabit-core`
  (selection, actions, engine, injection — no UI dependency), with the daemon,
  settings window, and applet as thin binaries over it. Not before; splitting
  2,800 lines today would be structure without need.
- **Tests where the risk is.** Unit coverage for detection, matching,
  placeholder expansion, and manifest parsing; a headless-compositor
  integration test (wlroots in headless mode) exercising selection → bar →
  action end to end; the injection path tested against a virtual seat.
- **CI**: `just check` (tests + clippy `-D warnings`) and `cargo fmt --check`
  on every push, plus a build against libcosmic's current git head so drift
  surfaces early — libcosmic is a moving git dependency and breakage should be
  found by CI, not by users.
- **Performance budget, enforced.** Selection-settle to bar-visible under
  100 ms on cosmic-comp; daemon RSS under 40 MB idle; startup to first
  selection watch under 200 ms. Measured by a benchmark harness in the repo,
  run in CI, with regressions failing the build. The renderer is wgpu via
  libcosmic/iced already — the work is verifying the softbuffer fallback path
  still functions where wgpu cannot initialize, and keeping the release binary
  small (`lto`, `strip` already in place).
- **Packaging**: `debian/` and RPM spec under `packaging/`, a `flake.nix`, and
  the vendoring recipes from the conventions doc. Flatpak is investigated
  honestly: data-control and virtual-keyboard are not sandbox-friendly, so the
  likely outcome is documented native packaging rather than a hobbled Flatpak.
- **Releases**: `release.config.json` wired to conventional commits, changelog
  generation from the existing CHANGELOG discipline, signed tags, and a
  versioned protocol between daemon and GNOME extension so the two can be
  updated independently.

*Done when:* CI is green and required; the benchmark harness runs in CI with
the stated budgets; at least one distribution package installs and runs on a
clean system; a release can be cut with one command.

---

## Explicit non-goals

Held from the README and conventions doc so this roadmap cannot drift into
them:

- **No X11 backend.** Wayland-only remains by design.
- **No shell-string `exec`.** Actions stay argv-form; `shell.toml` stays
  disabled by default.
- **No second configuration store.** Every GUI writes the TOML the user could
  have written.
- **No bar-before-motion on stock compositors.** Physics of pointer focus, not
  a bug; the keyboard path is `grabit show`.
- **No translation of log output.**
