//! [`TrayControl`]: injects pointer interactions into embedded icon windows.
//!
//! This runs on its own X11 connection so it can be used from async tasks
//! (the SNI side) without touching the blocking host event loop. Interactions
//! are delivered as synthetic `ButtonPress`/`ButtonRelease` events sent
//! directly to the icon window — the least-invasive approach that doesn't warp
//! the user's real pointer.

use std::sync::Arc;
use std::time::Duration;

use tracing::debug;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ButtonPressEvent, ConfigureWindowAux, ConnectionExt as _, EventMask,
};
use x11rb::rust_connection::RustConnection;

use crate::{IconId, Result};

/// How long an icon stays slid under the interaction point after a click, so
/// the app has time to ask the server where its icon is before placing a menu.
const PARK_DURATION: Duration = Duration::from_millis(400);

/// X11 button numbers.
const BUTTON_LEFT: u8 = 1;
const BUTTON_MIDDLE: u8 = 2;
const BUTTON_RIGHT: u8 = 3;
const BUTTON_SCROLL_UP: u8 = 4;
const BUTTON_SCROLL_DOWN: u8 = 5;
const BUTTON_SCROLL_LEFT: u8 = 6;
const BUTTON_SCROLL_RIGHT: u8 = 7;

/// Where to put an app's menu when the SNI host sends no interaction
/// coordinates (Quickshell's `Activate(0, 0)`, for one): the icon is parked in
/// that corner of the screen for the click, so the menu opens there.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MenuAnchor {
    #[default]
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

impl std::str::FromStr for MenuAnchor {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "top-right" => Ok(Self::TopRight),
            "top-left" => Ok(Self::TopLeft),
            "bottom-right" => Ok(Self::BottomRight),
            "bottom-left" => Ok(Self::BottomLeft),
            other => Err(format!(
                "unknown menu anchor: {other} (expected top-right, top-left, bottom-right or bottom-left)"
            )),
        }
    }
}

/// A cheaply-cloneable handle for delivering interactions to embedded icons.
#[derive(Clone)]
pub struct TrayControl {
    conn: Arc<RustConnection>,
    root: u32,
    screen_size: (u16, u16),
    /// `_NET_WORKAREA`: the screen minus panels, published by EWMH-aware WMs
    /// (Hyprland's Xwayland does, with the bar's strip excluded).
    workarea_atom: u32,
    menu_anchor: MenuAnchor,
}

impl std::fmt::Debug for TrayControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrayControl").finish_non_exhaustive()
    }
}

impl TrayControl {
    /// Open a fresh connection to the same display for injecting events.
    pub fn connect() -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None)?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let screen_size = (screen.width_in_pixels, screen.height_in_pixels);
        let workarea_atom = conn.intern_atom(false, b"_NET_WORKAREA")?.reply()?.atom;
        Ok(Self {
            conn: Arc::new(conn),
            root,
            screen_size,
            workarea_atom,
            menu_anchor: MenuAnchor::default(),
        })
    }

    /// Choose where menus open for interactions that carry no coordinates.
    pub fn with_menu_anchor(mut self, anchor: MenuAnchor) -> Self {
        self.menu_anchor = anchor;
        self
    }

    /// Primary action (left click). `x`/`y` are the host-provided root
    /// coordinates of the interaction (where a resulting menu should appear).
    pub fn activate(&self, icon: IconId, x: i32, y: i32) -> Result<()> {
        self.click(icon, BUTTON_LEFT, x, y)
    }

    /// Secondary action (middle click).
    pub fn secondary_activate(&self, icon: IconId, x: i32, y: i32) -> Result<()> {
        self.click(icon, BUTTON_MIDDLE, x, y)
    }

    /// Context menu (right click) — most legacy tray icons pop their own menu.
    pub fn context_menu(&self, icon: IconId, x: i32, y: i32) -> Result<()> {
        self.click(icon, BUTTON_RIGHT, x, y)
    }

    /// Scroll by `delta` steps along the given axis (positive = up/right).
    pub fn scroll(&self, icon: IconId, delta: i32, horizontal: bool) -> Result<()> {
        if delta == 0 {
            return Ok(());
        }
        let button = match (horizontal, delta > 0) {
            (false, true) => BUTTON_SCROLL_UP,
            (false, false) => BUTTON_SCROLL_DOWN,
            (true, true) => BUTTON_SCROLL_RIGHT,
            (true, false) => BUTTON_SCROLL_LEFT,
        };
        for _ in 0..delta.unsigned_abs().min(10) {
            self.click(icon, button, 0, 0)?;
        }
        Ok(())
    }

    /// Send a press+release of `button` on the icon window. `root_x`/`root_y`
    /// are the interaction's screen coordinates (from the SNI host) so the app
    /// can place any resulting menu correctly; if zero we fall back to the
    /// window centre.
    fn click(&self, icon: IconId, button: u8, root_x: i32, root_y: i32) -> Result<()> {
        // Ask the server for the icon's current size so we click its centre.
        let (w, h) = match self.conn.get_geometry(icon)?.reply() {
            Ok(geo) => (geo.width, geo.height),
            Err(_) => (1, 1), // window may have vanished; harmless coordinates
        };
        let (ex, ey) = ((w / 2) as i16, (h / 2) as i16);
        // Hosts that know where the click happened tell us; others (Quickshell
        // sends `Activate(0, 0)`) get the configured screen corner, which is
        // where a bar's tray usually lives.
        let hinted = root_x != 0 || root_y != 0;
        let (rx, ry) = if hinted {
            // A click on a bar lands inside the bar's strip, and a menu that
            // opens there is drawn under the bar (layer surfaces sit above X11
            // popups on Wayland compositors). Keep the point in the work area.
            self.clamp_to_work_area(root_x, root_y)
        } else {
            self.anchor_point(w, h)
        };

        // Apps place any resulting menu at the pointer, not at our event's
        // coordinates. Warp the X pointer to the interaction point so a menu
        // appears by the tray. (On Wayland this is best-effort — Xwayland may
        // ignore it when the real pointer is over a native Wayland surface.)
        if hinted {
            let _ = self
                .conn
                .warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, rx, ry);
            let _ = self.conn.flush();
        }

        // Some toolkits ignore the event's root coordinates and ask the server
        // where the icon window is before placing a menu (Java AWT's
        // XTrayIconPeer translates the icon's origin to root coordinates; GTK
        // checks the pointer is over the icon). The icon sits far offscreen
        // inside its container, so those menus would open off screen. Moving
        // the container on screen would make the compositor map it as a window
        // of its own (Hyprland then shows it and counts it on a workspace), so
        // slide the icon *within* the container instead, so that its root
        // coordinates land on the interaction point. The container clips it,
        // nothing shows, and the compositor never hears of it. It slides back
        // once the app has had time to read the position.
        let slid = self.slide_icon_to(
            icon,
            i32::from(rx) - i32::from(ex),
            i32::from(ry) - i32::from(ey),
        );

        // Toolkits select button events on the widget that draws the icon, not
        // on the toplevel they docked (Java AWT's tray canvas is a child window
        // of its embedded frame). A `send_event` only propagates *up* the tree,
        // so deliver to the deepest window under the click point instead.
        let (target, ex, ey) = self.deepest_window_at(icon, ex, ey);

        let press = ButtonPressEvent {
            response_type: x11rb::protocol::xproto::BUTTON_PRESS_EVENT,
            detail: button,
            sequence: 0,
            time: x11rb::CURRENT_TIME,
            root: self.root,
            event: target,
            child: x11rb::NONE,
            root_x: rx,
            root_y: ry,
            event_x: ex,
            event_y: ey,
            state: 0u16.into(),
            same_screen: true,
        };
        let mut release = press;
        release.response_type = x11rb::protocol::xproto::BUTTON_RELEASE_EVENT;
        // Reflect the held button in the release event's modifier state.
        let held: u16 = match button {
            BUTTON_LEFT => 0x100,
            BUTTON_MIDDLE => 0x200,
            BUTTON_RIGHT => 0x400,
            _ => 0,
        };
        release.state = held.into();

        let mask = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE;
        self.conn.send_event(true, target, mask, press)?;
        self.conn.send_event(true, target, mask, release)?;
        self.conn.flush()?;
        debug!(
            icon = format_args!("{icon:#010x}"),
            target = format_args!("{target:#010x}"),
            button,
            "sent click"
        );

        if slid {
            let conn = Arc::clone(&self.conn);
            std::thread::spawn(move || {
                std::thread::sleep(PARK_DURATION);
                let _ = conn.configure_window(icon, &ConfigureWindowAux::new().x(0).y(0));
                let _ = conn.flush();
            });
        }
        Ok(())
    }

    /// Move `icon` inside its container so that the icon's origin lands on
    /// the given root coordinates. Returns false when the icon has no
    /// container of ours or the server refused.
    fn slide_icon_to(&self, icon: IconId, root_x: i32, root_y: i32) -> bool {
        let Some(container) = self.container_of(icon) else {
            return false;
        };
        let Ok(cookie) = self.conn.get_geometry(container) else {
            return false;
        };
        let Ok(geometry) = cookie.reply() else {
            return false;
        };
        let aux = ConfigureWindowAux::new()
            .x(root_x - i32::from(geometry.x))
            .y(root_y - i32::from(geometry.y));
        self.conn.configure_window(icon, &aux).is_ok() && self.conn.flush().is_ok()
    }

    /// The deepest viewable descendant of `window` containing (`x`, `y`), with
    /// the point translated into that descendant's coordinates. Falls back to
    /// `window` itself when it has no child there.
    fn deepest_window_at(&self, window: u32, x: i16, y: i16) -> (u32, i16, i16) {
        let (mut target, mut tx, mut ty) = (window, x, y);
        for _ in 0..16 {
            let Ok(cookie) = self.conn.translate_coordinates(window, target, x, y) else {
                break;
            };
            let Ok(reply) = cookie.reply() else {
                break;
            };
            if reply.child == x11rb::NONE {
                break;
            }
            target = reply.child;
            // Point relative to the new target: translate from the icon once more.
            let Ok(cookie) = self.conn.translate_coordinates(window, target, x, y) else {
                break;
            };
            let Ok(inner) = cookie.reply() else {
                break;
            };
            tx = inner.dst_x;
            ty = inner.dst_y;
        }
        (target, tx, ty)
    }

    /// Root coordinates of the icon's centre once parked in the configured
    /// screen corner (used when the host gives no interaction coordinates).
    fn anchor_point(&self, w: u16, h: u16) -> (i16, i16) {
        let (ax, ay, aw, ah) = self.work_area();
        let (w, h) = (i32::from(w), i32::from(h));
        let (x, y) = match self.menu_anchor {
            MenuAnchor::TopRight => (ax + aw - w / 2, ay + h / 2),
            MenuAnchor::TopLeft => (ax + w / 2, ay + h / 2),
            MenuAnchor::BottomRight => (ax + aw - w / 2, ay + ah - h / 2),
            MenuAnchor::BottomLeft => (ax + w / 2, ay + ah - h / 2),
        };
        let limit = i32::from(i16::MAX);
        (x.clamp(0, limit) as i16, y.clamp(0, limit) as i16)
    }

    /// The usable screen in root coordinates: `_NET_WORKAREA` of the first
    /// desktop when the WM publishes it, else the whole screen.
    fn work_area(&self) -> (i32, i32, i32, i32) {
        let whole = (
            0,
            0,
            i32::from(self.screen_size.0),
            i32::from(self.screen_size.1),
        );
        let Ok(cookie) = self.conn.get_property(
            false,
            self.root,
            self.workarea_atom,
            AtomEnum::CARDINAL,
            0,
            4,
        ) else {
            return whole;
        };
        let Ok(reply) = cookie.reply() else {
            return whole;
        };
        let Some(values) = reply.value32() else {
            return whole;
        };
        let values: Vec<i32> = values
            .take(4)
            .map(|v| v.min(i32::MAX as u32) as i32)
            .collect();
        match values[..] {
            [x, y, w, h] if w > 0 && h > 0 => (x, y, w, h),
            _ => whole,
        }
    }

    /// Pull an interaction point into the work area, so a menu that opens at
    /// the point isn't drawn under a panel.
    fn clamp_to_work_area(&self, x: i32, y: i32) -> (i16, i16) {
        let (ax, ay, aw, ah) = self.work_area();
        let x = x.clamp(ax, (ax + aw - 1).max(ax));
        let y = y.clamp(ay, (ay + ah - 1).max(ay));
        let limit = i32::from(i16::MAX);
        (x.clamp(0, limit) as i16, y.clamp(0, limit) as i16)
    }

    /// The offscreen container the host reparented `icon` into, if any.
    fn container_of(&self, icon: IconId) -> Option<u32> {
        let tree = self.conn.query_tree(icon).ok()?.reply().ok()?;
        (tree.parent != self.root && tree.parent != x11rb::NONE).then_some(tree.parent)
    }
}
