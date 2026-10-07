//! Where the tray panel appears: centered under the tray icon (or above it
//! when the taskbar is at the bottom), kept inside the monitor. All values
//! share one unit in desktop coordinates: points on macOS, where screens of
//! different scales share one point space, and physical pixels elsewhere.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    #[cfg(any(target_os = "macos", test))]
    fn scaled(&self, factor: f64) -> Rect {
        Rect {
            x: self.x * factor,
            y: self.y * factor,
            w: self.w * factor,
            h: self.h * factor,
        }
    }

    pub fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

/// Gap between the icon and the panel, and the minimum distance to screen edges.
const GAP: f64 = 6.0;
const MARGIN: f64 = 8.0;
/// Without a usable icon position the panel sits below the top bar
/// (macOS menu bar incl. notch, GNOME top bar).
const TOP_BAR: f64 = 40.0;

/// The status item in points. macOS reports it in physical pixels of its own
/// screen, so on screens of mixed scale its physical position can fall inside
/// another screen's physical range. `screens` holds each screen in points with
/// its scale, primary first; the screen under `pointer` (in points) is the one
/// clicked, otherwise the first screen whose scale maps the item onto itself.
#[cfg(any(target_os = "macos", test))]
pub fn status_item_points(
    item: Rect,
    screens: &[(Rect, f64)],
    pointer: Option<(f64, f64)>,
) -> Rect {
    let fits = |(screen, scale): &&(Rect, f64)| {
        let (x, y) = item.scaled(1.0 / scale).center();
        screen.contains(x, y)
    };
    let under_pointer =
        |(screen, _): &&(Rect, f64)| pointer.is_some_and(|(x, y)| screen.contains(x, y));
    let scale = screens
        .iter()
        .filter(fits)
        .find(under_pointer)
        .or_else(|| screens.iter().find(fits))
        .map_or(1.0, |(_, scale)| *scale);
    item.scaled(1.0 / scale)
}

/// Returns the panel's top-left corner.
///
/// `anchor` is the tray icon's rectangle; it may be zero-sized when the tray
/// only reports a click point. It is ignored when it lies outside `monitor`,
/// as happens for a menu bar icon hidden behind the notch.
pub fn place(anchor: Option<Rect>, panel: (f64, f64), monitor: Rect, scale: f64) -> (f64, f64) {
    let (w, h) = panel;
    let (gap, margin) = (GAP * scale, MARGIN * scale);
    let anchor = anchor.filter(|a| {
        let (cx, cy) = a.center();
        monitor.contains(cx, cy)
    });
    let (x, y) = match anchor {
        Some(a) => {
            let (cx, cy) = a.center();
            let below = cy < monitor.y + monitor.h / 2.0;
            let y = if below {
                a.y + a.h + gap
            } else {
                a.y - h - gap
            };
            (cx - w / 2.0, y)
        }
        None => (
            monitor.x + monitor.w - w - margin,
            monitor.y + TOP_BAR * scale,
        ),
    };
    let max_x = (monitor.x + monitor.w - w - margin).max(monitor.x);
    let max_y = (monitor.y + monitor.h - h - margin).max(monitor.y);
    (
        x.clamp(monitor.x + margin, max_x.max(monitor.x + margin)),
        y.clamp(monitor.y, max_y),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 3000.0,
        h: 2000.0,
    };

    #[test]
    fn centers_below_a_menu_bar_icon() {
        let icon = Rect {
            x: 2000.0,
            y: 0.0,
            w: 56.0,
            h: 48.0,
        };
        let (x, y) = place(Some(icon), (760.0, 1200.0), SCREEN, 2.0);
        assert_eq!(x, 2028.0 - 380.0);
        assert_eq!(y, 48.0 + 12.0);
    }

    #[test]
    fn opens_above_a_bottom_taskbar_icon() {
        let icon = Rect {
            x: 1500.0,
            y: 1950.0,
            w: 40.0,
            h: 40.0,
        };
        let (_, y) = place(Some(icon), (380.0, 600.0), SCREEN, 1.0);
        assert_eq!(y, 1950.0 - 600.0 - 6.0);
    }

    #[test]
    fn stays_on_screen_near_the_right_edge() {
        let icon = Rect {
            x: 2980.0,
            y: 0.0,
            w: 20.0,
            h: 24.0,
        };
        let (x, _) = place(Some(icon), (380.0, 600.0), SCREEN, 1.0);
        assert_eq!(x, 3000.0 - 380.0 - 8.0);
    }

    #[test]
    fn hidden_icon_falls_back_to_top_right() {
        // tray-icon reports a menu bar item hidden by the notch at the screen's bottom edge.
        let hidden = Rect {
            x: 0.0,
            y: 2000.0,
            w: 56.0,
            h: 0.0,
        };
        let (x, y) = place(Some(hidden), (760.0, 1200.0), SCREEN, 2.0);
        assert_eq!((x, y), (3000.0 - 760.0 - 16.0, 80.0));
        assert_eq!(place(None, (760.0, 1200.0), SCREEN, 2.0), (x, y));
    }

    /// A 2x built-in display beside a 1x external one, in points.
    const BUILT_IN: (Rect, f64) = (
        Rect {
            x: 0.0,
            y: 0.0,
            w: 1512.0,
            h: 982.0,
        },
        2.0,
    );
    const EXTERNAL: (Rect, f64) = (
        Rect {
            x: 1512.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        },
        1.0,
    );

    #[test]
    fn status_items_on_mixed_scale_screens_are_found_in_points() {
        let screens = [BUILT_IN, EXTERNAL];
        // At 1300 points on the 2x display the item is at 2600 physical
        // pixels, inside the external display's physical range as well.
        let item = Rect {
            x: 2600.0,
            y: 0.0,
            w: 56.0,
            h: 48.0,
        };
        let on_built_in = Rect {
            x: 1300.0,
            y: 0.0,
            w: 28.0,
            h: 24.0,
        };
        assert_eq!(
            status_item_points(item, &screens, Some((1310.0, 10.0))),
            on_built_in
        );
        assert_eq!(status_item_points(item, &screens, None), on_built_in);
        // The same pixels clicked on the external display's menu bar.
        let on_external = status_item_points(item, &screens, Some((2610.0, 10.0)));
        assert_eq!(on_external, item);
        let (x, _) = place(Some(on_built_in), (360.0, 600.0), BUILT_IN.0, 1.0);
        assert!(BUILT_IN.0.contains(x, 0.0) && BUILT_IN.0.contains(x + 359.0, 0.0));
        let (x, _) = place(Some(on_external), (360.0, 600.0), EXTERNAL.0, 1.0);
        assert!(EXTERNAL.0.contains(x, 0.0));
    }

    #[test]
    fn a_single_screen_status_item_keeps_its_place() {
        let item = Rect {
            x: 2000.0,
            y: 0.0,
            w: 56.0,
            h: 48.0,
        };
        let points = status_item_points(item, &[BUILT_IN], None);
        assert_eq!(points.scaled(2.0), item);
        assert_eq!(status_item_points(item, &[], None), item);
    }

    #[test]
    fn click_point_without_size_is_a_valid_anchor() {
        // KDE's StatusNotifierItem reports only the click position.
        let point = Rect {
            x: 1000.0,
            y: 10.0,
            w: 0.0,
            h: 0.0,
        };
        assert_eq!(
            place(Some(point), (400.0, 600.0), SCREEN, 1.0),
            (800.0, 16.0)
        );
    }
}
