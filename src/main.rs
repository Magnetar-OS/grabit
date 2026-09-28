// SPDX-License-Identifier: GPL-3.0-only
//! grabit — selection-triggered actions for Wayland desktops.
//!
//! Highlight text anywhere, get a small bar of actions over it. The daemon is
//! desktop-independent; only the front-end differs:
//!
//! * **layer-shell** (cosmic-comp, KWin, wlroots) — grabit watches the primary
//!   selection itself and draws the bar in an overlay surface.
//! * **GNOME Shell** — Mutter exposes neither capability to clients, so the
//!   companion shell extension does the watching and drawing.

mod actions;
mod classify;
mod clipboard;
mod config;
mod control;
mod detect;
mod engine;
mod focus;
mod i18n;
mod inject;
mod selection;
mod settings_ui;
mod ui_cosmic;
mod ui_gnome;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::engine::Engine;
use crate::inject::{Injector, VirtualKeyboard};

#[derive(Parser)]
#[command(name = "grabit", version, about = "Selection-triggered actions for Wayland desktops")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon. This is the default when no subcommand is given.
    Run,
    /// Show the action bar for the current selection in a running daemon.
    Show,
    /// Hide the action bar.
    Hide,
    /// Make a running daemon re-read its configuration.
    Reload,
    /// Stop a running daemon.
    Quit,
    /// List the actions that are loaded and where they came from.
    Actions,
    /// Open the settings window.
    Settings,
    /// Report what this session supports and which front-end would be used.
    Doctor,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("grabit=info"))
        .init();

    i18n::init();

    match Cli::parse().command.unwrap_or(Cmd::Run) {
        Cmd::Run => run(),
        Cmd::Show => control::call("Show"),
        Cmd::Hide => control::call("Hide"),
        Cmd::Reload => control::call("Reload"),
        Cmd::Quit => control::call("Quit"),
        Cmd::Actions => list_actions(),
        Cmd::Settings => settings_ui::run(),
        Cmd::Doctor => doctor(),
    }
}

fn run() -> Result<()> {
    let loaded = config::load().context("loading configuration")?;
    log::info!("{} actions loaded", loaded.actions.len());

    let capabilities = detect::probe()?;

    // Results from `after = "show"` actions travel back to whichever front-end
    // is drawing the bar.
    let (feedback_tx, feedback_rx) = async_channel::bounded(16);

    if capabilities.supports_layer_frontend() {
        let injector = open_injector(capabilities.virtual_keyboard);
        let engine = Engine::new(loaded, injector, feedback_tx);
        let Some(commands) = control::serve()? else {
            return already_running();
        };

        // The watcher needs its own connection and thread: it blocks on Wayland
        // events and on reading selection transfers.
        let (selections, receiver) = async_channel::bounded(16);
        std::thread::Builder::new()
            .name("grabit-selection".into())
            .spawn(move || {
                if let Err(e) = selection::data_control::run(selections) {
                    log::error!("selection watcher stopped: {e:#}");
                }
            })
            .context("spawning the selection watcher")?;

        log::info!("front-end: layer-shell");
        return ui_cosmic::run(ui_cosmic::Flags {
            engine,
            commands,
            selections: receiver,
            feedback: feedback_rx,
        });
    }

    if ui_gnome::is_available() {
        let injector: Option<Box<dyn Injector>> = match ui_gnome::ShellInjector::new() {
            Ok(injector) => Some(Box::new(injector)),
            Err(e) => {
                log::warn!("cannot reach the shell extension for pasting: {e:#}");
                None
            }
        };
        let engine = Engine::new(loaded, injector, feedback_tx);
        let Some(commands) = control::serve()? else {
            return already_running();
        };
        log::info!("front-end: GNOME Shell extension");
        return ui_gnome::run(engine, commands, feedback_rx);
    }

    // Not a failure a restart can fix — GNOME before its extension is
    // enabled, say — so exit with the status the user unit does not restart
    // on, rather than failing into a restart loop until the start limit.
    log::error!("{}", explain_unsupported(&capabilities));
    std::process::exit(UNSUPPORTED_SESSION)
}

/// Exit status for a session grabit cannot run in. `data/grabit.service`
/// names it in `RestartPreventExitStatus=`. 78 is `EX_CONFIG` in sysexits.h.
const UNSUPPORTED_SESSION: i32 = 78;

/// A second `grabit run` is a no-op that succeeds: the autostart entry and the
/// user unit can both start grabit at login, and whichever comes second must
/// neither run a daemon nobody can reach nor fail and be restarted forever.
fn already_running() -> Result<()> {
    log::info!("grabit is already running (it owns {}); nothing to do", control::BUS_NAME);
    Ok(())
}

fn open_injector(supported: bool) -> Option<Box<dyn Injector>> {
    if !supported {
        log::warn!(
            "this compositor has no zwp_virtual_keyboard_manager_v1; actions with \
             `after = \"replace\"` will copy but not paste"
        );
        return None;
    }
    match VirtualKeyboard::new() {
        Ok(keyboard) => Some(Box::new(keyboard)),
        Err(e) => {
            log::warn!("virtual keyboard unavailable: {e:#}");
            None
        }
    }
}

/// Say exactly which piece is missing rather than "unsupported desktop".
fn explain_unsupported(capabilities: &detect::Capabilities) -> String {
    let mut missing = Vec::new();
    if !capabilities.data_control {
        missing
            .push("ext-data-control-v1 or wlr-data-control-v1 v2 (needed to watch the selection)");
    }
    if !capabilities.layer_shell {
        missing.push("zwlr_layer_shell_v1 (needed to place the popup)");
    }

    format!(
        "this compositor is missing: {}.\n\
         On GNOME this is expected — install and enable the grabit GNOME Shell \
         extension, which provides both, then start grabit again.",
        missing.join("; ")
    )
}

fn list_actions() -> Result<()> {
    let loaded = config::load()?;
    for action in &loaded.actions {
        let kind = match (&action.spec.builtin, &action.spec.url, &action.spec.exec) {
            (Some(builtin), _, _) => format!("builtin {builtin:?}").to_lowercase(),
            (_, Some(url), _) => format!("url {url}"),
            (_, _, Some(exec)) => format!("exec {}", shell_words::join(exec)),
            _ => "selection".to_owned(),
        };
        println!(
            "{:<12} {:<24} order={:<4} {}{}\n{:<12} {}",
            action.spec.id,
            action.spec.title,
            action.spec.order,
            kind,
            if action.spec.enabled { "" } else { "  [disabled]" },
            "",
            action.origin.display(),
        );
    }
    Ok(())
}

fn doctor() -> Result<()> {
    let capabilities = detect::probe()?;
    let gnome = ui_gnome::is_available();

    let yes_no = |ok: bool| if ok { fl!("yes") } else { fl!("no") };
    // Widths are set from the longest label so the report stays aligned in
    // whichever language it is read.
    let rows = [
        (fl!("doctor-selection"), yes_no(capabilities.data_control)),
        (fl!("doctor-layer-shell"), yes_no(capabilities.layer_shell)),
        (fl!("doctor-virtual-keyboard"), yes_no(capabilities.virtual_keyboard)),
        (
            fl!("doctor-per-app"),
            capabilities.per_app.map_or_else(|| fl!("no"), |s| s.describe().to_owned()),
        ),
        (fl!("doctor-shell-extension"), yes_no(gnome)),
    ];
    let width = rows.iter().map(|(label, _)| label.chars().count()).max().unwrap_or(0);

    println!("{}", fl!("doctor-capabilities"));
    for (label, value) in &rows {
        let padding = " ".repeat(width - label.chars().count());
        println!("  {label}{padding} : {value}");
    }

    println!();
    let front_end = if capabilities.supports_layer_frontend() {
        fl!("doctor-frontend-layer")
    } else if gnome {
        fl!("doctor-frontend-gnome")
    } else {
        fl!("doctor-frontend-none")
    };
    println!("{}: {front_end}", fl!("doctor-frontend"));

    // The app id the exclude list has to name; run from a terminal, it is the
    // terminal's own.
    if capabilities.per_app.is_some() {
        match focus::spawn() {
            Ok(focus) => {
                // cosmic-comp sends activation on its next refresh rather than
                // in reply to the request, so give it a moment to arrive.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
                while focus.current().is_none() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                let app = focus.current().unwrap_or_else(|| fl!("doctor-focused-unknown"));
                println!("{}: {app}", fl!("doctor-focused"));
            }
            Err(e) => println!("{}: {e:#}", fl!("doctor-focused")),
        }
    }

    if !capabilities.virtual_keyboard && !gnome {
        println!("{}", fl!("doctor-no-paste"));
    }

    println!();
    println!("{}: {}", fl!("doctor-config"), config::user_config_dir()?.display());
    match config::load() {
        Ok(loaded) => {
            let count = loaded.actions.len();
            println!("{}: {}", fl!("doctor-actions"), fl!("doctor-actions-loaded", count = count));
            for skipped in &loaded.skipped {
                println!(
                    "  {}",
                    fl!(
                        "doctor-action-skipped",
                        path = skipped.path.display().to_string(),
                        error = skipped.error.as_str()
                    )
                );
            }
        }
        Err(e) => println!(
            "{}: {}",
            fl!("doctor-actions"),
            fl!("doctor-actions-failed", error = format!("{e:#}"))
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The user unit restarts on failure; an unsupported session is not a
    /// failure a restart can fix, so the unit has to exempt its exit status.
    #[test]
    fn the_user_unit_does_not_restart_an_unsupported_session() {
        let unit = include_str!("../data/grabit.service");
        let expected = format!("RestartPreventExitStatus={}", super::UNSUPPORTED_SESSION);
        assert!(unit.lines().any(|line| line.trim() == expected), "missing `{expected}`");
    }
}
