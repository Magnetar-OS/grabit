// SPDX-License-Identifier: GPL-3.0-only
//! The layer-shell front-end, used on cosmic-comp, KWin and wlroots.
//!
//! # Three surfaces
//!
//! Wayland gives a client no way to ask where the pointer is, and layer-shell
//! positions surfaces by anchoring them to screen edges rather than to
//! coordinates. The way around both is three separate layer surfaces, following
//! the pattern cosmic-launcher uses:
//!
//! * **dummy** — one pixel on the bottom layer with an empty input region, never
//!   destroyed. It exists only so the process always owns a surface; a toolkit
//!   that loses its last one tends to tear its Wayland connection down.
//! * **hunter** — full-screen, transparent, accepts pointer input. Created when a
//!   selection settles and destroyed the moment it learns where the pointer is.
//! * **bar** — the actual buttons, anchored top-left with margins set to the
//!   pointer position. Its input region is its own bounds, so nothing else on
//!   screen is affected while it is up.
//!
//! # Waiting for motion
//!
//! Compositors re-evaluate which surface the pointer is over when the pointer
//! *moves*, not when a surface appears beneath it. Measured on cosmic-comp: a
//! full-screen overlay accepting all input received no pointer event at all
//! until the mouse was nudged. So the hunter cannot ask where the pointer is; it
//! waits to be told.
//!
//! That suits the interaction. You are still holding the mouse when a selection
//! settles, so the first movement afterwards both hands over coordinates and
//! feels like the bar following the cursor. And because focus only transfers on
//! motion, a click made without moving still lands in the application below —
//! the hunter is not a trap. If nothing moves within [`HUNT_WINDOW`] it is torn
//! down and no bar appears.
//!
//! # Keyboard
//!
//! A bar raised by a settled selection never takes keyboard focus: focus must
//! stay in the application so a paste lands where the user is working. A bar
//! raised by `grabit show` — the keyboard trigger — takes it exclusively, so
//! the arrows, Return and Escape work; the compositor returns focus when the
//! bar is destroyed, before any injected keystroke fires.

use std::time::Duration;

use anyhow::{Context, Result};
use cosmic::app::{Core, Settings, Task};
use cosmic::iced::event::listen_raw;
use cosmic::iced::keyboard::key::Named;
use cosmic::iced::platform_specific::runtime::wayland::layer_surface::SctkLayerSurfaceSettings;
use cosmic::iced::platform_specific::shell::commands::layer_surface::{
    Anchor, KeyboardInteractivity, Layer, destroy_layer_surface, get_layer_surface, set_margin,
};
use cosmic::iced::runtime::core::layout::Limits;
use cosmic::iced::runtime::core::window::Id as SurfaceId;
use cosmic::iced::runtime::platform_specific::wayland::layer_surface::{IcedMargin, IcedOutput};
use cosmic::iced::{self, Length, Point, Size, Subscription};
use cosmic::widget;
use cosmic::{Element, theme};
use futures::StreamExt;

use crate::clipboard;
use crate::config::After;
use crate::control::Command;
use crate::engine::{Button, Engine, Feedback};
use crate::selection::{self, Grab, Raw, Settled};

/// How long the hunter waits to be told where the pointer is before giving up.
const HUNT_WINDOW: Duration = Duration::from_secs(4);

/// The entrance animation: the bar starts this many logical pixels below its
/// place and slides up over [`ANIM_STEPS`] frames.
const ANIM_SLIDE: f32 = 12.0;
const ANIM_STEPS: u8 = 6;
const ANIM_FRAME: Duration = Duration::from_millis(16);

/// Everything the app needs that cannot be rebuilt from `Core`.
pub struct Flags {
    pub engine: Engine,
    pub commands: async_channel::Receiver<Command>,
    pub selections: async_channel::Receiver<Raw>,
    pub feedback: async_channel::Receiver<Feedback>,
}

pub fn run(flags: Flags) -> Result<()> {
    // No main window: grabit is a daemon whose surfaces come and go, and closing
    // one must never be read as the application exiting.
    let settings = Settings::default()
        .antialiasing(true)
        .client_decorations(false)
        .debug(false)
        .no_main_window(true)
        .exit_on_close(false);

    cosmic::app::run::<Grabit>(settings, flags).context("running the layer-shell front-end")
}

/// A keyboard-navigation gesture on the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Next,
    Prev,
    Activate,
    Dismiss,
}

#[derive(Debug, Clone)]
pub enum Message {
    /// A selection settled, or went away.
    Selection(Settled),
    /// The hunter learned where the pointer is, in coordinates on its output.
    Pointer(SurfaceId, Point),
    /// A surface was mapped and reported its size.
    Opened(SurfaceId, Size),
    /// A mapped surface changed size — the bar does when it swaps to a result.
    Resized(SurfaceId, Size),
    /// The pointer entered or left the bar.
    Hover(SurfaceId, bool),
    Invoke(String),
    /// Flip to another page of actions; the offset is ±1.
    Page(i32),
    /// A keyboard gesture, only delivered while the bar holds keyboard focus.
    Key(Nav),
    /// An `after = "show"` action produced its result.
    EngineFeedback(Feedback),
    /// Put the shown result on the clipboard and close.
    CopyResult,
    /// One frame of the entrance animation, for the generation it carries.
    Anim(u64, u8),
    Control(Command),
    /// A timer belonging to the generation it carries expired.
    HuntExpired(u64),
    Expired(u64),
    Dismiss(u64),
}

struct Grabit {
    core: Core,
    engine: Engine,
    commands: async_channel::Receiver<Command>,
    selections: async_channel::Receiver<Raw>,
    feedback: async_channel::Receiver<Feedback>,
    focus: Option<crate::focus::Handle>,

    dummy: SurfaceId,
    hunter: Option<SurfaceId>,
    bar: Option<SurfaceId>,

    /// The selection the visible bar acts on.
    grab: Grab,
    buttons: Vec<Button>,
    /// Which page of actions is showing, when they do not all fit.
    page: usize,
    /// The keyboard-focused action, as an index into the visible page.
    /// `None` when the bar was raised by the mouse and has no keyboard.
    focused: Option<usize>,
    /// Whether this bar took keyboard focus (raised by `grabit show`).
    keyboard: bool,
    /// A result being shown in place of the buttons: (action title, body).
    result: Option<(String, String)>,
    /// An `after = "show"` action is running and the bar is waiting on it.
    awaiting: bool,
    /// Where the bar was asked to appear.
    anchor: Point,
    /// The bar's own size, once it has reported one.
    bar_size: Size,
    /// Size of the output the hunter covered, learned when it was mapped.
    output: Size,
    /// Progress of the entrance animation, `ANIM_STEPS` when settled.
    anim: u8,
    /// Whether the pointer has been over the bar since it appeared. The bar is
    /// drawn *beside* the pointer, so dismissing on the first leave would take
    /// it away the instant it arrived.
    hovered: bool,
    /// Bumped whenever the popup is torn down, so timers armed for an older
    /// popup can tell that they are stale.
    epoch: u64,
}

impl Grabit {
    /// Tear down the hunter and the bar, invalidating any pending timers.
    fn teardown(&mut self) -> Task<Message> {
        self.epoch = self.epoch.wrapping_add(1);
        self.grab = Grab::default();
        self.buttons.clear();
        self.page = 0;
        self.focused = None;
        self.keyboard = false;
        self.result = None;
        self.awaiting = false;
        self.hovered = false;
        self.bar_size = Size::ZERO;
        self.anim = ANIM_STEPS;

        let mut tasks = Vec::new();
        if let Some(id) = self.hunter.take() {
            tasks.push(destroy_layer_surface(id));
        }
        if let Some(id) = self.bar.take() {
            tasks.push(destroy_layer_surface(id));
        }
        Task::batch(tasks)
    }

    /// Start hunting for the pointer so the bar can be placed for `grab`.
    /// `keyboard` marks a bar raised by the keyboard trigger, which navigates.
    fn begin(&mut self, grab: Grab, keyboard: bool) -> Task<Message> {
        let teardown = self.teardown();

        if let Some(focus) = &self.focus
            && let Some(app) = focus.current()
            && self.engine.config().applications.is_excluded(&app)
        {
            log::debug!("selection in excluded app `{app}`; not showing the bar");
            return teardown;
        }

        let buttons = self.engine.buttons(&grab.text);
        if buttons.is_empty() {
            log::debug!("no action matches this selection");
            return teardown;
        }
        self.grab = grab;
        self.buttons = buttons;
        self.keyboard = keyboard;
        self.focused = keyboard.then_some(0);

        let id = SurfaceId::unique();
        self.hunter = Some(id);
        let epoch = self.epoch;

        Task::batch([
            teardown,
            get_layer_surface(SctkLayerSurfaceSettings {
                id,
                layer: Layer::Overlay,
                // Taking keyboard focus would move it away from the application
                // the user selected in, and a paste would then land elsewhere.
                keyboard_interactivity: KeyboardInteractivity::None,
                // No zone means "all input", which is the point of the hunter.
                input_zone: None,
                // Anchoring to opposite edges stretches the surface across the
                // output, which makes surface-local pointer coordinates equal to
                // coordinates on that output.
                anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                output: IcedOutput::Active,
                namespace: "grabit-hunter".into(),
                margin: IcedMargin::default(),
                size: Some((None, None)),
                // -1 opts out of other clients' exclusive zones, so the hunter
                // really does cover the whole output, panels included.
                exclusive_zone: -1,
                size_limits: Limits::NONE.min_width(1.0).min_height(1.0),
            }),
            timer(HUNT_WINDOW, move || Message::HuntExpired(epoch)),
        ])
    }

    /// Replace the hunter with the bar, at the position it reported.
    fn place(&mut self, at: Point) -> Task<Message> {
        let Some(hunter) = self.hunter.take() else {
            return Task::none();
        };
        self.anchor = at;

        let id = SurfaceId::unique();
        self.bar = Some(id);
        let epoch = self.epoch;
        let config = self.engine.config();
        let animate = config.popup.animate;
        self.anim = if animate { 0 } else { ANIM_STEPS };
        let margin = self.current_margin();

        let mut tasks = vec![
            destroy_layer_surface(hunter),
            get_layer_surface(SctkLayerSurfaceSettings {
                id,
                layer: Layer::Overlay,
                // Only the keyboard-triggered bar navigates; a mouse-raised bar
                // must leave focus in the application (see the module docs).
                keyboard_interactivity: if self.keyboard {
                    KeyboardInteractivity::Exclusive
                } else {
                    KeyboardInteractivity::None
                },
                // The bar's own bounds are its input region, so clicks anywhere
                // else go straight to the application underneath.
                input_zone: None,
                // Anchored to two adjacent edges, margins act as coordinates.
                anchor: Anchor::TOP | Anchor::LEFT,
                output: IcedOutput::Active,
                namespace: "grabit".into(),
                margin,
                // None asks the compositor to size the surface to its contents.
                size: None,
                exclusive_zone: -1,
                size_limits: Limits::NONE.min_width(1.0).min_height(1.0),
            }),
        ];
        if animate {
            tasks.push(timer(ANIM_FRAME, move || Message::Anim(epoch, 1)));
        }
        if config.popup.timeout_ms > 0 {
            tasks.push(timer(Duration::from_millis(config.popup.timeout_ms), move || {
                Message::Expired(epoch)
            }));
        }
        Task::batch(tasks)
    }

    /// The margin for the bar right now: its resting place, offset downward by
    /// however much of the entrance slide is still to run.
    fn current_margin(&self) -> IcedMargin {
        let config = self.engine.config();
        let mut margin = place_beside(
            self.anchor,
            self.bar_size,
            self.output,
            (config.popup.offset_x as f32, config.popup.offset_y as f32),
        );
        if self.anim < ANIM_STEPS {
            let t = f32::from(self.anim) / f32::from(ANIM_STEPS);
            let remaining = 1.0 - ease_out_cubic(t);
            margin.top += (ANIM_SLIDE * remaining) as i32;
        }
        margin
    }

    /// The buttons on the current page, with the page count.
    fn visible(&self) -> (&[Button], usize) {
        let per_page = self.engine.config().popup.max_actions.max(1);
        let pages = self.buttons.len().div_ceil(per_page).max(1);
        let start = (self.page.min(pages - 1)) * per_page;
        let end = (start + per_page).min(self.buttons.len());
        (&self.buttons[start..end], pages)
    }

    fn bar_view(&self) -> Element<'_, Message> {
        let cosmic = theme::active();
        let cosmic = cosmic.cosmic();
        let pad_bar = cosmic.space_xxxs();

        if let Some((title, body)) = &self.result {
            return self.result_view(title, body);
        }

        let config = self.engine.config();
        let (visible, pages) = self.visible();
        let mut row =
            widget::row::with_capacity(visible.len() + 2).spacing(cosmic.space_xxxs() / 2);

        if pages > 1 {
            row = row.push(self.pager_button("go-previous-symbolic", "‹", self.page > 0, -1));
        }

        for (index, spec) in visible.iter().enumerate() {
            // Falling back to a label keeps the action usable under icon themes
            // that lack the symbolic name it asked for.
            let has_icon = !spec.icon.is_empty()
                && widget::icon::from_name(spec.icon.clone()).path().is_some();

            let content: Element<'_, Message> = if has_icon {
                widget::icon::from_name(spec.icon.clone())
                    .size(config.popup.icon_size as u16)
                    .into()
            } else {
                widget::text::body(&spec.label).into()
            };

            // The keyboard-focused action reads as the suggested one; there is
            // no hover to mark it otherwise.
            let class = if self.focused == Some(index) {
                theme::Button::Suggested
            } else {
                theme::Button::Icon
            };

            row = row.push(widget::tooltip(
                widget::button::custom(content)
                    .class(class)
                    .padding([cosmic.space_xxxs(), cosmic.space_xxs()])
                    .on_press(Message::Invoke(spec.id.clone())),
                widget::text::body(&spec.title),
                widget::tooltip::Position::Bottom,
            ));
        }

        if pages > 1 {
            row = row.push(self.pager_button("go-next-symbolic", "›", self.page + 1 < pages, 1));
        }

        widget::container(row).class(theme::Container::Dropdown).padding(pad_bar).into()
    }

    fn pager_button(
        &self,
        icon: &str,
        fallback: &str,
        enabled: bool,
        offset: i32,
    ) -> Element<'_, Message> {
        let cosmic = theme::active();
        let cosmic = cosmic.cosmic();
        let has_icon = widget::icon::from_name(icon).path().is_some();
        let content: Element<'_, Message> = if has_icon {
            widget::icon::from_name(icon).size(12).into()
        } else {
            widget::text::body(fallback.to_owned()).into()
        };
        let mut button = widget::button::custom(content)
            .class(theme::Button::Icon)
            .padding([cosmic.space_xxxs(), cosmic.space_xxxs()]);
        if enabled {
            button = button.on_press(Message::Page(offset));
        }
        button.into()
    }

    /// What an `after = "show"` action produced, in place of the buttons.
    fn result_view(&self, title: &str, body: &str) -> Element<'_, Message> {
        let cosmic = theme::active();
        let cosmic = cosmic.cosmic();

        let header = widget::row::with_capacity(3)
            .spacing(cosmic.space_xxs())
            .align_y(iced::Alignment::Center)
            .push(widget::text::caption_heading(title.to_owned()))
            .push(widget::space::horizontal().width(Length::Fill))
            .push(widget::tooltip(
                widget::button::custom(widget::icon::from_name("edit-copy-symbolic").size(14))
                    .class(theme::Button::Icon)
                    .padding(cosmic.space_xxxs())
                    .on_press(Message::CopyResult),
                widget::text::body(crate::fl!("result-copy")),
                widget::tooltip::Position::Bottom,
            ))
            .push(widget::tooltip(
                widget::button::custom(widget::icon::from_name("window-close-symbolic").size(14))
                    .class(theme::Button::Icon)
                    .padding(cosmic.space_xxxs())
                    .on_press(Message::Key(Nav::Dismiss)),
                widget::text::body(crate::fl!("result-close")),
                widget::tooltip::Position::Bottom,
            ));

        let column = widget::column::with_capacity(2)
            .spacing(cosmic.space_xxs())
            .push(header)
            .push(widget::text::body(body.to_owned()));

        widget::container(column)
            .class(theme::Container::Dropdown)
            .padding(cosmic.space_xs())
            .max_width(420.0)
            .into()
    }

    /// Move keyboard focus by `offset`, paging past either end.
    fn navigate(&mut self, offset: i32) -> Task<Message> {
        let (visible, pages) = self.visible();
        let count = visible.len() as i32;
        if count == 0 {
            return Task::none();
        }
        let current = self.focused.unwrap_or(0) as i32;
        let next = current + offset;
        if next < 0 {
            if self.page > 0 {
                self.page -= 1;
                let (previous, _) = self.visible();
                self.focused = Some(previous.len().saturating_sub(1));
            }
        } else if next >= count {
            if self.page + 1 < pages {
                self.page += 1;
                self.focused = Some(0);
            }
        } else {
            self.focused = Some(next as usize);
        }
        Task::none()
    }

    fn invoke(&mut self, action: String) -> Task<Message> {
        // An `after = "show"` action reports back into the bar, so the bar must
        // outlive the invocation; everything else tears down first, because the
        // action may paste and the keystroke must not race the bar's own
        // disappearance.
        if self.engine.action_after(&action) == Some(After::Show) {
            self.awaiting = true;
            self.engine.invoke(&action, self.grab.clone());
            return Task::none();
        }
        let grab = std::mem::take(&mut self.grab);
        let teardown = self.teardown();
        self.engine.invoke(&action, grab);
        teardown
    }
}

impl cosmic::Application for Grabit {
    type Executor = cosmic::executor::single::Executor;
    type Flags = Flags;
    type Message = Message;

    const APP_ID: &'static str = "io.github.idominikos.Grabit";

    fn init(mut core: Core, flags: Flags) -> (Self, Task<Message>) {
        core.set_app_type(cosmic::core::AppType::System);
        core.set_keyboard_nav(false);

        // Per-app rules need to know the focused app; where the protocol is
        // missing the tracker is absent and no exclusion applies.
        let focus = match crate::focus::spawn() {
            Ok(handle) => Some(handle),
            Err(e) => {
                log::info!("per-app rules unavailable: {e:#}");
                None
            }
        };

        let dummy = SurfaceId::unique();
        let app = Grabit {
            core,
            engine: flags.engine,
            commands: flags.commands,
            selections: flags.selections,
            feedback: flags.feedback,
            focus,
            dummy,
            hunter: None,
            bar: None,
            grab: Grab::default(),
            buttons: Vec::new(),
            page: 0,
            focused: None,
            keyboard: false,
            result: None,
            awaiting: false,
            anchor: Point::ORIGIN,
            bar_size: Size::ZERO,
            output: Size::new(f32::MAX, f32::MAX),
            anim: ANIM_STEPS,
            hovered: false,
            epoch: 0,
        };

        // A surface that exists for no reason other than to exist. Without it
        // the process would own none between popups.
        let task = get_layer_surface(SctkLayerSurfaceSettings {
            id: dummy,
            layer: Layer::Background,
            keyboard_interactivity: KeyboardInteractivity::None,
            // An empty zone accepts nothing, so it cannot swallow a click.
            input_zone: Some(Vec::new()),
            anchor: Anchor::TOP | Anchor::LEFT,
            output: IcedOutput::Active,
            namespace: "grabit-dummy".into(),
            margin: IcedMargin::default(),
            size: Some((Some(1), Some(1))),
            exclusive_zone: -1,
            size_limits: Limits::NONE.min_width(1.0).min_height(1.0),
        });

        (app, task)
    }

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn view(&self) -> Element<'_, Message> {
        // `no_main_window` means this is never reached, but the trait requires
        // something; an empty surface is the honest answer.
        widget::space::horizontal().into()
    }

    fn view_window(&self, id: SurfaceId) -> Element<'_, Message> {
        if Some(id) == self.bar {
            return self.bar_view();
        }

        // The hunter is deliberately blank and covers its output; the dummy is
        // blank and one pixel. Neither draws anything the user can see.
        let fill = if id == self.dummy { Length::Fixed(1.0) } else { Length::Fill };
        widget::container(widget::space::horizontal())
            .width(fill)
            .height(fill)
            .class(theme::Container::Transparent)
            .into()
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Selection(Settled::Text(grab)) => self.begin(grab, false),
            Message::Selection(Settled::Cleared) => self.teardown(),

            Message::Pointer(id, at) => {
                if Some(id) != self.hunter {
                    return Task::none();
                }
                log::debug!("pointer at {},{}", at.x, at.y);
                self.place(at)
            }

            Message::Opened(id, size) | Message::Resized(id, size) => {
                if Some(id) == self.hunter {
                    // A surface anchored to all four edges is exactly the size
                    // of its output, which is what the bar is clamped against.
                    self.output = size;
                } else if Some(id) == self.bar {
                    // Now that the bar's real size is known, nudge it back
                    // on-screen if the first guess put it over an edge.
                    self.bar_size = size;
                    return set_margin_of(id, self.current_margin());
                }
                Task::none()
            }

            Message::Anim(epoch, step) => {
                if epoch != self.epoch || self.bar.is_none() {
                    return Task::none();
                }
                self.anim = step.min(ANIM_STEPS);
                let margin = self.current_margin();
                let apply = set_margin_of(self.bar.expect("checked above"), margin);
                if step < ANIM_STEPS {
                    let next = step + 1;
                    Task::batch([apply, timer(ANIM_FRAME, move || Message::Anim(epoch, next))])
                } else {
                    apply
                }
            }

            Message::Hover(id, inside) => {
                if Some(id) != self.bar {
                    return Task::none();
                }
                if inside {
                    self.hovered = true;
                    return Task::none();
                }
                let dismiss = self.engine.config().popup.dismiss_ms;
                if !self.hovered || dismiss == 0 {
                    return Task::none();
                }
                let epoch = self.epoch;
                timer(Duration::from_millis(dismiss), move || Message::Dismiss(epoch))
            }

            Message::Invoke(action) => self.invoke(action),

            Message::Page(offset) => {
                let (_, pages) = self.visible();
                let next = self.page as i32 + offset;
                if next >= 0 && (next as usize) < pages {
                    self.page = next as usize;
                    if self.focused.is_some() {
                        self.focused = Some(0);
                    }
                }
                Task::none()
            }

            Message::Key(nav) => {
                if self.bar.is_none() {
                    return Task::none();
                }
                match nav {
                    Nav::Next => self.navigate(1),
                    Nav::Prev => self.navigate(-1),
                    Nav::Activate => {
                        if self.result.is_some() {
                            return self.teardown();
                        }
                        let (visible, _) = self.visible();
                        match self.focused.and_then(|i| visible.get(i)) {
                            Some(button) => {
                                let id = button.id.clone();
                                self.invoke(id)
                            }
                            None => Task::none(),
                        }
                    }
                    Nav::Dismiss => self.teardown(),
                }
            }

            Message::EngineFeedback(Feedback::Result { title, body }) => {
                if self.bar.is_none() {
                    // The bar is gone — the user moved on — so the result has
                    // nowhere honest to appear.
                    log::info!("result from “{title}” arrived after the bar closed");
                    return Task::none();
                }
                self.awaiting = false;
                self.result = Some((title, body));
                // A result is being read; give it a fresh lifetime.
                let timeout = self.engine.config().popup.timeout_ms;
                let epoch = self.epoch;
                if timeout > 0 {
                    timer(Duration::from_millis(timeout), move || Message::Expired(epoch))
                } else {
                    Task::none()
                }
            }

            Message::CopyResult => {
                if let Some((_, body)) = &self.result
                    && let Err(e) = clipboard::set(body)
                {
                    log::error!("copying the result: {e:#}");
                }
                self.teardown()
            }

            Message::Control(Command::Show) => match crate::control::current_primary_selection() {
                Ok(text) if selection::is_interesting(&text, &self.engine.config().selection) => {
                    self.begin(Grab::text(text), true)
                }
                Ok(_) => {
                    log::info!("nothing selected");
                    Task::none()
                }
                Err(e) => {
                    log::error!("reading the primary selection: {e:#}");
                    Task::none()
                }
            },
            Message::Control(Command::Hide) => self.teardown(),
            Message::Control(Command::Reload) => {
                if let Err(e) = self.engine.reload() {
                    log::error!("{e:#}");
                }
                Task::none()
            }
            Message::Control(Command::Quit) => {
                let teardown = self.teardown();
                Task::batch([teardown, cosmic::iced::exit()])
            }

            Message::HuntExpired(epoch) => {
                if epoch != self.epoch || self.hunter.is_none() {
                    return Task::none();
                }
                log::debug!("pointer never moved; not showing the bar for this selection");
                self.teardown()
            }
            Message::Expired(epoch) | Message::Dismiss(epoch) => {
                if epoch != self.epoch {
                    return Task::none();
                }
                self.teardown()
            }
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let config = self.engine.config();

        let feed = Feed {
            name: "",
            selections: self.selections.clone(),
            commands: self.commands.clone(),
            feedback: self.feedback.clone(),
            config: config.selection.clone(),
            settle: Duration::from_millis(config.popup.settle_ms),
        };

        let selections =
            Subscription::run_with(Feed { name: "grabit-selections", ..feed.clone() }, |feed| {
                selection::settled(feed.selections.clone(), feed.config.clone(), feed.settle)
                    .map(Message::Selection)
                    .boxed()
            });

        let commands =
            Subscription::run_with(Feed { name: "grabit-commands", ..feed.clone() }, |feed| {
                feed.commands.clone().map(Message::Control).boxed()
            });

        let feedback = Subscription::run_with(Feed { name: "grabit-feedback", ..feed }, |feed| {
            feed.feedback.clone().map(Message::EngineFeedback).boxed()
        });

        let events = listen_raw(|event, _status, id| match event {
            iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                Some(Message::Pointer(id, position))
            }
            iced::Event::Mouse(iced::mouse::Event::CursorEntered) => Some(Message::Hover(id, true)),
            iced::Event::Mouse(iced::mouse::Event::CursorLeft) => Some(Message::Hover(id, false)),
            iced::Event::Window(iced::window::Event::Opened { size, .. }) => {
                Some(Message::Opened(id, size))
            }
            iced::Event::Window(iced::window::Event::Resized(size)) => {
                Some(Message::Resized(id, size))
            }
            // Key events only arrive while the keyboard-triggered bar holds
            // focus; a mouse-raised bar never takes it.
            iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, .. }) => {
                match key.as_ref() {
                    iced::keyboard::Key::Named(Named::ArrowRight | Named::ArrowDown) => {
                        Some(Message::Key(Nav::Next))
                    }
                    iced::keyboard::Key::Named(Named::ArrowLeft | Named::ArrowUp) => {
                        Some(Message::Key(Nav::Prev))
                    }
                    iced::keyboard::Key::Named(Named::Enter) => Some(Message::Key(Nav::Activate)),
                    iced::keyboard::Key::Named(Named::Escape) => Some(Message::Key(Nav::Dismiss)),
                    iced::keyboard::Key::Named(Named::Tab) => Some(Message::Key(Nav::Next)),
                    _ => None,
                }
            }
            _ => None,
        });

        Subscription::batch([selections, commands, feedback, events])
    }
}

/// Margins that put a bar of `size` next to `anchor` without leaving `output`.
///
/// An anchored layer surface is positioned by its margins, and layer-shell has
/// no positioner to flip or slide it, so that is done here: prefer below-right
/// of the pointer, flip to the other side when that would overflow, and clamp as
/// a last resort for a bar too large to fit on either side.
fn place_beside(anchor: Point, size: Size, output: Size, offset: (f32, f32)) -> IcedMargin {
    let mut x = anchor.x + offset.0;
    let mut y = anchor.y + offset.1;
    if x + size.width > output.width {
        x = anchor.x - offset.0 - size.width;
    }
    if y + size.height > output.height {
        y = anchor.y - offset.1 - size.height;
    }
    x = x.clamp(0.0, (output.width - size.width).max(0.0));
    y = y.clamp(0.0, (output.height - size.height).max(0.0));

    IcedMargin { top: y as i32, right: 0, bottom: 0, left: x as i32 }
}

fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// Inputs handed to a subscription.
///
/// `Subscription::run_with` identifies a subscription by hashing this and takes a
/// plain function pointer, so the struct has to carry everything the stream needs
/// and hash to something stable. Only the name participates: the channels are
/// fixed for the lifetime of the process, and hashing them is neither possible
/// nor meaningful.
#[derive(Clone)]
struct Feed {
    name: &'static str,
    selections: async_channel::Receiver<Raw>,
    commands: async_channel::Receiver<Command>,
    feedback: async_channel::Receiver<Feedback>,
    config: crate::config::Selection,
    settle: Duration,
}

impl std::hash::Hash for Feed {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

/// One-shot timer as a task, so timers cancel by becoming stale rather than by
/// being tracked and removed.
fn timer(after: Duration, message: impl FnOnce() -> Message + Send + 'static) -> Task<Message> {
    cosmic::task::future(async move {
        tokio::time::sleep(after).await;
        message()
    })
}

fn set_margin_of(id: SurfaceId, margin: IcedMargin) -> Task<Message> {
    set_margin(id, margin.top, margin.right, margin.bottom, margin.left)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUT: Size = Size { width: 1920.0, height: 1080.0 };
    const BAR: Size = Size { width: 200.0, height: 40.0 };
    const OFFSET: (f32, f32) = (12.0, 18.0);

    #[test]
    fn sits_below_right_of_the_pointer_when_there_is_room() {
        let margin = place_beside(Point::new(500.0, 500.0), BAR, OUTPUT, OFFSET);
        assert_eq!((margin.left, margin.top), (512, 518));
    }

    #[test]
    fn flips_to_the_left_rather_than_overflowing() {
        let margin = place_beside(Point::new(1900.0, 500.0), BAR, OUTPUT, OFFSET);
        assert_eq!(margin.left, 1900 - 12 - 200);
    }

    #[test]
    fn flips_upward_rather_than_overflowing() {
        let margin = place_beside(Point::new(500.0, 1070.0), BAR, OUTPUT, OFFSET);
        assert_eq!(margin.top, 1070 - 18 - 40);
    }

    #[test]
    fn a_pointer_in_the_corner_flips_on_both_axes() {
        let margin = place_beside(Point::new(1915.0, 1075.0), BAR, OUTPUT, OFFSET);
        assert_eq!((margin.left, margin.top), (1915 - 12 - 200, 1075 - 18 - 40));
    }

    /// A bar wider than its output cannot be placed politely; it must still land
    /// on screen rather than at a negative margin.
    #[test]
    fn clamps_when_the_bar_cannot_fit_either_side() {
        let huge = Size { width: 3000.0, height: 40.0 };
        let margin = place_beside(Point::new(10.0, 10.0), huge, OUTPUT, OFFSET);
        assert_eq!(margin.left, 0);
    }

    /// Before the bar reports its size the first margin is computed against a
    /// zero size, which must still be the pointer position plus the offset.
    #[test]
    fn a_zero_size_bar_lands_exactly_at_the_offset() {
        let margin = place_beside(Point::new(300.0, 400.0), Size::ZERO, OUTPUT, OFFSET);
        assert_eq!((margin.left, margin.top), (312, 418));
    }

    #[test]
    fn the_entrance_ease_starts_moving_and_comes_to_rest() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert!(ease_out_cubic(0.5) > 0.5, "ease-out front-loads the motion");
    }
}
