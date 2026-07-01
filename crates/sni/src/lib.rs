//! StatusNotifierItem (SNI) publisher.
//!
//! Exports each tray icon as its own `org.kde.StatusNotifierItem` on the
//! session bus and registers it with `org.kde.StatusNotifierWatcher`, so SNI
//! hosts (waybar's `tray` module, etc.) pick it up.
//!
//! Each item gets its **own** D-Bus connection and well-known name
//! (`org.kde.StatusNotifierItem-<pid>-<n>`, at `/StatusNotifierItem`). This
//! mirrors how normal SNI apps register and — crucially — means dropping the
//! connection releases the name, which the watcher reports to hosts as a clean
//! removal.
//!
//! This crate is deliberately decoupled from X11: interactions are delivered
//! through the [`ItemActions`] trait, and icons are passed as raw ARGB32.

use std::sync::Arc;

use tracing::{debug, warn};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, interface};

/// The SNI `IconPixmap` wire type: a list of `(width, height, ARGB32 bytes)`.
pub type Pixmap = Vec<(i32, i32, Vec<u8>)>;

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM_PATH: &str = "/StatusNotifierItem";

/// Interactions a host can trigger on an item, forwarded to the tray icon.
///
/// Implemented by the bridge over the X11 side. Methods are expected to be
/// quick and non-blocking.
pub trait ItemActions: Send + Sync + 'static {
    fn activate(&self);
    fn secondary_activate(&self);
    fn context_menu(&self);
    fn scroll(&self, delta: i32, horizontal: bool);
}

/// The state and D-Bus interface for a single tray item.
pub struct StatusNotifierItem {
    id: String,
    title: String,
    category: String,
    status: String,
    icon: Pixmap,
    actions: Arc<dyn ItemActions>,
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl StatusNotifierItem {
    #[zbus(property)]
    fn category(&self) -> &str {
        &self.category
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        &self.id
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        &self.title
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        &self.status
    }

    #[zbus(property)]
    fn window_id(&self) -> u32 {
        0
    }

    #[zbus(property)]
    fn icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Pixmap {
        self.icon.clone()
    }

    #[zbus(property)]
    fn overlay_icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn attention_movie_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn tool_tip(&self) -> (String, Pixmap, String, String) {
        (String::new(), Vec::new(), self.title.clone(), String::new())
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        // We forward clicks to the X11 icon rather than exposing a dbusmenu.
        false
    }

    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from("/NO_DBUSMENU").expect("valid object path")
    }

    fn activate(&self, _x: i32, _y: i32) {
        self.actions.activate();
    }

    fn secondary_activate(&self, _x: i32, _y: i32) {
        self.actions.secondary_activate();
    }

    fn context_menu(&self, _x: i32, _y: i32) {
        self.actions.context_menu();
    }

    fn scroll(&self, delta: i32, orientation: String) {
        let horizontal = orientation.eq_ignore_ascii_case("horizontal");
        self.actions.scroll(delta, horizontal);
    }

    #[zbus(signal)]
    async fn new_title(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_status(emitter: &SignalEmitter<'_>, status: &str) -> zbus::Result<()>;
}

/// A published item: its own bus connection plus the object path it serves.
///
/// Dropping this (via [`PublishedItem::remove`], or just dropping) tears down
/// the connection, releasing the name so hosts see the item disappear.
pub struct PublishedItem {
    conn: Connection,
    name: String,
}

impl std::fmt::Debug for PublishedItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublishedItem")
            .field("name", &self.name)
            .finish()
    }
}

impl PublishedItem {
    /// Publish `item` on a fresh connection named
    /// `org.kde.StatusNotifierItem-<pid>-<seq>` and register it with the watcher.
    ///
    /// Registration failure (e.g. no host running yet) is not fatal — the item
    /// stays served and can be re-registered later via [`register_with_watcher`].
    pub async fn publish(seq: u64, item: StatusNotifierItem) -> zbus::Result<Self> {
        let name = format!("org.kde.StatusNotifierItem-{}-{seq}", std::process::id());
        let conn = zbus::connection::Builder::session()?
            .name(name.clone())?
            .serve_at(ITEM_PATH, item)?
            .build()
            .await?;

        if let Err(err) = register_with_watcher(&conn, &name).await {
            warn!(%name, %err, "could not register item with StatusNotifierWatcher yet");
        }
        Ok(Self { conn, name })
    }

    /// The well-known bus name this item is published under.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// (Re-)register this item with the watcher (e.g. after a host appears).
    pub async fn register(&self) -> zbus::Result<()> {
        register_with_watcher(&self.conn, &self.name).await
    }

    /// Replace the item's icon and notify hosts.
    pub async fn set_icon(&self, width: i32, height: i32, argb32: Vec<u8>) -> zbus::Result<()> {
        let iref = self.interface().await?;
        iref.get_mut().await.icon = vec![(width, height, argb32)];
        StatusNotifierItem::new_icon(iref.signal_emitter()).await
    }

    /// Replace the item's title and notify hosts.
    pub async fn set_title(&self, title: String) -> zbus::Result<()> {
        let iref = self.interface().await?;
        {
            let mut iface = iref.get_mut().await;
            iface.title = title;
        }
        StatusNotifierItem::new_title(iref.signal_emitter()).await?;
        StatusNotifierItem::new_tool_tip(iref.signal_emitter()).await
    }

    /// Set the item's status (`Active`, `Passive`, or `NeedsAttention`) and
    /// notify hosts.
    pub async fn set_status(&self, status: &str) -> zbus::Result<()> {
        let iref = self.interface().await?;
        iref.get_mut().await.status = status.to_string();
        StatusNotifierItem::new_status(iref.signal_emitter(), status).await
    }

    /// Explicitly remove the item (drops the connection, releasing the name).
    pub async fn remove(self) {
        debug!(name = %self.name, "removing item");
        drop(self);
    }

    async fn interface(
        &self,
    ) -> zbus::Result<zbus::object_server::InterfaceRef<StatusNotifierItem>> {
        self.conn
            .object_server()
            .interface::<_, StatusNotifierItem>(ITEM_PATH)
            .await
    }
}

impl StatusNotifierItem {
    /// Build an item from its initial state and interaction handler.
    pub fn new(
        id: String,
        title: String,
        category: String,
        icon: Pixmap,
        actions: Arc<dyn ItemActions>,
    ) -> Self {
        Self {
            id,
            title,
            category,
            status: "Active".to_string(),
            icon,
            actions,
        }
    }
}

/// Call `RegisterStatusNotifierItem` on the watcher for `name`.
pub async fn register_with_watcher(conn: &Connection, name: &str) -> zbus::Result<()> {
    let proxy = zbus::Proxy::new(conn, WATCHER_NAME, WATCHER_PATH, WATCHER_NAME).await?;
    proxy
        .call_method("RegisterStatusNotifierItem", &name)
        .await?;
    debug!(%name, "registered with StatusNotifierWatcher");
    Ok(())
}
