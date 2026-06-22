//! Compile the Slint UI into Rust at build time. `slint::include_modules!()` in
//! `lib.rs` then pulls in the generated `BarWindow` component.

fn main() {
    slint_build::compile("ui/app.slint").expect("compiling ui/app.slint");
}
