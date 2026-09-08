# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
- `docs/cosmic-conventions.md`, recording the patterns the COSMIC projects share.
