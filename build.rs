// SPDX-License-Identifier: GPL-3.0-only
//! Generates the desktop entry and the AppStream metainfo.
//!
//! Both carry the application's name, comment and keywords, and all three of
//! those are translatable. Rather than keeping parallel copies in sync by hand,
//! `xdgen` expands the templates in `resources/` against the Fluent catalogue in
//! `i18n/`, emitting one localized block per language present.

use std::path::Path;
use std::{env, fs};

use xdgen::{App, Context, FluentString};

fn main() {
    println!("cargo:rerun-if-changed=resources");
    println!("cargo:rerun-if-changed=i18n");

    let package = env::var("CARGO_PKG_NAME").expect("cargo always sets CARGO_PKG_NAME");
    let context = Context::new("i18n", package).expect("reading the i18n catalogue");

    let app = App::new(FluentString("app-title"))
        .comment(FluentString("app-comment"))
        .keywords(FluentString("app-keywords"));

    let desktop = app
        .expand_desktop("resources/app.desktop", &context)
        .expect("expanding resources/app.desktop");
    let metainfo = app
        .expand_metainfo("resources/app.metainfo.xml", &context)
        .expect("expanding resources/app.metainfo.xml");

    // Written next to the build artifacts rather than to a hardcoded `target/`,
    // so `CARGO_TARGET_DIR` and the justfile agree on where to find them.
    let target = env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_owned());
    let output = Path::new(&target).join("xdgen");
    fs::create_dir_all(&output).expect("creating the xdgen output directory");
    fs::write(output.join("app.desktop"), desktop).expect("writing the desktop entry");
    fs::write(output.join("app.metainfo.xml"), metainfo).expect("writing the metainfo");
}
