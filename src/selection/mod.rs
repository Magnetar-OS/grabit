// SPDX-License-Identifier: GPL-3.0-only
//! Selection sources and the debounce that turns them into a single event.

pub mod data_control;

use std::time::Duration;

use futures::Stream;

use crate::config::Selection as SelectionConfig;

/// One captured selection: the text, plus the source's HTML rendering of it
/// when one was offered. Browsers and rich editors offer `text/html` alongside
/// the plain text; the `{{html}}` and `{{markdown}}` placeholders come from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grab {
    pub text: String,
    pub html: Option<String>,
}

impl Grab {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), html: None }
    }
}

/// An unfiltered selection event straight off a backend.
#[derive(Debug, Clone)]
pub enum Raw {
    Selection {
        /// Monotonic per-backend counter. Transfers are read on worker threads
        /// and can therefore complete out of order; a lower generation than one
        /// already seen is stale and must be dropped.
        generation: u64,
        grab: Grab,
    },
    /// The primary selection was cleared or holds nothing we can read.
    Cleared,
}

/// A selection that has stopped changing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// Act on this selection.
    Text(Grab),
    /// Take any visible popup down.
    Cleared,
}

/// Whether a selection is worth popping up for.
pub fn is_interesting(text: &str, cfg: &SelectionConfig) -> bool {
    let candidate = text.trim();
    if cfg.ignore_whitespace_only && candidate.is_empty() {
        return false;
    }
    let len = candidate.chars().count();
    len >= cfg.min_length && len <= cfg.max_length
}

/// Turn a raw event stream into one event per settled selection.
///
/// Dragging a selection rewrites the primary selection on every pointer motion,
/// so reacting to raw events would flash the bar through every intermediate
/// state. Waiting for quiescence is what makes the popup feel like it appears
/// when you *finish* selecting.
///
/// Executor-agnostic on purpose: both front-ends consume this, and they run on
/// different runtimes.
pub fn settled(
    rx: async_channel::Receiver<Raw>,
    cfg: SelectionConfig,
    settle: Duration,
) -> impl Stream<Item = Settled> + Send + 'static {
    struct State {
        rx: async_channel::Receiver<Raw>,
        cfg: SelectionConfig,
        settle: Duration,
        /// Highest generation seen, so a slow transfer belonging to an older
        /// selection cannot overwrite a newer one.
        newest: u64,
    }

    futures::stream::unfold(State { rx, cfg, settle, newest: 0 }, |mut state| async move {
        let first = state.rx.recv().await.ok()?;
        let mut pending = accept(first, &mut state.newest).unwrap_or_default();

        // Keep swallowing events until the stream goes quiet for `settle`.
        loop {
            match tokio::time::timeout(state.settle, state.rx.recv()).await {
                Err(_timed_out) => break,
                Ok(Err(_closed)) => return None,
                Ok(Ok(raw)) => pending = accept(raw, &mut state.newest).unwrap_or_default(),
            }
        }

        let event = if is_interesting(&pending.text, &state.cfg) {
            Settled::Text(pending)
        } else {
            if !pending.text.is_empty() {
                log::debug!("selection filtered out by the [selection] limits");
            }
            Settled::Cleared
        };
        Some((event, state))
    })
}

/// Fold one raw event into the newest-generation bookkeeping.
///
/// Returns the selection to adopt, or `None` for "cleared / stale".
fn accept(raw: Raw, newest: &mut u64) -> Option<Grab> {
    match raw {
        Raw::Cleared => None,
        Raw::Selection { generation, grab } => {
            if generation < *newest {
                log::debug!("dropping stale selection (gen {generation})");
                return None;
            }
            *newest = generation;
            Some(grab)
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    fn cfg() -> SelectionConfig {
        SelectionConfig { min_length: 1, max_length: 10, ignore_whitespace_only: true }
    }

    #[test]
    fn whitespace_only_is_uninteresting() {
        assert!(!is_interesting("   \n ", &cfg()));
    }

    #[test]
    fn length_bounds_are_measured_in_characters_after_trimming() {
        assert!(is_interesting("  héllo  ", &cfg()));
        assert!(!is_interesting("way too long here", &cfg()));
    }

    #[test]
    fn stale_generations_are_dropped() {
        let mut newest = 0;
        assert_eq!(
            accept(Raw::Selection { generation: 5, grab: Grab::text("new") }, &mut newest),
            Some(Grab::text("new"))
        );
        assert_eq!(
            accept(Raw::Selection { generation: 4, grab: Grab::text("old") }, &mut newest),
            None
        );
        assert_eq!(newest, 5);
    }

    /// A drag produces a burst of selections; only the last one should surface.
    #[test]
    fn a_burst_collapses_to_its_final_value() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("building a test runtime");

        runtime.block_on(async {
            let (tx, rx) = async_channel::unbounded();
            for (generation, text) in [(1, "h"), (2, "he"), (3, "hello")] {
                tx.send(Raw::Selection { generation, grab: Grab::text(text) }).await.unwrap();
            }
            let mut stream = Box::pin(settled(rx, cfg(), Duration::from_millis(20)));
            assert_eq!(stream.next().await, Some(Settled::Text(Grab::text("hello"))));
        });
    }

    #[test]
    fn clearing_reports_cleared() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("building a test runtime");

        runtime.block_on(async {
            let (tx, rx) = async_channel::unbounded();
            tx.send(Raw::Cleared).await.unwrap();
            let mut stream = Box::pin(settled(rx, cfg(), Duration::from_millis(20)));
            assert_eq!(stream.next().await, Some(Settled::Cleared));
        });
    }
}
