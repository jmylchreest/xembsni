//! X11 system-tray host for xembsni.
//!
//! Implements the *host* side of the freedesktop System Tray Protocol (the
//! "XEMBED" tray): it acquires the `_NET_SYSTEM_TRAY_S<screen>` selection,
//! announces itself with a `MANAGER` client message, and embeds the icon
//! windows that clients (e.g. Wine/Proton apps) dock into it.
//!
//! Each embedded icon is reparented into an offscreen container, redirected
//! via the Composite extension so its contents can be captured offscreen, and
//! tracked via the Damage extension. Contents are surfaced as [`IconImage`]s
//! (ARGB32) through [`IconEvent`]s; the `bridge`/`sni` crates turn those into
//! StatusNotifierItems. Interactions flow the other way through [`TrayControl`].
//!
//! Protocol references (spec only — no code was taken from any implementation):
//! - System Tray Protocol: <https://specifications.freedesktop.org/systemtray-spec/>
//! - XEMBED: <https://specifications.freedesktop.org/xembed-spec/>
//! - KDE's `xembed-sni-proxy` solves the same problem and was consulted for
//!   design inspiration only.

mod atoms;
mod control;
mod host;
mod image;

pub use control::TrayControl;
pub use host::{TrayHost, Waker};

/// Identifier for a docked icon: its X11 window id.
pub type IconId = u32;

/// A captured icon image in ARGB32 (`[A, R, G, B]` per pixel), row-major.
#[derive(Clone, PartialEq, Eq)]
pub struct IconImage {
    pub width: u16,
    pub height: u16,
    pub argb32: Vec<u8>,
}

impl std::fmt::Debug for IconImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IconImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.argb32.len())
            .finish()
    }
}

/// Descriptive metadata for a docked icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconMeta {
    pub id: IconId,
    /// Application identifier, derived from `WM_CLASS`.
    pub app_id: String,
    /// Human-readable title, from `_NET_WM_NAME`/`WM_NAME` (may be empty).
    pub title: String,
}

/// An event produced by the [`TrayHost`] event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconEvent {
    /// A new icon was docked and embedded. `image` is the initial capture, if
    /// one was available immediately (more arrive as [`IconEvent::Updated`]).
    Added {
        meta: IconMeta,
        image: Option<IconImage>,
    },
    /// An icon's contents changed.
    Updated { id: IconId, image: IconImage },
    /// An icon's title changed.
    TitleChanged { id: IconId, title: String },
    /// A client toggled whether its icon should be shown (`_XEMBED_INFO`). Maps
    /// to StatusNotifierItem `Status` (`Active` when visible, `Passive` when not).
    VisibilityChanged { id: IconId, visible: bool },
    /// An icon was removed (its window was destroyed or taken back).
    Removed { id: IconId },
    /// The tray selection was taken over by another host; shut down.
    SelectionLost,
}

/// Errors the tray host can produce.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to connect to the X server: {0}")]
    Connect(#[from] x11rb::errors::ConnectError),
    #[error("X11 request failed: {0}")]
    Connection(#[from] x11rb::errors::ConnectionError),
    #[error("X11 reply error: {0}")]
    Reply(#[from] x11rb::errors::ReplyError),
    #[error("could not allocate an X11 resource id: {0}")]
    ReplyOrId(#[from] x11rb::errors::ReplyOrIdError),
    #[error("another system tray already owns {0}")]
    AlreadyOwned(String),
    #[error("failed to take ownership of {0}")]
    AcquireFailed(String),
}

/// Convenience result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;
