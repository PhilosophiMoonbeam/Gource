// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native application entry point for the Rust successor.

fn main() {
    if let Err(error) = gource_app::run() {
        eprintln!("gource-app: {error}");
        std::process::exit(1);
    }
}
