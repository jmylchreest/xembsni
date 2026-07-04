//! [`TrayControl`]: injects pointer interactions into embedded icon windows.
//!
//! This runs on its own X11 connection so it can be used from async tasks
//! (the SNI side) without touching the blocking host event loop. Interactions
//! are delivered as synthetic `ButtonPress`/`ButtonRelease` events sent
//! directly to the icon window — the least-invasive approach that doesn't warp
//! the user's real pointer.

use std::sync::Arc;

use tracing::debug;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ButtonPressEvent, ConnectionExt as _, EventMask};
use x11rb::rust_connection::RustConnection;

use crate::{IconId, Result};

/// X11 button numbers.
const BUTTON_LEFT: u8 = 1;
const BUTTON_MIDDLE: u8 = 2;
const BUTTON_RIGHT: u8 = 3;
const BUTTON_SCROLL_UP: u8 = 4;
const BUTTON_SCROLL_DOWN: u8 = 5;
const BUTTON_SCROLL_LEFT: u8 = 6;
const BUTTON_SCROLL_RIGHT: u8 = 7;

/// A cheaply-cloneable handle for delivering interactions to embedded icons.
#[derive(Clone)]
pub struct TrayControl {
    conn: Arc<RustConnection>,
    root: u32,
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
        let root = conn.setup().roots[screen_num].root;
        Ok(Self {
            conn: Arc::new(conn),
            root,
        })
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
        let (rx, ry) = if root_x != 0 || root_y != 0 {
            (root_x as i16, root_y as i16)
        } else {
            (ex, ey)
        };

        // Apps place any resulting menu at the pointer, not at our event's
        // coordinates. Warp the X pointer to the interaction point so a menu
        // appears by the tray. (On Wayland this is best-effort — Xwayland may
        // ignore it when the real pointer is over a native Wayland surface.)
        if root_x != 0 || root_y != 0 {
            let _ = self
                .conn
                .warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, rx, ry);
            let _ = self.conn.flush();
        }

        let press = ButtonPressEvent {
            response_type: x11rb::protocol::xproto::BUTTON_PRESS_EVENT,
            detail: button,
            sequence: 0,
            time: x11rb::CURRENT_TIME,
            root: self.root,
            event: icon,
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
        self.conn.send_event(true, icon, mask, press)?;
        self.conn.send_event(true, icon, mask, release)?;
        self.conn.flush()?;
        debug!(icon = format_args!("{icon:#010x}"), button, "sent click");
        Ok(())
    }
}
