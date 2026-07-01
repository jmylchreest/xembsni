//! Bridge between the X11 tray host and the SNI publisher.
//!
//! Consumes the [`IconEvent`] stream from the tray host and maintains one
//! published [`PublishedItem`] per icon, forwarding host interactions back to
//! the X11 side via [`TrayControl`]. Also re-registers items whenever a
//! StatusNotifierWatcher (e.g. waybar) appears, so ordering with the bar's
//! startup doesn't matter.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{info, warn};
use xembsni_sni::{ItemActions, Pixmap, PublishedItem, StatusNotifierItem};
use xembsni_tray_host::{IconEvent, IconId, IconImage, IconMeta, TrayControl};

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const DEFAULT_CATEGORY: &str = "ApplicationStatus";

/// Forwards SNI interactions to a specific X11 icon window.
struct IconActions {
    control: TrayControl,
    icon: IconId,
}

impl ItemActions for IconActions {
    fn activate(&self) {
        log_err(self.control.activate(self.icon), "activate");
    }
    fn secondary_activate(&self) {
        log_err(
            self.control.secondary_activate(self.icon),
            "secondary_activate",
        );
    }
    fn context_menu(&self) {
        log_err(self.control.context_menu(self.icon), "context_menu");
    }
    fn scroll(&self, delta: i32, horizontal: bool) {
        log_err(self.control.scroll(self.icon, delta, horizontal), "scroll");
    }
}

fn log_err(result: xembsni_tray_host::Result<()>, what: &str) {
    if let Err(err) = result {
        warn!(%err, action = what, "failed to forward interaction to icon");
    }
}

fn to_pixmap(image: &IconImage) -> Pixmap {
    vec![(
        image.width as i32,
        image.height as i32,
        image.argb32.clone(),
    )]
}

/// Run the bridge until the event stream ends or the selection is lost.
pub async fn run(
    mut events: UnboundedReceiver<IconEvent>,
    control: TrayControl,
) -> anyhow::Result<()> {
    let mut items: HashMap<IconId, PublishedItem> = HashMap::new();
    let mut seq: u64 = 0;

    // A connection just for watching the watcher come and go.
    let monitor = zbus::Connection::session().await?;
    let dbus = zbus::fdo::DBusProxy::new(&monitor).await?;
    let mut name_changes = dbus.receive_name_owner_changed().await?;

    loop {
        tokio::select! {
            maybe_event = events.recv() => {
                match maybe_event {
                    Some(event) => {
                        if matches!(event, IconEvent::SelectionLost) {
                            break;
                        }
                        handle_event(event, &mut items, &mut seq, &control).await;
                    }
                    None => break,
                }
            }
            Some(signal) = name_changes.next() => {
                if watcher_appeared(&signal) {
                    reregister_all(&items).await;
                }
            }
        }
    }

    info!(count = items.len(), "bridge shutting down; removing items");
    for (_, item) in items.drain() {
        item.remove().await;
    }
    Ok(())
}

async fn handle_event(
    event: IconEvent,
    items: &mut HashMap<IconId, PublishedItem>,
    seq: &mut u64,
    control: &TrayControl,
) {
    match event {
        IconEvent::Added { meta, image } => {
            add_item(meta, image, items, seq, control).await;
        }
        IconEvent::Updated { id, image } => {
            if let Some(item) = items.get(&id) {
                if let Err(err) = item
                    .set_icon(image.width as i32, image.height as i32, image.argb32)
                    .await
                {
                    warn!(%err, "failed to update icon");
                }
            }
        }
        IconEvent::TitleChanged { id, title } => {
            if let Some(item) = items.get(&id) {
                if let Err(err) = item.set_title(title).await {
                    warn!(%err, "failed to update title");
                }
            }
        }
        IconEvent::VisibilityChanged { id, visible } => {
            if let Some(item) = items.get(&id) {
                let status = if visible { "Active" } else { "Passive" };
                if let Err(err) = item.set_status(status).await {
                    warn!(%err, "failed to update status");
                }
            }
        }
        IconEvent::Removed { id } => {
            if let Some(item) = items.remove(&id) {
                item.remove().await;
            }
        }
        IconEvent::SelectionLost => {}
    }
}

async fn add_item(
    meta: IconMeta,
    image: Option<IconImage>,
    items: &mut HashMap<IconId, PublishedItem>,
    seq: &mut u64,
    control: &TrayControl,
) {
    let actions = Arc::new(IconActions {
        control: control.clone(),
        icon: meta.id,
    });
    let title = if meta.title.is_empty() {
        meta.app_id.clone()
    } else {
        meta.title.clone()
    };
    let pixmap = image.as_ref().map(to_pixmap).unwrap_or_default();
    let item = StatusNotifierItem::new(
        meta.app_id.clone(),
        title,
        DEFAULT_CATEGORY.to_string(),
        pixmap,
        actions,
    );

    *seq += 1;
    match PublishedItem::publish(*seq, item).await {
        Ok(published) => {
            info!(
                icon = format_args!("{:#010x}", meta.id),
                name = published.name(),
                "published item"
            );
            items.insert(meta.id, published);
        }
        Err(err) => warn!(%err, "failed to publish item"),
    }
}

fn watcher_appeared(signal: &zbus::fdo::NameOwnerChanged) -> bool {
    match signal.args() {
        Ok(args) => args.name.as_str() == WATCHER_NAME && args.new_owner.is_some(),
        Err(_) => false,
    }
}

async fn reregister_all(items: &HashMap<IconId, PublishedItem>) {
    info!(
        count = items.len(),
        "StatusNotifierWatcher appeared; re-registering items"
    );
    for item in items.values() {
        if let Err(err) = item.register().await {
            warn!(%err, name = item.name(), "re-registration failed");
        }
    }
}
