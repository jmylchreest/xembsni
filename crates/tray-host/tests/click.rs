//! A click slides the icon under the interaction point inside its container,
//! and the icon stays there: it keeps repainting and being captured, and the
//! next click moves it again and reaches the app.
//!
//! Requires a running X server (`$DISPLAY`) and skips cleanly otherwise:
//!
//! ```sh
//! xvfb-run -a cargo test -p xembsni-tray-host --test click
//! ```

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt as _, CreateWindowAux,
    EventMask, PropMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use xembsni_tray_host::{IconEvent, IconImage, TrayHost};

const SYSTEM_TRAY_REQUEST_DOCK: u32 = 0;
const ICON_SIZE: u16 = 24;
const RED: u32 = 0x00ff_0000;
const BLUE: u32 = 0x0000_00ff;
const BUTTON_RIGHT: u8 = 3;

fn x_server_available() -> bool {
    match x11rb::connect(None) {
        Ok(_) => true,
        Err(err) => {
            eprintln!("skipping: no X server available ({err})");
            false
        }
    }
}

/// Create a red icon window that listens for button events, as a toolkit's
/// tray icon does, and dock it with the tray owner.
fn dock_icon() -> anyhow::Result<(RustConnection, u32, u32)> {
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
        &CreateWindowAux::new()
            .background_pixel(RED)
            .event_mask(EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE),
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        icon,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"clickicon\0ClickIcon\0",
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
    Ok((conn, root, icon))
}

fn has_pixel(image: &IconImage, rgb: u32) -> bool {
    let [_, r, g, b] = rgb.to_be_bytes();
    image.argb32.chunks_exact(4).any(|px| px == [0xff, r, g, b])
}

/// Wait for the host to publish a capture of `icon` containing `rgb`.
fn wait_for_capture(rx: &mpsc::Receiver<IconEvent>, icon: u32, rgb: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(IconEvent::Added { meta, image }) if meta.id == icon => {
                if image.as_ref().is_some_and(|image| has_pixel(image, rgb)) {
                    return true;
                }
            }
            Ok(IconEvent::Updated { id, image }) if id == icon && has_pixel(&image, rgb) => {
                return true;
            }
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => panic!("event channel closed: {e}"),
        }
    }
    false
}

/// Wait for the client to receive a press of `button` on `icon`.
fn wait_for_press(client: &RustConnection, icon: u32, button: u8) -> anyhow::Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        while let Some(event) = client.poll_for_event()? {
            if matches!(event, Event::ButtonPress(press) if press.event == icon && press.detail == button)
            {
                return Ok(true);
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(false)
}

/// Root coordinates of the icon's origin, as an app asks the server for them.
fn icon_origin(client: &RustConnection, root: u32, icon: u32) -> anyhow::Result<(i16, i16)> {
    let reply = client.translate_coordinates(icon, root, 0, 0)?.reply()?;
    Ok((reply.dst_x, reply.dst_y))
}

#[test]
fn a_clicked_icon_stays_under_the_click_and_keeps_working() -> anyhow::Result<()> {
    if !x_server_available() {
        return Ok(());
    }

    let host = TrayHost::acquire()?;
    let control = host.control()?;
    let waker = host.waker()?;
    let (tx, rx) = mpsc::channel();
    let loop_thread = thread::spawn(move || host.run(|event| tx.send(event).unwrap()));

    thread::sleep(Duration::from_millis(100));
    let (client, root, icon) = dock_icon()?;
    assert!(
        wait_for_capture(&rx, icon, RED),
        "never captured the red icon"
    );

    let centre = i16::try_from(ICON_SIZE / 2)?;

    // First click: the app reads the icon under the click point.
    control.context_menu(icon, 100, 80)?;
    assert!(
        wait_for_press(&client, icon, BUTTON_RIGHT)?,
        "first click never arrived"
    );
    assert_eq!(
        icon_origin(&client, root, icon)?,
        (100 - centre, 80 - centre)
    );

    // Well past the old 400 ms reset, it is still there for a slow app.
    thread::sleep(Duration::from_millis(600));
    assert_eq!(
        icon_origin(&client, root, icon)?,
        (100 - centre, 80 - centre),
        "the icon left the click point on its own"
    );

    // Still slid, the icon repaints and the host captures the new pixels.
    client.change_window_attributes(
        icon,
        &ChangeWindowAttributesAux::new().background_pixel(BLUE),
    )?;
    client.clear_area(false, icon, 0, 0, 0, 0)?;
    client.flush()?;
    assert!(
        wait_for_capture(&rx, icon, BLUE),
        "a slid icon was not captured again"
    );

    // A second click right away moves it to its own point and reaches the app.
    control.context_menu(icon, 200, 150)?;
    assert!(
        wait_for_press(&client, icon, BUTTON_RIGHT)?,
        "second click never arrived"
    );
    assert_eq!(
        icon_origin(&client, root, icon)?,
        (200 - centre, 150 - centre)
    );

    client.destroy_window(icon)?;
    client.flush()?;
    waker.wake()?;
    loop_thread.join().expect("event loop thread panicked")?;
    Ok(())
}
