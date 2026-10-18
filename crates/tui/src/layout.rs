//! Responsive layout for the redesign surfaces.
//!
//! Three tiers, all with saturating arithmetic so any terminal size from
//! 1x1 upward renders without panics:
//!
//! - Wide (>=120x24): header, 45/55 Library/Queue split, four-row Now
//!   playing panel, keycap footer.
//! - Medium (80-119 cols, >=24 rows): one focused list at a time (Tab
//!   switches Library/Queue) plus a collapsed one-line player strip.
//! - Narrow (anything smaller): focused list, mini player, small-terminal
//!   hint, safely clipped header/footer.

use ratatui::layout::Rect;

pub const WIDE_MIN_WIDTH: u16 = 120;
pub const MEDIUM_MIN_WIDTH: u16 = 80;
pub const COMFY_MIN_HEIGHT: u16 = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Wide,
    Medium,
    Narrow,
}

pub fn tier(area: Rect) -> Tier {
    if area.width >= WIDE_MIN_WIDTH && area.height >= COMFY_MIN_HEIGHT {
        Tier::Wide
    } else if area.width >= MEDIUM_MIN_WIDTH && area.height >= COMFY_MIN_HEIGHT {
        Tier::Medium
    } else {
        Tier::Narrow
    }
}

/// Regions of the home surface (Library + Queue + Now playing).
#[derive(Debug, Clone, Copy)]
pub struct HomeRegions {
    pub tier: Tier,
    pub header: Rect,
    /// The Library list region (always present).
    pub library: Rect,
    /// The Queue region; zero-sized when the tier shows a single list.
    pub queue: Rect,
    /// True when Library and Queue are side by side.
    pub queue_visible: bool,
    pub player: Rect,
    pub footer: Rect,
}

/// Take `height` rows at `*y`, saturating at the bottom of `area`.
fn take_rows(area: Rect, y: &mut u16, height: u16) -> Rect {
    let bottom = area.y.saturating_add(area.height);
    let top = (*y).min(bottom);
    let rows = height.min(bottom.saturating_sub(top));
    *y = top.saturating_add(rows);
    Rect {
        x: area.x,
        y: top,
        width: area.width,
        height: rows,
    }
}

/// Split the remaining rows between `y` and the reserved tail height.
fn rest(area: Rect, y: u16, reserved: u16) -> Rect {
    let bottom = area.y.saturating_add(area.height);
    let top = y.min(bottom);
    let rows = bottom
        .saturating_sub(top)
        .saturating_sub(reserved);
    Rect {
        x: area.x,
        y: top,
        width: area.width,
        height: rows,
    }
}

/// Library/Queue/Now playing layout for the current tier.
pub fn home(area: Rect) -> HomeRegions {
    let tier = tier(area);
    let mut y = area.y;
    let header = take_rows(area, &mut y, 1);
    match tier {
        Tier::Wide => {
            let footer_h = 1;
            let player_h = 6; // border + four content rows
            let main = rest(area, y, player_h + footer_h);
            let player_top = main.y.saturating_add(main.height);
            let player = Rect {
                x: area.x,
                y: player_top,
                width: area.width,
                height: player_h.min(area.y.saturating_add(area.height).saturating_sub(player_top)),
            };
            let footer_top = player.y.saturating_add(player.height);
            let footer = Rect {
                x: area.x,
                y: footer_top,
                width: area.width,
                height: area
                    .y
                    .saturating_add(area.height)
                    .saturating_sub(footer_top),
            };
            let left_width = main.width * 45 / 100;
            let library = Rect {
                width: left_width,
                ..main
            };
            let queue = Rect {
                x: main.x.saturating_add(left_width),
                width: main.width.saturating_sub(left_width),
                ..main
            };
            HomeRegions {
                tier,
                header,
                library,
                queue,
                queue_visible: true,
                player,
                footer,
            }
        }
        Tier::Medium => {
            let player_h = 1;
            let main = rest(area, y, player_h + 1);
            let player_top = main.y.saturating_add(main.height);
            let player = Rect {
                x: area.x,
                y: player_top,
                width: area.width,
                height: player_h.min(area.y.saturating_add(area.height).saturating_sub(player_top)),
            };
            let footer_top = player.y.saturating_add(player.height);
            let footer = Rect {
                x: area.x,
                y: footer_top,
                width: area.width,
                height: area
                    .y
                    .saturating_add(area.height)
                    .saturating_sub(footer_top),
            };
            HomeRegions {
                tier,
                header,
                library: main,
                queue: Rect { height: 0, ..main },
                queue_visible: false,
                player,
                footer,
            }
        }
        Tier::Narrow => {
            let player_h = 1;
            let main = rest(area, y, player_h + 1);
            let player_top = main.y.saturating_add(main.height);
            let player = Rect {
                x: area.x,
                y: player_top,
                width: area.width,
                height: player_h.min(area.y.saturating_add(area.height).saturating_sub(player_top)),
            };
            let footer_top = player.y.saturating_add(player.height);
            let footer = Rect {
                x: area.x,
                y: footer_top,
                width: area.width,
                height: area
                    .y
                    .saturating_add(area.height)
                    .saturating_sub(footer_top),
            };
            HomeRegions {
                tier,
                header,
                library: main,
                queue: Rect { height: 0, ..main },
                queue_visible: false,
                player,
                footer,
            }
        }
    }
}

/// A centered overlay region: about 65% of the viewport width, clamped to
/// the viewport, vertically centered on `height` rows (clamped).
pub fn centered_overlay(area: Rect, height: u16) -> Rect {
    let width = (area.width * 65 / 100).max(20).min(area.width);
    let height = height.min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Split a region into left/right percentage columns; the right column
/// gets the remainder so widths always sum to the region width.
pub fn split_columns(area: Rect, left_percent: u16) -> (Rect, Rect) {
    let left = area.width * left_percent.min(100) / 100;
    (
        Rect {
            width: left,
            ..area
        },
        Rect {
            x: area.x.saturating_add(left),
            width: area.width.saturating_sub(left),
            ..area
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fits(area: Rect, region: Rect) {
        assert!(region.x >= area.x && region.y >= area.y, "{region:?} in {area:?}");
        assert!(region.x + region.width <= area.x + area.width, "{region:?} in {area:?}");
        assert!(region.y + region.height <= area.y + area.height, "{region:?} in {area:?}");
    }

    #[test]
    fn tiers_classify_by_size() {
        assert_eq!(tier(Rect::new(0, 0, 120, 24)), Tier::Wide);
        assert_eq!(tier(Rect::new(0, 0, 200, 40)), Tier::Wide);
        assert_eq!(tier(Rect::new(0, 0, 119, 24)), Tier::Medium);
        assert_eq!(tier(Rect::new(0, 0, 80, 24)), Tier::Medium);
        assert_eq!(tier(Rect::new(0, 0, 79, 24)), Tier::Narrow);
        assert_eq!(tier(Rect::new(0, 0, 200, 23)), Tier::Narrow);
        assert_eq!(tier(Rect::new(0, 0, 1, 1)), Tier::Narrow);
    }

    #[test]
    fn wide_splits_library_and_queue_45_55() {
        let area = Rect::new(0, 0, 140, 40);
        let regions = home(area);
        assert!(regions.queue_visible);
        assert_eq!(regions.library.width, 63);
        assert_eq!(regions.queue.width, 77);
        assert_eq!(regions.queue.x, 63);
        for region in [
            regions.header,
            regions.library,
            regions.queue,
            regions.player,
            regions.footer,
        ] {
            fits(area, region);
        }
    }

    #[test]
    fn medium_and_narrow_show_one_list() {
        for (w, h) in [(80, 24), (119, 30), (79, 23), (40, 12)] {
            let area = Rect::new(0, 0, w, h);
            let regions = home(area);
            assert!(!regions.queue_visible, "{w}x{h}");
            assert_eq!(regions.queue.height, 0);
            for region in [regions.header, regions.library, regions.player, regions.footer] {
                fits(area, region);
            }
        }
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        for (w, h) in [(1, 1), (2, 2), (10, 3), (0, 0)] {
            let area = Rect::new(0, 0, w, h);
            let regions = home(area);
            for region in [
                regions.header,
                regions.library,
                regions.queue,
                regions.player,
                regions.footer,
            ] {
                fits(area, region);
            }
            let overlay = centered_overlay(area, 10);
            fits(area, overlay);
        }
    }

    #[test]
    fn overlay_centers_and_clamps() {
        let area = Rect::new(0, 0, 140, 40);
        let overlay = centered_overlay(area, 20);
        assert_eq!(overlay.width, 91);
        assert_eq!(overlay.height, 20);
        assert_eq!(overlay.x, (140 - 91) / 2);
        assert_eq!(overlay.y, 10);
        // Height beyond the viewport clamps.
        let tall = centered_overlay(area, 100);
        assert_eq!(tall.height, 40);
        assert_eq!(tall.y, 0);
    }

    #[test]
    fn split_columns_sums_to_width() {
        let (left, right) = split_columns(Rect::new(3, 2, 101, 10), 45);
        assert_eq!(left.width + right.width, 101);
        assert_eq!(right.x, 3 + left.width);
        let (left, right) = split_columns(Rect::new(0, 0, 0, 5), 45);
        assert_eq!(left.width + right.width, 0);
    }
}
