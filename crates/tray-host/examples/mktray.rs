//! `mktray` — a minimal fake XEmbed tray icon, for exercising xembsni without a
//! real Wine/Proton app.
//!
//! It creates a solid-colour icon window, docks it into whatever owns the
//! system-tray selection (i.e. a running `xembsni`), and prints the pointer
//! buttons it receives — so you can watch clicks forwarded back from an SNI
//! host like waybar.
//!
//! ```sh
//! # Terminal 1: the daemon
//! cargo run -p xembsni
//! # Terminal 2: a fake icon (optionally an AARRGGBB colour and a title)
//! cargo run -p xembsni-tray-host --example mktray -- 00ff8800 "My Fake Icon"
//! ```

use std::thread;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

const SYSTEM_TRAY_REQUEST_DOCK: u32 = 0;
const XEMBED_MAPPED: u32 = 1;
const SIZE: u16 = 24;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let color = args
        .next()
        .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0x00ff_8800);
    let title = args.next().unwrap_or_else(|| "mktray".to_string());

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
        SIZE,
        SIZE,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &CreateWindowAux::new()
            .background_pixel(color)
            .event_mask(EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::EXPOSURE),
    )?;

    // Advertise WM_CLASS / WM_NAME so xembsni can derive an app id and title.
    conn.change_property8(
        PropMode::REPLACE,
        icon,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"mktray\0MkTray\0",
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        icon,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        title.as_bytes(),
    )?;
    // _XEMBED_INFO: version 0, flags = XEMBED_MAPPED.
    let xembed_info = conn.intern_atom(false, b"_XEMBED_INFO")?.reply()?.atom;
    conn.change_property32(
        PropMode::REPLACE,
        icon,
        xembed_info,
        xembed_info,
        &[0, XEMBED_MAPPED],
    )?;
    conn.flush()?;

    let selection = conn
        .intern_atom(false, format!("_NET_SYSTEM_TRAY_S{screen_num}").as_bytes())?
        .reply()?
        .atom;
    let opcode = conn
        .intern_atom(false, b"_NET_SYSTEM_TRAY_OPCODE")?
        .reply()?
        .atom;

    // Wait for a tray host to be running, then dock.
    let owner = loop {
        let owner = conn.get_selection_owner(selection)?.reply()?.owner;
        if owner != x11rb::NONE {
            break owner;
        }
        eprintln!("no system tray running yet; waiting...");
        thread::sleep(Duration::from_millis(500));
    };

    let dock = ClientMessageEvent::new(
        32,
        owner,
        opcode,
        [x11rb::CURRENT_TIME, SYSTEM_TRAY_REQUEST_DOCK, icon, 0, 0],
    );
    conn.send_event(false, owner, EventMask::NO_EVENT, dock)?;
    conn.flush()?;
    println!("docked icon {icon:#010x} (color {color:06x}, title {title:?}); Ctrl-C to quit");

    loop {
        match conn.wait_for_event()? {
            Event::ButtonPress(ev) => println!("received button {} press", ev.detail),
            Event::ButtonRelease(ev) => println!("received button {} release", ev.detail),
            Event::ClientMessage(_) => {} // e.g. _XEMBED notifications
            _ => {}
        }
    }
}
