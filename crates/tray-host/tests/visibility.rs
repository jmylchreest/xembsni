//! Verify that toggling a client's `_XEMBED_INFO` mapped flag is surfaced as
//! [`IconEvent::VisibilityChanged`] (which the bridge maps to SNI status).
//!
//! Requires an X server; skips cleanly otherwise. Run under Xvfb:
//! `xvfb-run -a cargo test -p xembsni-tray-host`.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use xembsni_tray_host::{IconEvent, TrayHost};

const SYSTEM_TRAY_REQUEST_DOCK: u32 = 0;
const XEMBED_MAPPED: u32 = 1;

fn set_mapped(conn: &RustConnection, icon: u32, xembed_info: u32, mapped: bool) {
    let flags = if mapped { XEMBED_MAPPED } else { 0 };
    conn.change_property32(
        PropMode::REPLACE,
        icon,
        xembed_info,
        xembed_info,
        &[0, flags],
    )
    .unwrap();
    conn.flush().unwrap();
}

fn wait_visibility(rx: &mpsc::Receiver<IconEvent>, icon: u32) -> Option<bool> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(IconEvent::VisibilityChanged { id, visible }) if id == icon => return Some(visible),
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => return None,
        }
    }
    None
}

#[test]
fn xembed_info_toggles_visibility() -> anyhow::Result<()> {
    if x11rb::connect(None).is_err() {
        eprintln!("skipping: no X server available");
        return Ok(());
    }

    let host = TrayHost::acquire()?;
    let waker = host.waker()?;
    let (tx, rx) = mpsc::channel();
    let loop_thread = thread::spawn(move || host.run(|e| tx.send(e).unwrap()));
    thread::sleep(Duration::from_millis(100));

    // Client icon: starts mapped.
    let (conn, screen_num) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let visual = conn.setup().roots[screen_num].root_visual;
    let xembed_info = conn.intern_atom(false, b"_XEMBED_INFO")?.reply()?.atom;

    let icon = conn.generate_id()?;
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
        &CreateWindowAux::new().background_pixel(0x00ff_0000),
    )?;
    set_mapped(&conn, icon, xembed_info, true);

    let selection = conn
        .intern_atom(false, format!("_NET_SYSTEM_TRAY_S{screen_num}").as_bytes())?
        .reply()?
        .atom;
    let opcode = conn
        .intern_atom(false, b"_NET_SYSTEM_TRAY_OPCODE")?
        .reply()?
        .atom;
    let owner = conn.get_selection_owner(selection)?.reply()?.owner;
    let dock = ClientMessageEvent::new(
        32,
        owner,
        opcode,
        [x11rb::CURRENT_TIME, SYSTEM_TRAY_REQUEST_DOCK, icon, 0, 0],
    );
    conn.send_event(false, owner, EventMask::NO_EVENT, dock)?;
    conn.flush()?;

    // Wait for it to be embedded.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut added = false;
    while Instant::now() < deadline && !added {
        if let Ok(IconEvent::Added { meta, .. }) = rx.recv_timeout(Duration::from_millis(500)) {
            added = meta.id == icon;
        }
    }
    assert!(added, "icon was never embedded");

    // Hide, then show, via _XEMBED_INFO.
    set_mapped(&conn, icon, xembed_info, false);
    assert_eq!(wait_visibility(&rx, icon), Some(false), "hide not reported");

    set_mapped(&conn, icon, xembed_info, true);
    assert_eq!(wait_visibility(&rx, icon), Some(true), "show not reported");

    waker.wake()?;
    loop_thread.join().expect("event loop thread panicked")?;
    Ok(())
}
