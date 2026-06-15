//! clawdpanel-ui -- compiled Slint HUD for the Rust/Slint rewrite (epic #47).
//!
//! S1 (#48): exposes the frameless bar window (`BarWindow`). Cascadia Mono is
//! embedded into the binary at compile time via a file-import in `bar.slint`
//! (the Slint Rust generator defaults to `EmbedAllResources`), so the HUD
//! renders identically wherever it ships with no runtime font lookup.
//!
//! S4 (#51): the `bar` module below ports the bar's display logic that lives in
//! Rust (not Slint markup): `normalize_separators` (the JS
//! `normalizeBarSeparators`), the `░▒▓█` meter split, the warn-threshold ladder,
//! and `fmt_msgs`. Pure + unit-tested; the app calls these to build
//! `ClaudeBarData` and the per-separator visibility the bar reads.

slint::include_modules!();

pub mod bar;

/// Hook the app calls once before showing any window.
///
/// In Slint >= 1.16 there is no global runtime font-registration entry point:
/// the embedded Cascadia Mono is registered automatically when the compiled
/// `BarWindow` is instantiated (see the `import "...ttf"` in `bar.slint`). This
/// stays as the single call-site seam for any future font wiring and documents
/// where font setup lives; today it is intentionally a no-op.
pub fn register_fonts() {}
