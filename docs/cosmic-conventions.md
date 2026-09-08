# COSMIC ecosystem conventions

Patterns shared across the COSMIC projects, recorded while building grabit's
layer-shell front-end. Each one says what the convention is, where it is
evidenced, and what grabit does about it — including where grabit deliberately
departs.

Read against these checkouts, all current as of 2026-08:

| Repository | Commit | Why it was useful |
| --- | --- | --- |
| [libcosmic](https://github.com/pop-os/libcosmic) | `ef490df5` | The toolkit; layer-surface and popup APIs |
| [cosmic-launcher](https://github.com/pop-os/cosmic-launcher) | `a9ad093` | An on-demand overlay — closest analogue to grabit |
| [cosmic-panel](https://github.com/pop-os/cosmic-panel) | `3c08c30` | Long-lived layer surfaces |
| [cosmic-app-template](https://github.com/pop-os/cosmic-app-template) | `97ff759` | The canonical skeleton |
| [cosmic-applet-template](https://github.com/pop-os/cosmic-applet-template) | `58f506f` | Applet variant of the same |
| [cosmic-edit](https://github.com/pop-os/cosmic-edit) | `0e9c927` | A full application in the same shape |

---

## Repository layout

Every project lands in the same shape:

```
justfile              build and install entry point
i18n.toml             Fluent configuration
i18n/<lang>/<crate>.ftl
src/{main,app,config,i18n}.rs
data/                 <APPID>.desktop, <APPID>.metainfo.xml, icons/, justfile
resources/            templates, when the metadata is generated rather than written
debian/               distro packaging
flake.nix             Nix development shell
hooks/pre-commit.hook rustfmt check
.github/              CI
```

`data/` holds finished files and `resources/` holds templates expanded at build
time — the templates have moved to the latter, older applications still use the
former.

**grabit**: follows this, with `data/` reduced to the systemd unit because
everything else is generated, and `gnome-extension/` added for the GNOME
companion, which has no COSMIC equivalent.

---

## Build and install: `just`, not `make`

Every project uses [`just`](https://github.com/casey/just), with a recognisable
preamble:

```just
export NAME := 'cosmic-launcher'
export APPID := 'com.system76.CosmicLauncher'

rootdir := ''
prefix := '/usr'
debug := '0'

base-dir := absolute_path(clean(rootdir / prefix))
cargo-target-dir := env('CARGO_TARGET_DIR', 'target')
bin-src := cargo-target-dir / (if debug == '1' { 'debug' } else { 'release' }) / NAME
bin-dst := base-dir / 'bin' / NAME

default: build-release
```

The parts that matter:

- **`rootdir` is the staging root**, the `DESTDIR` equivalent. Every destination
  is derived from it, so `just rootdir=/tmp/stage install` produces a complete
  tree without touching the system. Distro packaging depends on this.
- **`cargo-target-dir` reads `CARGO_TARGET_DIR`** rather than hardcoding
  `target/`. Anything a build script emits has to honour the same variable or the
  install recipes will not find it.
- **`NAME` and `APPID` are exported**, so nested justfiles inherit them.
  cosmic-launcher's root `install` recipe is three lines that delegate:
  ```just
  install:
      install -Dm0755 {{bin-src}} {{bin-dst}}
      @just data/install
      @just data/icons/install
  ```
- **`default: build-release`**, so a bare `just` builds a release binary.
- **`check`** runs clippy. cosmic-launcher uses `-W clippy::pedantic`.
- **Vendoring recipes** (`vendor`, `vendor-extract`, `build-vendored`) produce a
  `vendor.tar` for offline distro builds.
- **Optional linker detection** — clang plus mold when both are present.

**grabit**: adopted, minus the nested justfiles (too small to need them) and the
vendoring recipes (nothing packages it yet). `just check` runs the tests *and*
clippy with `-D warnings`, which is stricter than pedantic warnings.

---

## Application identity

A reverse-DNS `APPID` is the primary key for everything else:

- `const APP_ID: &str` on the `cosmic::Application` impl
- `<APPID>.desktop`
- `<APPID>.metainfo.xml`
- `<APPID>.svg` in `share/icons/hicolor/scalable/apps/`

System76 uses `com.system76.CosmicLauncher`. Third-party projects should use a
domain they control, or `io.github.<user>.<App>` for GitHub-hosted ones.

**Keep the constant and the filenames identical.** They are joined by
`<launchable type="desktop-id">` in the metainfo, and by the compositor when it
matches a running application to its desktop entry.

**grabit**: `io.github.idominikos.Grabit` throughout, even though grabit is a
daemon whose desktop entry is `NoDisplay=true` and exists only to autostart it.

---

## Generated XDG metadata: `xdgen`

Both templates now generate the desktop entry and metainfo at build time instead
of maintaining them by hand, because the name, comment and keywords in them are
translatable and would otherwise be three copies to keep in sync:

```rust
// build.rs
use xdgen::{App, Context, FluentString};

let ctx = Context::new("i18n", env::var("CARGO_PKG_NAME").unwrap()).unwrap();
let app = App::new(FluentString("app-title"))
    .comment(FluentString("app-comment"))
    .keywords(FluentString("app-keywords"));

let desktop = app.expand_desktop("resources/app.desktop", &ctx).unwrap();
let metainfo = app.expand_metainfo("resources/app.metainfo.xml", &ctx).unwrap();
```

Two things are easy to get wrong:

- **xdgen replaces keys, it does not add them.** A template with no `Name=` line
  gets no `Name=` line, silently. The metainfo template likewise needs `<name>`
  and `<summary>` present, or the expansion fails outright with `missing tag
  name`. Put placeholder values in and let xdgen localize them.
- **The templates hardcode `target/xdgen/`**, which breaks under
  `CARGO_TARGET_DIR`. Reading the variable costs one line.

Output looks like this, one variant per language in the catalogue:

```ini
Name=grabit
Name[en]=grabit
Comment=Selection-triggered actions for Wayland desktops
Comment[en]=Selection-triggered actions for Wayland desktops
```

**grabit**: adopted, with the `CARGO_TARGET_DIR` fix.

---

## Localization

`i18n-embed` with the Fluent backend, catalogues embedded in the binary via
`rust-embed`, and a crate-local `fl!` macro:

```toml
# i18n.toml
fallback_language = "en"

[fluent]
assets_dir = "i18n"
```

`src/i18n.rs` is close to identical in every project: a `RustEmbed` struct over
`i18n/`, a `LazyLock<FluentLanguageLoader>` that loads the fallback language, and
a `macro_rules! fl` wrapping `i18n_embed_fl::fl!`. The catalogue is
`i18n/<lang>/<crate_name>.ftl` — underscored crate name, not the binary name.
Translations arrive through Hosted Weblate.

One thing no template mentions: **Fluent wraps interpolated values in Unicode
bidi isolates by default**, which are correct in a text widget and appear as
stray characters in a terminal. `loader.set_use_isolating(false)` turns them off,
and it has to be called *after* `select()`, which rebuilds the bundles.

**grabit**: adopted, with a narrower scope than the templates assume. Every label
and tooltip in grabit's popup comes from the user's own action manifests, so it
is their text and not translatable by us. The catalogue holds the application
identity, the failure notification and `grabit doctor` — and nothing else. Log
output is deliberately left untranslated: it is read while debugging, and a
translated log line is harder to grep for, not easier.

---

## Application shape

```rust
impl cosmic::Application for App {
    type Executor = cosmic::executor::single::Executor;
    type Flags = Args;
    type Message = Message;

    const APP_ID: &'static str = "com.system76.CosmicLauncher";

    fn init(core: Core, flags: Args) -> (Self, Task<Message>);
    fn core(&self) -> &Core;
    fn core_mut(&mut self) -> &mut Core;
    fn view(&self) -> Element<'_, Message>;
    fn view_window(&self, id: SurfaceId) -> Element<'_, Message>;
    fn update(&mut self, message: Message) -> Task<Message>;
    fn subscription(&self) -> Subscription<Message>;
}
```

`Core` carries the shared shell state and must be handed back through
`core`/`core_mut`. `Flags` is how anything the application needs but cannot
rebuild gets in — channels, parsed arguments, handles.

For anything that is not a conventional windowed app:

```rust
Settings::default()
    .no_main_window(true)     // surfaces are created on demand
    .exit_on_close(false)     // closing one is not the app exiting
```

With `no_main_window`, `view()` is unreachable and `view_window(id)` dispatches on
surface id. cosmic-launcher writes `unreachable!("No main window")`.

`Subscription::run_with(data, builder)` takes **a plain `fn` pointer**, not a
closure, and identifies the subscription by hashing `data`. Anything the stream
needs must therefore travel inside `data`, and `data` must implement `Hash`. For
channels, which cannot meaningfully be hashed and never change, hash a constant
name and clone the channel out inside the builder.

`listen_raw(|event, status, id| ...)` is how surface-tagged input is received —
the `id` is what tells one surface's events from another's.

**grabit**: adopted wholesale. The daemon runs `no_main_window` with three
surfaces distinguished by id in `view_window`.

---

## Layer surfaces

The most valuable thing in these repositories, and the part with the least
documentation elsewhere.

### Settings

```rust
SctkLayerSurfaceSettings {
    id: SurfaceId::unique(),
    layer: Layer::Overlay,
    keyboard_interactivity: KeyboardInteractivity::None,
    input_zone: Some(Vec::new()),
    anchor: Anchor::TOP | Anchor::LEFT,
    output: IcedOutput::Active,
    namespace: "my-surface".into(),
    margin: IcedMargin { top: y, left: x, ..Default::default() },
    size: None,
    exclusive_zone: -1,
    size_limits: Limits::NONE.min_width(1.0).min_height(1.0),
}
```

- **`input_zone: Option<Vec<Rectangle>>`** — `None` accepts all input, `Some(vec![])`
  accepts none. It compiles to plain `wl_surface.set_input_region`, so it is
  portable to any wlr-layer-shell compositor, and libcosmic records the surface
  in a `to_commit` map so the region actually reaches the wire. Doing this by
  hand through a toolkit that does not track pending commits is where the bodies
  are buried: a region set on a surface that renders no new frame is silently
  never applied.
- **`margin` is how you position an anchored surface.** Anchor to two adjacent
  edges and the margins become coordinates. There is no positioner, so flipping
  and clamping near screen edges is the client's job — unlike xdg popups.
- **`size: None` autosizes to content**; `Some((None, None))` with opposite-edge
  anchors stretches across the output.
- **`exclusive_zone: -1`** opts out of *other* clients' exclusive zones, so the
  surface covers the whole output including under panels. Any full-screen overlay
  wants this.
- **`IcedOutput::Active`** targets the focused output, which removes the need to
  build one surface per monitor.
- **`KeyboardInteractivity::None`** where focus must stay with the application
  underneath.

Runtime changes go through `set_margin`, `set_anchor`, `set_exclusive_zone`,
`set_layer`, `set_size`, `set_padding`, and an `InputZone` action. All of them
mark the surface for commit.

### Create and destroy, never hide and show

`get_layer_surface` per appearance, `destroy_layer_surface` per disappearance.
cosmic-launcher's `show()` and `hide()` do exactly this.

This is not a stylistic preference. Hiding and re-showing a surface can make a
toolkit build a second `zwlr_layer_surface_v1` on the same `wl_surface`, which
smithay-based compositors reject as a role reassignment and answer by dropping
the client. Creating a fresh surface each time gives a fresh `wl_surface` and
sidesteps it.

### The dummy surface

```rust
fn create_dummy_layer_surface(&mut self) -> Task<Message> { /* … */ }
```

cosmic-launcher keeps a permanently mapped surface on the bottom layer with an
empty input region, purely so the process always owns one. A toolkit that loses
its last surface tends to close its Wayland connection, and the next surface then
has nothing to be created on. Any application whose surfaces all come and go
needs one of these.

### Popups get a positioner, layer surfaces do not

For a menu at a point, an xdg popup parented to a layer surface does the
constraint solving compositor-side:

```rust
SctkPopupSettings {
    parent: self.window_id,
    positioner: SctkPositioner {
        anchor_rect: Rectangle { x, y, width: 1, height: 1 },
        anchor: Anchor::Right,
        gravity: Gravity::Right,
        constraint_adjustment: 15,   // flip and slide on both axes
        reactive: true,
        ..Default::default()
    },
    grab: true,
    ..
}
```

libcosmic's applet tooltips use `input_zone: Some(vec![Rectangle::new(Point::new(-1000., -1000.), Size::default())])`
— an off-screen zero-size rectangle — to make a popup entirely click-through.

### What nothing here solves

**No COSMIC project can ask where the pointer is.** cosmic-launcher tracks
`Mouse(CursorMoved)` within its own surface and only ever uses the value after
the user has already interacted with it. There is no protocol for a global cursor
position, and `wp_pointer_warp_v1` does not help because warping needs the
surface to already hold pointer focus.

Worse, and measured on cosmic-comp rather than inferred: **a compositor
re-evaluates which surface the pointer is over on motion, not when a surface
appears beneath it.** A full-screen overlay accepting all input received no
pointer event whatsoever until the mouse was nudged.

The same property is also a safety net. Because focus does not transfer until the
pointer moves, an overlay that opens its input region does not steal the next
click — a click made without moving still reaches the application underneath.

**grabit**: this shaped the whole front-end. Three surfaces — a permanent dummy, a
throwaway full-screen "hunter" that waits to be told the pointer position, and a
margin-anchored bar — with the hunter torn down after four seconds if nothing
moves. See [`src/ui_cosmic.rs`](../src/ui_cosmic.rs).

---

## Configuration

`cosmic-config`, with a derive and an explicit version:

```rust
use cosmic::cosmic_config::{self, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry};

#[derive(Debug, Default, Clone, CosmicConfigEntry, Eq, PartialEq)]
#[version = 1]
pub struct Config {
    demo: String,
}
```

The `dbus-config` feature watches for changes through cosmic-settings-daemon, so
edits made in COSMIC Settings apply live.

**grabit deliberately does not use this.** Its configuration is one TOML file per
action, and being able to drop an action in, edit it and delete it is the feature
— the same thing a PopClip extension is. Moving that behind a settings daemon
would trade the point of the design for ecosystem tidiness. The daemon settings
in `config.toml` could reasonably move to cosmic-config later; the actions should
not.

---

## Theming

Use the semantic classes rather than literal colours, so light and dark and
accent changes are handled:

```rust
widget::container(content).class(theme::Container::Dropdown)   // floating surface
widget::button::custom(content).class(theme::Button::Icon)
```

`Container` covers `WindowBackground`, `Background`, `Card`, `Dialog`, `Dropdown`,
`List`, `Primary`, `Secondary`, `Tooltip`, `Transparent` (the default) and
`Custom`. `Button` covers `Standard` (default), `Suggested`, `Destructive`,
`Icon`, `IconVertical`, `Link`, `Text`, `AppletIcon`, `AppletMenu`, menu variants
and `Custom`.

Spacing comes from the theme rather than from literals:

```rust
let theme = cosmic::theme::active();
let cosmic = theme.cosmic();
let padding: Padding = [cosmic.space_xxs(), cosmic.space_m()].into();
```

**grabit**: uses `Container::Dropdown` for the bar and `Button::Icon` for the
actions; the padding is still literal and should move to spacing tokens.

---

## Rust conventions

- `edition = "2024"`, with `rust-version` pinned in `Cargo.toml`.
- libcosmic is a **git dependency**, not a crates.io one, with features selected
  explicitly and a commented-out `path` override for local work:
  ```toml
  # libcosmic = { path = "../libcosmic" }
  ```
- Tokio as the executor, behind the `tokio` feature.
- `#[allow(clippy::too_many_lines)]` on `update` methods, which grow large by the
  nature of the pattern.
- A rustfmt `pre-commit` hook in `hooks/`.

---

## What grabit took, and what it left

| Convention | Status |
| --- | --- |
| `justfile` with `rootdir`/`prefix`/`debug` | Adopted |
| Reverse-DNS `APPID` across binary and metadata | Adopted |
| `xdgen` generation of desktop entry and metainfo | Adopted, with a `CARGO_TARGET_DIR` fix |
| `i18n-embed` + Fluent + `fl!` | Adopted, scoped to what grabit itself says |
| `cosmic::Application` with `no_main_window` | Adopted |
| Dummy surface, create/destroy per appearance | Adopted |
| `input_zone`, `IcedOutput::Active`, margin positioning | Adopted |
| Semantic theme classes | Adopted; spacing tokens still to do |
| `cosmic-config` | Rejected — drop-in TOML actions are the design |
| Nested `data/justfile`, vendoring recipes | Skipped — nothing packages grabit yet |
| `debian/`, `flake.nix`, `hooks/`, CI | Not yet |
