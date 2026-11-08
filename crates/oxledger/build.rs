// SPDX-License-Identifier: GPL-2.0-or-later
// `sqlx::migrate!` embeds the migrations at compile time. Without this, adding a
// new file under migrations/ does not rebuild the crate and the binary (and the
// tests) keep running the old set.
fn main() {
    println!("cargo:rerun-if-changed=../../migrations");
}
