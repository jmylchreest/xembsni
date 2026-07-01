//! Full end-to-end test: dock a fake icon, and verify the bridge publishes it
//! as a StatusNotifierItem (with a captured pixmap) and forwards interactions
//! back to the X11 window.
//!
//! Requires **both** an X server and a D-Bus session bus. When either is
//! missing the test prints a skip notice and passes. Run it locally with:
//!
//! ```sh
//! xvfb-run -a dbus-run-session -- cargo test -p xembsni-bridge --test e2e
//! ```

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;
use xembsni_tray_host::TrayHost;
use zbus::interface;
use zbus::object_server::SignalEmitter;

type Pixmap = Vec<(i32, i32, Vec<u8>)>;

const SYSTEM_TRAY_REQUEST_DOCK: u32 = 0;
const RED: u32 = 0x00ff_0000;
const ITEM_PREFIX: &str = "org.kde.StatusNotifierItem-";

/// Minimal `org.kde.StatusNotifierWatcher` that records registrations.
struct FakeWatcher {
    registered: Arc<Mutex<Vec<String>>>,
}

#[interface(name = "org.kde.StatusNotifierWatcher")]
impl FakeWatcher {
    async fn register_status_notifier_item(&self, service: String) {
        self.registered.lock().unwrap().push(service);
    }

    async fn register_status_notifier_host(&self, _service: String) {}

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.registered.lock().unwrap().clone()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(e: &SignalEmitter<'_>, s: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(e: &SignalEmitter<'_>, s: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(e: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Spawn a fake icon on its own connection: solid red, docked, recording the
/// pointer buttons it receives. Returns the icon id and the shared button log.
fn spawn_fake_icon() -> anyhow::Result<(u32, Arc<Mutex<Vec<u8>>>)> {
    let buttons = Arc::new(Mutex::new(Vec::new()));
    let buttons_thread = buttons.clone();
    let (id_tx, id_rx) = std::sync::mpsc::channel();

    thread::spawn(move || {
        let (conn, screen_num) = x11rb::connect(None).unwrap();
        let root = conn.setup().roots[screen_num].root;
        let visual = conn.setup().roots[screen_num].root_visual;

        let icon = conn.generate_id().unwrap();
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            icon,
            root,
            0,
            0,
            24,
            24,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &CreateWindowAux::new().background_pixel(RED).event_mask(
                EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::STRUCTURE_NOTIFY,
            ),
        )
        .unwrap();
        conn.change_property8(
            PropMode::REPLACE,
            icon,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"e2eicon\0E2eIcon\0",
        )
        .unwrap();
        conn.change_property8(
            PropMode::REPLACE,
            icon,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"E2E Icon",
        )
        .unwrap();

        let selection = conn
            .intern_atom(false, format!("_NET_SYSTEM_TRAY_S{screen_num}").as_bytes())
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let opcode = conn
            .intern_atom(false, b"_NET_SYSTEM_TRAY_OPCODE")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let owner = conn
            .get_selection_owner(selection)
            .unwrap()
            .reply()
            .unwrap()
            .owner;

        let dock = ClientMessageEvent::new(
            32,
            owner,
            opcode,
            [x11rb::CURRENT_TIME, SYSTEM_TRAY_REQUEST_DOCK, icon, 0, 0],
        );
        conn.send_event(false, owner, EventMask::NO_EVENT, dock)
            .unwrap();
        conn.flush().unwrap();
        id_tx.send(icon).unwrap();

        while let Ok(event) = conn.wait_for_event() {
            if let Event::ButtonPress(ev) = event {
                buttons_thread.lock().unwrap().push(ev.detail);
            }
        }
    });

    let icon = id_rx.recv_timeout(Duration::from_secs(5))?;
    Ok((icon, buttons))
}

/// Poll `f` until it returns `Some`, or give up after `timeout`.
async fn poll_until<T, F, Fut>(timeout: Duration, mut f: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(v) = f().await {
            return Some(v);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn icon_is_published_and_interactive() -> anyhow::Result<()> {
    if x11rb::connect(None).is_err() {
        eprintln!("skipping: no X server available");
        return Ok(());
    }
    let Ok(bus) = zbus::Connection::session().await else {
        eprintln!("skipping: no D-Bus session bus available");
        return Ok(());
    };

    // Stand up a fake watcher that records registrations.
    let registered = Arc::new(Mutex::new(Vec::new()));
    let _watcher = zbus::connection::Builder::session()?
        .name("org.kde.StatusNotifierWatcher")?
        .serve_at(
            "/StatusNotifierWatcher",
            FakeWatcher {
                registered: registered.clone(),
            },
        )?
        .build()
        .await?;

    // Start the tray host + bridge.
    let host = TrayHost::acquire()?;
    let control = host.control()?;
    let waker = host.waker()?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let x11_thread = thread::spawn(move || {
        host.run(|e| {
            let _ = tx.send(e);
        })
    });
    let bridge = tokio::spawn(xembsni_bridge::run(rx, control));

    // Dock a fake icon.
    let (_icon, buttons) = spawn_fake_icon()?;

    // It should get registered with the watcher under an SNI item name.
    let name = poll_until(Duration::from_secs(10), || async {
        registered
            .lock()
            .unwrap()
            .iter()
            .find(|n| n.starts_with(ITEM_PREFIX))
            .cloned()
    })
    .await
    .expect("item was never registered with the watcher");

    // Read the item's properties over D-Bus, as a host would.
    let item = zbus::Proxy::new(
        &bus,
        name.clone(),
        "/StatusNotifierItem",
        "org.kde.StatusNotifierItem",
    )
    .await?;

    let title: String = item.get_property("Title").await?;
    assert_eq!(title, "E2E Icon");
    let id: String = item.get_property("Id").await?;
    assert_eq!(id, "e2eicon");

    // The pixmap should eventually reflect the icon's red contents.
    let got_red = poll_until(Duration::from_secs(10), || async {
        let pixmap: Pixmap = item.get_property("IconPixmap").await.ok()?;
        let red = pixmap
            .iter()
            .any(|(_, _, bytes)| bytes.chunks_exact(4).any(|p| p == [0xff, 0xff, 0x00, 0x00]));
        red.then_some(())
    })
    .await;
    assert!(
        got_red.is_some(),
        "item never exposed the icon's red pixels"
    );

    // Activating the item should forward a left click to the X11 icon.
    item.call_method("Activate", &(0i32, 0i32)).await?;
    let clicked = poll_until(Duration::from_secs(5), || async {
        buttons.lock().unwrap().contains(&1).then_some(())
    })
    .await;
    assert!(
        clicked.is_some(),
        "Activate did not reach the icon as a click"
    );

    // Right click via ContextMenu should reach it as button 3.
    item.call_method("ContextMenu", &(0i32, 0i32)).await?;
    let right = poll_until(Duration::from_secs(5), || async {
        buttons.lock().unwrap().contains(&3).then_some(())
    })
    .await;
    assert!(
        right.is_some(),
        "ContextMenu did not reach the icon as a right click"
    );

    // Shut down.
    waker.wake()?;
    let _ = bridge.await;
    x11_thread.join().expect("x11 thread panicked")?;
    Ok(())
}
