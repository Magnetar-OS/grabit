// SPDX-License-Identifier: GPL-3.0-only
//! Localization support.
//!
//! grabit has very little translatable chrome, and that is deliberate: every
//! label and tooltip in the popup comes from the user's own action manifests, so
//! it is their text, not ours. What lives here is the identity used to generate
//! the desktop entry and metainfo, plus the handful of strings grabit itself
//! puts in front of someone — failure notifications and `grabit doctor`.
//!
//! Log messages are not translated. They are read by whoever is debugging, and
//! a translated log line is harder to search for, not easier.

use i18n_embed::fluent::{FluentLanguageLoader, fluent_language_loader};
use i18n_embed::{DefaultLocalizer, LanguageLoader, Localizer, unic_langid::LanguageIdentifier};
use rust_embed::RustEmbed;
use std::sync::LazyLock;

#[derive(RustEmbed)]
#[folder = "i18n/"]
struct Localizations;

pub static LANGUAGE_LOADER: LazyLock<FluentLanguageLoader> = LazyLock::new(|| {
    let loader: FluentLanguageLoader = fluent_language_loader!();
    loader
        .load_fallback_language(&Localizations)
        .expect("the fallback language is embedded in the binary");
    loader
});

/// Select translations for the languages the desktop asked for.
pub fn init() {
    let requested: Vec<LanguageIdentifier> =
        i18n_embed::DesktopLanguageRequester::requested_languages();
    let localizer = DefaultLocalizer::new(&*LANGUAGE_LOADER, &Localizations);
    if let Err(e) = localizer.select(&requested) {
        // Not fatal: the fallback language is compiled in, so the program is
        // still fully usable in English.
        eprintln!("grabit: could not load translations: {e}");
    }

    // Fluent wraps interpolated values in Unicode bidi isolates by default.
    // Those are right in a rich text widget and show up as stray characters in a
    // terminal, which is where grabit's own strings mostly end up. This has to
    // come after `select`, which rebuilds the bundles.
    LANGUAGE_LOADER.set_use_isolating(false);
}

/// Look up a localized string by its Fluent identifier.
#[macro_export]
macro_rules! fl {
    ($message_id:literal) => {{
        i18n_embed_fl::fl!($crate::i18n::LANGUAGE_LOADER, $message_id)
    }};
    ($message_id:literal, $($args:expr),*) => {{
        i18n_embed_fl::fl!($crate::i18n::LANGUAGE_LOADER, $message_id, $($args),*)
    }};
}
