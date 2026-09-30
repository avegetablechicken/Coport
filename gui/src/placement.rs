//! Where the tray panel appears: centered under the tray icon (or above it
//! when the taskbar is at the bottom), kept inside the monitor. All values
//! are physical pixels in desktop coordinates.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
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
