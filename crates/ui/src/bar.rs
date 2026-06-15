//! Bar display logic that lives in Rust rather than Slint markup — ported 1:1
//! from `frontend/src/main.js`. The app feeds these into the `ClaudeBar` Slint
//! globals; nothing here touches Slint types, so it is plain, testable Rust.

/// Cells in the `░▒▓█` usage meter (`BAR_CHARS` in main.js).
pub const BAR_CHARS: usize = 9;

/// Three-level warn ladder shared by the weekly and 5H segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Warn {
    None,
    Medium,
    High,
}

/// Fill character by percent (`renderProgress`, main.js:20-32):
/// `<25% ░`, `25–55% ▒`, `55–85% ▓`, `≥85% █`.
pub fn ramp_char(pct: f64) -> char {
    if pct >= 0.85 {
        '█'
    } else if pct >= 0.55 {
        '▓'
    } else if pct >= 0.25 {
        '▒'
    } else {
        '░'
    }
}

/// Splits the 9-cell meter into its filled run and its `·` padding run, the way
/// the CSS paints `.prog-fill` (accent) and `.prog-empty` (muted) separately.
pub fn meter_parts(pct: f64) -> (String, String) {
    let filled = ((pct * BAR_CHARS as f64).round() as i64).clamp(0, BAR_CHARS as i64) as usize;
    let empty = BAR_CHARS - filled;
    let c = ramp_char(pct);
    (
        std::iter::repeat(c).take(filled).collect(),
        "·".repeat(empty),
    )
}

/// `fmtMsgs`: `90543 → "90.5K"`, `1000 → "1K"`, `150 → "150"`.
pub fn fmt_msgs(n: i64) -> String {
    if n >= 1000 {
        let s = format!("{:.1}", n as f64 / 1000.0);
        let s = s.strip_suffix(".0").map(str::to_string).unwrap_or(s);
        format!("{s}K")
    } else {
        n.to_string()
    }
}

/// Weekly warn level (main.js:124-126): high on limit-exceeded or ≥95%, medium
/// on ≥85% (only when a percent is actually shown, i.e. a limit is configured).
pub fn weekly_warn(has_limit: bool, pct: f64, limit_exceeded: bool) -> Warn {
    if limit_exceeded || (has_limit && pct >= 0.95) {
        Warn::High
    } else if has_limit && pct >= 0.85 {
        Warn::Medium
    } else {
        Warn::None
    }
}

/// 5H warn level (main.js:153-154): high ≥95%, medium ≥85%.
pub fn hourly_warn(pct: f64) -> Warn {
    if pct >= 0.95 {
        Warn::High
    } else if pct >= 0.85 {
        Warn::Medium
    } else {
        Warn::None
    }
}

/// One slot in the bar's content row, in DOM order. `Seg(false)` is a hidden
/// segment; `Spacer` is the flex pusher (skipped, like `.spacer` in JS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Seg(bool),
    Sep,
    Spacer,
}

/// Port of `normalizeBarSeparators` (main.js:41-57): a separator shows only when
/// a visible segment precedes it and none has been shown since — collapsing runs
/// of hidden segments + their separators to a single divider. Returns one bool
/// per `Sep` in order.
pub fn normalize_separators(items: &[Item]) -> Vec<bool> {
    let mut seen_seg = false;
    let mut gap_has_sep = false;
    let mut out = Vec::new();
    for it in items {
        match it {
            Item::Spacer => continue,
            Item::Sep => {
                let show = seen_seg && !gap_has_sep;
                out.push(show);
                if show {
                    gap_has_sep = true;
                }
            }
            Item::Seg(visible) => {
                if *visible {
                    seen_seg = true;
                    gap_has_sep = false;
                }
            }
        }
    }
    out
}

/// Builds the bar's content row in DOM order and returns the 9 separator
/// visibilities the `ClaudeBar.sep-visible` array consumes. Segment visibility
/// follows the feature flags (weekly/radio/monitor/theme) and the data-gated 5H
/// pair, exactly as `applyFeatureVisibility` + `refresh` set `style.display`.
#[allow(clippy::too_many_arguments)]
pub fn bar_separators(
    weekly: bool,
    hourly: bool,
    radio: bool,
    monitor: bool,
    theme: bool,
) -> Vec<bool> {
    use Item::*;
    let items = [
        Seg(true),    // account (always)
        Sep,          // 0: acct | weekly
        Seg(weekly),  // weekly
        Sep,          // 1: weekly | reset
        Seg(weekly),  // weekly reset
        Sep,          // 2: reset | 5h
        Seg(hourly),  // 5h
        Sep,          // 3: 5h | 5h-reset
        Seg(hourly),  // 5h reset
        Sep,          // 4: 5h-reset | model
        Seg(true),    // model (always)
        Sep,          // 5: model | status
        Seg(true),    // status (always)
        Spacer,
        Seg(radio),   // radio
        Sep,          // 6: radio | monitor
        Seg(monitor), // monitor
        Sep,          // 7: monitor | theme
        Seg(theme),   // theme
        Sep,          // 8: theme | pin
        Seg(true),    // pin (always)
    ];
    normalize_separators(&items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramp_char_thresholds() {
        assert_eq!(ramp_char(0.0), '░');
        assert_eq!(ramp_char(0.24), '░');
        assert_eq!(ramp_char(0.25), '▒');
        assert_eq!(ramp_char(0.54), '▒');
        assert_eq!(ramp_char(0.55), '▓');
        assert_eq!(ramp_char(0.84), '▓');
        assert_eq!(ramp_char(0.85), '█');
        assert_eq!(ramp_char(1.0), '█');
    }

    #[test]
    fn meter_parts_split_and_length() {
        // 50% → round(4.5) = 5 filled (banker's? Rust rounds half away → 5), 4 empty.
        let (fill, empty) = meter_parts(0.5);
        assert_eq!(fill.chars().count() + empty.chars().count(), BAR_CHARS);
        assert!(fill.chars().all(|c| c == '▒'));
        assert!(empty.chars().all(|c| c == '·'));

        let (f0, e0) = meter_parts(0.0);
        assert_eq!(f0, "");
        assert_eq!(e0, "·".repeat(9));

        let (f1, e1) = meter_parts(1.0);
        assert_eq!(f1, "█".repeat(9));
        assert_eq!(e1, "");
    }

    #[test]
    fn fmt_msgs_matches_js() {
        assert_eq!(fmt_msgs(150), "150");
        assert_eq!(fmt_msgs(999), "999");
        assert_eq!(fmt_msgs(1000), "1K");
        assert_eq!(fmt_msgs(1234), "1.2K");
        assert_eq!(fmt_msgs(90543), "90.5K");
        assert_eq!(fmt_msgs(9999), "10K");
    }

    #[test]
    fn warn_ladders() {
        // No limit configured → never warns even at high pct.
        assert_eq!(weekly_warn(false, 0.99, false), Warn::None);
        assert_eq!(weekly_warn(true, 0.80, false), Warn::None);
        assert_eq!(weekly_warn(true, 0.85, false), Warn::Medium);
        assert_eq!(weekly_warn(true, 0.94, false), Warn::Medium);
        assert_eq!(weekly_warn(true, 0.95, false), Warn::High);
        // Limit exceeded forces high regardless of pct / configured limit.
        assert_eq!(weekly_warn(false, 0.0, true), Warn::High);

        assert_eq!(hourly_warn(0.84), Warn::None);
        assert_eq!(hourly_warn(0.85), Warn::Medium);
        assert_eq!(hourly_warn(0.95), Warn::High);
    }

    #[test]
    fn separators_between_two_visible() {
        assert_eq!(
            normalize_separators(&[Item::Seg(true), Item::Sep, Item::Seg(true)]),
            vec![true]
        );
    }

    #[test]
    fn separators_collapse_around_hidden_middle() {
        // acct · [hidden] · model → only the first sep shows.
        assert_eq!(
            normalize_separators(&[
                Item::Seg(true),
                Item::Sep,
                Item::Seg(false),
                Item::Sep,
                Item::Seg(true),
            ]),
            vec![true, false]
        );
    }

    #[test]
    fn separators_drop_leading() {
        assert_eq!(
            normalize_separators(&[Item::Seg(false), Item::Sep, Item::Seg(true)]),
            vec![false]
        );
    }

    #[test]
    fn separators_skip_spacer_without_resetting() {
        // The spacer carries the run state across (no sep at the spacer itself).
        assert_eq!(
            normalize_separators(&[
                Item::Seg(true),
                Item::Spacer,
                Item::Sep,
                Item::Seg(true),
            ]),
            vec![true]
        );
    }

    #[test]
    fn bar_separators_all_visible() {
        // Every gap between consecutive visible segments gets exactly one "·".
        let v = bar_separators(true, true, true, true, true);
        assert_eq!(v.len(), 9);
        assert!(v.iter().all(|&s| s), "all 9 separators shown when nothing is hidden");
    }

    #[test]
    fn bar_separators_hourly_hidden_collapses() {
        // 5H pair hidden: the reset|5h, 5h|5h-reset, 5h-reset|model gap collapses
        // to a single divider (sep[2] shown, sep[3] + sep[4] hidden).
        let v = bar_separators(true, false, true, true, true);
        assert_eq!(v, vec![true, true, true, false, false, true, true, true, true]);
    }

    #[test]
    fn bar_separators_radio_hidden() {
        // Radio off: the spacer carries state from status; the next sep (radio|mon)
        // still shows because status (visible) preceded it across the spacer.
        let v = bar_separators(true, true, false, true, true);
        assert_eq!(v.len(), 9);
        assert!(v[6], "radio|mon sep still divides status from monitor");
    }
}
