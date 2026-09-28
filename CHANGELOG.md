# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- A "grabit Settings" entry in the applications menu. The settings window was
  only reachable as `grabit settings` from a terminal; the daemon's own entry
  is autostart-only and hidden.
- The settings window says when the session cannot tell grabit which
  application is focused, instead of offering an exclude list that would have
  no effect there.
- The settings window shows why a change could not be saved — an unreadable
  `config.toml`, a manifest it cannot rewrite — instead of only logging it
  while the control snaps back.
- `grabit doctor` and the settings window list action files that failed to
  load, with the reason. A broken drop-in was skipped with only a log line.

### Fixed

- Excluded applications are honoured on COSMIC. The focused app was only read
  from the wlroots foreign-toplevel protocol, which cosmic-comp does not offer,
  so on COSMIC the exclude list did nothing. grabit now follows focus through
  `zcosmic_toplevel_info_v1` there, and an app that loses focus without another
  gaining it no longer counts as focused. `grabit doctor` names the protocol in
  use and prints the focused app's id — the id the exclude list has to match.
- Selecting text no longer blurs the whole screen on COSMIC when the theme's
  frosted system interface is on. The invisible full-screen surface that waits
  for the pointer was being frosted like any other panel; it now opts out.
- `grabit reload`, and every change made in the settings window, now applies
  the `[selection]` limits and `settle_ms` to the next selection. Both were
  read once at startup, so they only took effect after a restart.
- A drag selection no longer occasionally shows no bar: a slow read of an
  earlier point in the drag, finishing last, was treated as the selection being
  cleared.
- Moving the pointer off the bar and back onto it no longer closes the bar
  under the pointer, and a result shown in the bar stays up for the full
  `timeout_ms` instead of closing on the deadline armed for the buttons.
- A selected action manifest that declares `[options]` and runs a command is
  now installed disabled and loadable. The `enabled = false` line was appended
  inside the last options table, so the file failed to load.
- "Open in file manager" opens `~/…` paths; `{{path}}` now expands the leading
  `~/` to the home directory, which an argv never does by itself.
- The daemon no longer grows for as long as the session runs. Every selection
  change — one per pointer motion during a drag — left its data-control offer
  alive in grabit and in the compositor.

## [1.1.0] - 2026-09-21

### Changed

- The application ID is `com.magnetaros.Grabit`, like the rest of the suite;
  it was still `io.github.idominikos.Grabit`. The autostart entry, AppStream
  metadata and window IDs follow. Settings live in `~/.config/grabit` and are
  unaffected.

### Fixed

- A second `grabit run` exits successfully instead of failing. The package
  installs both an autostart entry and a systemd user unit; with both active,
  whichever started second errored, and the unit's `Restart=on-failure`
  retried it until systemd gave up.

## [1.0.4] - 2026-09-16

### Fixed

- The package is signed. 1.0.3 reached the `[magnetar]` pacman repository
  unsigned, so a machine using the repository's documented
  `SigLevel = Required` refused to install it.

## [1.0.3] - 2026-09-10

### Fixed

- The pacman repository is actually published now. The release secrets are
  passed to the release kit by name instead of with `secrets: inherit`, which
  does not carry secrets across organisations — the kit is in entro314-labs and
  this repository in Magnetar-OS, so `ARCH_REPO_TOKEN` arrived empty and the
  `arch-repo` job staged, validated and skipped the push while still reporting
  success. Signing secrets were passed the same way and would have failed the
  same way.

## [1.0.2] - 2026-09-10

### Fixed

- The pacman repository is published again: the release now carries the
  `ARCH_REPO_TOKEN` the `arch-repo` job needs, which had been absent, so the
  job staged and validated the repository and then stopped without pushing.

## [1.0.1] - 2026-09-10

### Fixed

- The Arch package is a valid package again, so the pacman repository updates.
  Packaging the install tree as one `type: tree` entry at `/` made nfpm emit a
  tar entry with an empty filename; bsdtar errors on it, so `repo-add` rejected
  the package as "not a package file". Packaged as one tree per top-level
  directory instead. The `.deb` and `.rpm` payloads are byte-identical — they
  tolerated the empty entry, which is why only Arch broke.

## [1.0.0] - 2026-09-10

### Added

- Selection-triggered action bar for Wayland sessions.
- Layer-shell front-end for cosmic-comp, KWin and wlroots compositors, built on
  libcosmic, watching the primary selection through `ext-data-control-v1` with a
  fallback to `wlr-data-control-v1` version 2.
- Three-surface popup placement — a permanent dummy surface, a throwaway
  full-screen surface that reports the pointer position, and a margin-anchored
  bar — following the pattern cosmic-launcher uses.
- GNOME Shell extension front-end, since Mutter exposes neither selection
  monitoring nor surface placement to clients.
- Drop-in actions as one TOML file each, with regex matching, `url` and argv
  `exec` forms, placeholder expansion, and `copy` / `replace` handling of output.
- Paste-over-selection via `zwp_virtual_keyboard_v1`, or via the shell extension
  on GNOME.
- `grabit doctor` to report per-capability Wayland support and the front-end that
  would be chosen.
- D-Bus control interface at `org.grabit.Daemon` with `Show`, `Hide`, `Reload`
  and `Quit`, exposed as CLI subcommands for use from compositor keybindings.
- Fluent localization for the strings grabit itself shows, with the desktop entry
  and AppStream metainfo generated from the same catalogue at build time.
- `justfile` following the COSMIC ecosystem's build and install conventions,
  replacing the Makefile.
- Per-action `[options]` — an action declares its own settings, which appear in
  the settings window beneath it as a dropdown (`choices`) or a text field, and
  expand into `url` and `exec` through `{{option:NAME}}`.
  The packaged `translate` action uses one for its target language.
- `GPL-3.0-only` licensing, matching the other COSMIC applications in the set,
  declared in `Cargo.toml`, the AppStream metainfo, the packages and per source
  file as an SPDX identifier.
- `docs/cosmic-conventions.md`, recording the patterns the COSMIC projects share.

[Unreleased]: https://github.com/Magnetar-OS/grabit/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/Magnetar-OS/grabit/compare/v1.0.4...v1.1.0
[1.0.4]: https://github.com/Magnetar-OS/grabit/compare/v1.0.3...v1.0.4
[1.0.3]: https://github.com/Magnetar-OS/grabit/compare/v1.0.2...v1.0.3
[1.0.2]: https://github.com/Magnetar-OS/grabit/compare/v1.0.1...v1.0.2
[1.0.1]: https://github.com/Magnetar-OS/grabit/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/Magnetar-OS/grabit/releases/tag/v1.0.0
