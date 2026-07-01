//! End-to-end test of the tray host against a real (headless) X server: create
//! a client icon window, dock it, and assert it is embedded, captured, and
//! removed.
//!
//! Requires a running X server (`$DISPLAY`). When none is reachable the test
//! prints a skip notice and passes, so it stays green in headless CI without
//! Xvfb. To run it locally against a throwaway server:
//!
//! ```sh
//! xvfb-run -a cargo test -p xembsni-tray-host
//! ```

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use xembsni_tray_host::{IconEvent, IconImage, TrayHost};

const SYSTEM_TRAY_REQUEST_DOCK: u32 = 0;
const ICON_SIZE: u16 = 24;
const RED: u32 = 0x00ff_0000;

fn x_server_available() -> bool {
    match x11rb::connect(None) {
        Ok(_) => true,
        Err(err) => {
            eprintln!("skipping: no X server available ({err})");
            false
        }
    }
}

/// Create a client icon window (unmapped, solid red background) and dock it,
/// discovering the tray owner via the `_NET_SYSTEM_TRAY_S<n>` selection exactly
/// as a real client would. Returns the client connection (kept alive so the
/// window survives) and the icon window id.
fn dock_icon() -> anyhow::Result<(RustConnection, u32)> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let visual = conn.setup().roots[screen_num].root_visual;

    let icon = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        icon,
        root,
        0,
        0,
        ICON_SIZE,
        ICON_SIZE,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &CreateWindowAux::new().background_pixel(RED),
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        icon,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"testicon\0TestIcon\0",
    )?;

    let selection = conn
        .intern_atom(false, format!("_NET_SYSTEM_TRAY_S{screen_num}").as_bytes())?
        .reply()?
        .atom;
    let opcode = conn
        .intern_atom(false, b"_NET_SYSTEM_TRAY_OPCODE")?
        .reply()?
        .atom;
    let owner = conn.get_selection_owner(selection)?.reply()?.owner;
    assert_ne!(owner, x11rb::NONE, "tray host did not own the selection");

    let msg = ClientMessageEvent::new(
        32,
        owner,
        opcode,
        [x11rb::CURRENT_TIME, SYSTEM_TRAY_REQUEST_DOCK, icon, 0, 0],
    );
    conn.send_event(false, owner, EventMask::NO_EVENT, msg)?;
    conn.flush()?;
    Ok((conn, icon))
}

fn has_red_pixel(image: &IconImage) -> bool {
    image
        .argb32
        .chunks_exact(4)
        .any(|px| px == [0xff, 0xff, 0x00, 0x00])
}

#[test]
fn icon_is_embedded_captured_and_removed() -> anyhow::Result<()> {
    if !x_server_available() {
        return Ok(());
    }

    let host = TrayHost::acquire()?;
    let waker = host.waker()?;
    let (tx, rx) = mpsc::channel();
    let loop_thread = thread::spawn(move || host.run(|event| tx.send(event).unwrap()));

    thread::sleep(Duration::from_millis(100));
    let (client, icon) = dock_icon()?;

    // Expect an Added event for our window, then a captured red image (either
    // on Added or a subsequent Updated once the server paints the background).
    let mut added = false;
    let mut captured_red = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !(added && captured_red) {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(IconEvent::Added { meta, image }) => {
                assert_eq!(meta.id, icon);
                assert_eq!(meta.app_id, "testicon");
                added = true;
                if image.as_ref().is_some_and(has_red_pixel) {
                    captured_red = true;
                }
            }
            Ok(IconEvent::Updated { id, image }) if id == icon => {
                captured_red |= has_red_pixel(&image);
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => panic!("event channel closed: {e}"),
        }
    }
    assert!(added, "never received an Added event");
    assert!(captured_red, "never captured the icon's red pixels");

    // Destroying the client window should surface a Removed event.
    client.destroy_window(icon)?;
    client.flush()?;
    let mut removed = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !removed {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(IconEvent::Removed { id }) if id == icon => removed = true,
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => panic!("event channel closed: {e}"),
        }
    }
    assert!(removed, "never received a Removed event");

    waker.wake()?;
    loop_thread.join().expect("event loop thread panicked")?;
    Ok(())
}
