//! [`TrayHost`]: owns the tray selection, embeds docked icons, and captures
//! their pixmaps.

use std::collections::HashMap;
use std::fmt;

use tracing::{debug, info, warn};
use x11rb::COPY_DEPTH_FROM_PARENT;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::composite::{ConnectionExt as _, Redirect};
use x11rb::protocol::damage::{ConnectionExt as _, ReportLevel};
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, ImageFormat,
    PropMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

use crate::atoms::Atoms;
use crate::control::TrayControl;
use crate::image::{self, PixelFormat};
use crate::{Error, IconEvent, IconImage, IconMeta, Result};

/// `_NET_SYSTEM_TRAY_OPCODE` values (`data[1]`).
const SYSTEM_TRAY_REQUEST_DOCK: u32 = 0;
const SYSTEM_TRAY_BEGIN_MESSAGE: u32 = 1;
const SYSTEM_TRAY_CANCEL_MESSAGE: u32 = 2;

/// `_NET_SYSTEM_TRAY_ORIENTATION` value: lay icons out horizontally.
const TRAY_ORIENTATION_HORZ: u32 = 0;

/// XEMBED protocol.
const XEMBED_EMBEDDED_NOTIFY: u32 = 0;
const XEMBED_VERSION: u32 = 0;
/// `_XEMBED_INFO` flag: the client wants to be mapped.
const XEMBED_MAPPED: u32 = 1 << 0;

/// Where embedded containers live: far offscreen so no compositor shows them.
const OFFSCREEN: i16 = -16000;

/// Find a 32-bit TrueColor/DirectColor (ARGB) visual on `screen`, if one exists.
fn argb_visual(screen: &x11rb::protocol::xproto::Screen) -> Option<u32> {
    for depth in &screen.allowed_depths {
        if depth.depth == 32 {
            for visual in &depth.visuals {
                if matches!(
                    visual.class,
                    x11rb::protocol::xproto::VisualClass::TRUE_COLOR
                        | x11rb::protocol::xproto::VisualClass::DIRECT_COLOR
                ) {
                    return Some(visual.visual_id);
                }
            }
        }
    }
    None
}

/// Per-icon bookkeeping.
struct Icon {
    container: Window,
    damage: u32,
    width: u16,
    height: u16,
    format: Option<PixelFormat>,
    /// Whether the client currently wants its icon shown (`_XEMBED_INFO`).
    mapped: bool,
}

/// Owns the `_NET_SYSTEM_TRAY_S<screen>` selection and hosts docked icons.
pub struct TrayHost {
    conn: RustConnection,
    atoms: Atoms,
    root: Window,
    root_visual: u32,
    owner: Window,
    selection_name: String,
    selection_atom: u32,
}

impl fmt::Debug for TrayHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrayHost")
            .field("owner", &format_args!("{:#010x}", self.owner))
            .field("selection", &self.selection_name)
            .finish_non_exhaustive()
    }
}

impl TrayHost {
    /// Connect to `$DISPLAY` and take ownership of the tray selection.
    pub fn acquire() -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None)?;
        Self::acquire_on(conn, screen_num)
    }

    /// As [`Self::acquire`], but for an already-established connection/screen.
    pub fn acquire_on(conn: RustConnection, screen_num: usize) -> Result<Self> {
        // The extensions we rely on for offscreen capture and damage tracking.
        conn.composite_query_version(0, 4)?.reply()?;
        conn.damage_query_version(1, 1)?.reply()?;

        let atoms = Atoms::new(&conn)?.reply()?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let root_visual = screen.root_visual;

        let selection_name = format!("_NET_SYSTEM_TRAY_S{screen_num}");
        let selection_atom = conn
            .intern_atom(false, selection_name.as_bytes())?
            .reply()?
            .atom;

        if conn.get_selection_owner(selection_atom)?.reply()?.owner != NONE {
            return Err(Error::AlreadyOwned(selection_name));
        }

        let owner = conn.generate_id()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            owner,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            root_visual,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        conn.change_property32(
            PropMode::REPLACE,
            owner,
            atoms._NET_SYSTEM_TRAY_ORIENTATION,
            AtomEnum::CARDINAL,
            &[TRAY_ORIENTATION_HORZ],
        )?;

        // Advertise a 32-bit ARGB visual so clients (notably Wine) render their
        // icon with a real alpha channel instead of an opaque background.
        if let Some(visual) = argb_visual(screen) {
            conn.change_property32(
                PropMode::REPLACE,
                owner,
                atoms._NET_SYSTEM_TRAY_VISUAL,
                AtomEnum::VISUALID,
                &[visual],
            )?;
        }

        conn.set_selection_owner(owner, selection_atom, CURRENT_TIME)?;
        conn.flush()?;
        if conn.get_selection_owner(selection_atom)?.reply()?.owner != owner {
            return Err(Error::AcquireFailed(selection_name));
        }

        let announce = ClientMessageEvent::new(
            32,
            root,
            atoms.MANAGER,
            [CURRENT_TIME, selection_atom, owner, 0, 0],
        );
        conn.send_event(false, root, EventMask::STRUCTURE_NOTIFY, announce)?;
        conn.flush()?;

        info!(
            selection = %selection_name,
            owner = format_args!("{owner:#010x}"),
            "acquired system tray selection"
        );

        Ok(Self {
            conn,
            atoms,
            root,
            root_visual,
            owner,
            selection_name,
            selection_atom,
        })
    }

    /// The interned name of the selection this host owns.
    pub fn selection_name(&self) -> &str {
        &self.selection_name
    }

    /// Create a [`Waker`] that interrupts [`Self::run`] from another thread.
    pub fn waker(&self) -> Result<Waker> {
        let (conn, _screen) = x11rb::connect(None)?;
        Ok(Waker {
            conn,
            target: self.owner,
            wakeup_atom: self.atoms._XEMBSNI_WAKEUP,
        })
    }

    /// Create a [`TrayControl`] for forwarding SNI interactions to icons.
    pub fn control(&self) -> Result<TrayControl> {
        TrayControl::connect()
    }

    /// Run the blocking event loop, invoking `on_event` for each [`IconEvent`].
    ///
    /// Returns when interrupted via [`Waker`] or when the selection is lost.
    /// Surviving icons are reparented back to the root on exit so their owners
    /// don't lose their windows.
    pub fn run<F: FnMut(IconEvent)>(&self, mut on_event: F) -> Result<()> {
        let mut icons: HashMap<Window, Icon> = HashMap::new();
        let mut damage_to_icon: HashMap<u32, Window> = HashMap::new();

        let result = self.event_loop(&mut icons, &mut damage_to_icon, &mut on_event);
        self.teardown(&mut icons);
        result
    }

    fn event_loop<F: FnMut(IconEvent)>(
        &self,
        icons: &mut HashMap<Window, Icon>,
        damage_to_icon: &mut HashMap<u32, Window>,
        on_event: &mut F,
    ) -> Result<()> {
        loop {
            let event = self.conn.wait_for_event()?;
            match event {
                Event::ClientMessage(msg) if msg.type_ == self.atoms._XEMBSNI_WAKEUP => {
                    debug!("event loop woken for shutdown");
                    return Ok(());
                }
                Event::ClientMessage(msg) if msg.type_ == self.atoms._NET_SYSTEM_TRAY_OPCODE => {
                    let data = msg.data.as_data32();
                    match data[1] {
                        SYSTEM_TRAY_REQUEST_DOCK => {
                            self.on_dock(data[2], icons, damage_to_icon, on_event)
                        }
                        SYSTEM_TRAY_BEGIN_MESSAGE | SYSTEM_TRAY_CANCEL_MESSAGE => {}
                        other => debug!(opcode = other, "ignoring unknown tray opcode"),
                    }
                }
                Event::DamageNotify(ev) => {
                    if let Some(&icon) = damage_to_icon.get(&ev.damage) {
                        self.on_damage(icon, icons, on_event);
                    }
                }
                Event::DestroyNotify(ev) if icons.contains_key(&ev.window) => {
                    self.remove_icon(ev.window, icons, damage_to_icon, on_event);
                }
                Event::ReparentNotify(ev) if icons.contains_key(&ev.window) => {
                    // The client pulled its icon back out of our container; let go.
                    let ours = icons.get(&ev.window).map(|i| i.container);
                    if ours != Some(ev.parent) {
                        self.remove_icon(ev.window, icons, damage_to_icon, on_event);
                    }
                }
                Event::PropertyNotify(ev) if icons.contains_key(&ev.window) => {
                    if ev.atom == self.atoms._XEMBED_INFO {
                        self.on_xembed_info(ev.window, icons, on_event);
                    } else if ev.atom == self.atoms._NET_WM_NAME
                        || ev.atom == u32::from(AtomEnum::WM_NAME)
                    {
                        let title = self.read_title(ev.window);
                        on_event(IconEvent::TitleChanged {
                            id: ev.window,
                            title,
                        });
                    }
                }
                Event::SelectionClear(ev) if ev.selection == self.selection_atom => {
                    warn!(selection = %self.selection_name, "lost tray selection ownership");
                    on_event(IconEvent::SelectionLost);
                    return Ok(());
                }
                other => debug!(?other, "ignoring X11 event"),
            }
        }
    }

    fn on_dock<F: FnMut(IconEvent)>(
        &self,
        icon: Window,
        icons: &mut HashMap<Window, Icon>,
        damage_to_icon: &mut HashMap<u32, Window>,
        on_event: &mut F,
    ) {
        if icon == NONE || icons.contains_key(&icon) {
            return;
        }
        match self.embed(icon) {
            Ok((state, meta, image)) => {
                info!(icon = format_args!("{icon:#010x}"), app = %meta.app_id, "embedded icon");
                damage_to_icon.insert(state.damage, icon);
                icons.insert(icon, state);
                on_event(IconEvent::Added { meta, image });
            }
            Err(err) => {
                warn!(icon = format_args!("{icon:#010x}"), %err, "failed to embed icon");
            }
        }
    }

    /// Reparent `icon` into an offscreen container, wire up XEMBED, redirect it
    /// for offscreen capture, and grab an initial image.
    fn embed(&self, icon: Window) -> Result<(Icon, IconMeta, Option<IconImage>)> {
        let geo = self.conn.get_geometry(icon)?.reply()?;
        let attrs = self.conn.get_window_attributes(icon)?.reply()?;
        let width = geo.width.max(1);
        let height = geo.height.max(1);
        let format = PixelFormat::for_visual(self.conn.setup(), attrs.visual);

        // Watch the icon for destruction, unmap, and title changes.
        self.conn
            .change_window_attributes(
                icon,
                &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
                    .event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
            )?
            .check()?;

        // Offscreen, unmanaged container to hold the embedded icon.
        let container = self.conn.generate_id()?;
        self.conn
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                container,
                self.root,
                OFFSCREEN,
                OFFSCREEN,
                width,
                height,
                0,
                WindowClass::INPUT_OUTPUT,
                self.root_visual,
                &CreateWindowAux::new()
                    .override_redirect(1)
                    .event_mask(EventMask::STRUCTURE_NOTIFY),
            )?
            .check()?;

        self.conn.reparent_window(icon, container, 0, 0)?;
        self.conn
            .composite_redirect_window(icon, Redirect::AUTOMATIC)?;

        let damage = self.conn.generate_id()?;
        self.conn
            .damage_create(damage, icon, ReportLevel::NON_EMPTY)?;

        // Map both, then tell the client it's embedded.
        let want_mapped = self
            .read_xembed_info(icon)
            .map(|flags| flags & XEMBED_MAPPED != 0)
            .unwrap_or(true);
        if want_mapped {
            self.conn.map_window(icon)?;
        }
        self.conn.map_window(container)?;

        let notify = ClientMessageEvent::new(
            32,
            icon,
            self.atoms._XEMBED,
            [
                CURRENT_TIME,
                XEMBED_EMBEDDED_NOTIFY,
                0,
                container,
                XEMBED_VERSION,
            ],
        );
        self.conn
            .send_event(false, icon, EventMask::NO_EVENT, notify)?;
        self.conn.flush()?;

        let state = Icon {
            container,
            damage,
            width,
            height,
            format,
            mapped: want_mapped,
        };
        let image = self.capture(icon, &state);
        let meta = IconMeta {
            id: icon,
            app_id: self.read_app_id(icon),
            title: self.read_title(icon),
        };
        Ok((state, meta, image))
    }

    /// Handle a change to a client's `_XEMBED_INFO`: map/unmap the icon to match
    /// the requested visibility and surface it as [`IconEvent::VisibilityChanged`].
    fn on_xembed_info<F: FnMut(IconEvent)>(
        &self,
        icon: Window,
        icons: &mut HashMap<Window, Icon>,
        on_event: &mut F,
    ) {
        let Some(state) = icons.get_mut(&icon) else {
            return;
        };
        let want = self
            .read_xembed_info(icon)
            .map(|flags| flags & XEMBED_MAPPED != 0)
            .unwrap_or(true);
        if want == state.mapped {
            return;
        }
        let _ = if want {
            self.conn.map_window(icon)
        } else {
            self.conn.unmap_window(icon)
        };
        let _ = self.conn.flush();
        state.mapped = want;
        on_event(IconEvent::VisibilityChanged {
            id: icon,
            visible: want,
        });
    }

    fn on_damage<F: FnMut(IconEvent)>(
        &self,
        icon: Window,
        icons: &mut HashMap<Window, Icon>,
        on_event: &mut F,
    ) {
        let Some(state) = icons.get_mut(&icon) else {
            return;
        };
        // Acknowledge the damage so further changes keep being reported.
        let _ = self.conn.damage_subtract(state.damage, NONE, NONE);

        // Pick up size changes so capture stays correct.
        if let Ok(cookie) = self.conn.get_geometry(icon) {
            if let Ok(geo) = cookie.reply() {
                state.width = geo.width.max(1);
                state.height = geo.height.max(1);
            }
        }
        let snapshot = Icon {
            container: state.container,
            damage: state.damage,
            width: state.width,
            height: state.height,
            format: state.format,
            mapped: state.mapped,
        };
        if let Some(image) = self.capture(icon, &snapshot) {
            on_event(IconEvent::Updated { id: icon, image });
        }
    }

    /// Grab the icon's current contents from its redirected offscreen storage.
    fn capture(&self, icon: Window, state: &Icon) -> Option<IconImage> {
        let format = state.format?;
        let pixmap = self.conn.generate_id().ok()?;
        // NameWindowPixmap yields the backing store for the redirected window.
        // It fails with BadMatch if the window isn't viewable yet; skip if so.
        let named = self.conn.composite_name_window_pixmap(icon, pixmap).ok()?;
        if named.check().is_err() {
            return None;
        }
        let image = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                pixmap,
                0,
                0,
                state.width,
                state.height,
                !0,
            )
            .ok()
            .and_then(|c| c.reply().ok());
        let _ = self.conn.free_pixmap(pixmap);

        let reply = image?;
        let argb32 = image::to_argb32(state.width, state.height, &reply.data, format);
        Some(IconImage {
            width: state.width,
            height: state.height,
            argb32,
        })
    }

    fn remove_icon<F: FnMut(IconEvent)>(
        &self,
        icon: Window,
        icons: &mut HashMap<Window, Icon>,
        damage_to_icon: &mut HashMap<u32, Window>,
        on_event: &mut F,
    ) {
        if let Some(state) = icons.remove(&icon) {
            damage_to_icon.remove(&state.damage);
            let _ = self.conn.damage_destroy(state.damage);
            let _ = self.conn.destroy_window(state.container);
            let _ = self.conn.flush();
            info!(icon = format_args!("{icon:#010x}"), "removed icon");
            on_event(IconEvent::Removed { id: icon });
        }
    }

    /// On shutdown, rescue any surviving icons back onto the root window.
    fn teardown(&self, icons: &mut HashMap<Window, Icon>) {
        for (icon, state) in icons.drain() {
            let _ = self.conn.damage_destroy(state.damage);
            let _ = self.conn.reparent_window(icon, self.root, 0, 0);
            let _ = self.conn.destroy_window(state.container);
        }
        let _ = self.conn.flush();
    }

    fn read_xembed_info(&self, icon: Window) -> Option<u32> {
        // `_XEMBED_INFO` has its own atom as its type (not CARDINAL), so match
        // any type rather than filtering.
        let reply = self
            .conn
            .get_property(false, icon, self.atoms._XEMBED_INFO, AtomEnum::ANY, 0, 2)
            .ok()?
            .reply()
            .ok()?;
        let vals: Vec<u32> = reply.value32()?.collect();
        vals.get(1).copied()
    }

    fn read_app_id(&self, icon: Window) -> String {
        // WM_CLASS is two NUL-separated strings: instance then class.
        let reply =
            self.conn
                .get_property(false, icon, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256);
        if let Ok(Ok(reply)) = reply.map(|c| c.reply()) {
            let mut parts = reply.value.split(|&b| b == 0);
            if let Some(instance) = parts.next() {
                if !instance.is_empty() {
                    return String::from_utf8_lossy(instance).into_owned();
                }
            }
        }
        format!("xembsni-{icon:08x}")
    }

    fn read_title(&self, icon: Window) -> String {
        for (atom, ty) in [
            (self.atoms._NET_WM_NAME, self.atoms.UTF8_STRING),
            (u32::from(AtomEnum::WM_NAME), u32::from(AtomEnum::STRING)),
        ] {
            if let Ok(Ok(reply)) = self
                .conn
                .get_property(false, icon, atom, ty, 0, 1024)
                .map(|c| c.reply())
            {
                if !reply.value.is_empty() {
                    return String::from_utf8_lossy(&reply.value).into_owned();
                }
            }
        }
        String::new()
    }
}

/// A thread-safe handle used to interrupt [`TrayHost::run`].
pub struct Waker {
    conn: RustConnection,
    target: Window,
    wakeup_atom: u32,
}

impl fmt::Debug for Waker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Waker")
            .field("target", &format_args!("{:#010x}", self.target))
            .finish_non_exhaustive()
    }
}

impl Waker {
    /// Send a wakeup client message so a blocked [`TrayHost::run`] returns.
    pub fn wake(&self) -> Result<()> {
        let msg = ClientMessageEvent::new(32, self.target, self.wakeup_atom, [0u32; 5]);
        self.conn
            .send_event(false, self.target, EventMask::NO_EVENT, msg)?;
        self.conn.flush()?;
        Ok(())
    }
}
