//! `winetray` — a faux legacy XEmbed tray app, mimicking how a Wine/Proton
//! program presents a notification-area icon. Use it to exercise xembsni
//! without installing anything under Wine.
//!
//! Unlike the bare-bones `mktray` example, this one behaves like a real client:
//!
//! - it **paints** its icon on `Expose` (a coloured disc, optionally a glyph),
//!   so xembsni's Composite/Damage capture sees genuine, changing content;
//! - it sets `_XEMBED_INFO` / `WM_CLASS` / `WM_NAME` and logs `_XEMBED` messages;
//! - it **re-docks when a tray appears** (the `MANAGER` announcement), exactly
//!   as Wine does if the tray restarts;
//! - on **right-click it pops its own override-redirect menu** — the way Wine
//!   tray icons do (they don't speak dbusmenu) — so you can watch xembsni's
//!   `ContextMenu` forwarding make the app's real menu appear.
//!
//! ```sh
//! cargo run -p xembsni-tray-host --example winetray -- "Fake Wine App" --blink
//! ```
//! Left-click prints to stdout; right-click opens the menu; "Quit" exits.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Arc as XArc, AtomEnum, ChangeGCAux, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _,
    CreateGCAux, CreateWindowAux, EXPOSE_EVENT, EventMask, ExposeEvent, PropMode, Rectangle,
    Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

type Res<T = ()> = Result<T, Box<dyn std::error::Error>>;

const SYSTEM_TRAY_REQUEST_DOCK: u32 = 0;
const XEMBED_MAPPED: u32 = 1;
const ICON: u16 = 22; // typical Wine tray icon size
const MENU_W: u16 = 140;
const MENU_H: u16 = 52;

/// A rotating palette so `--blink` produces visibly different frames.
const PALETTE: [u32; 4] = [0x00e0_4030, 0x0030_a0e0, 0x0040_c060, 0x00e0_b020];

fn main() -> Res {
    let mut title = "winetray".to_string();
    let mut blink = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--blink" => blink = true,
            other => title = other.to_string(),
        }
    }

    let (conn, screen_num) = x11rb::connect(None)?;
    let conn = Arc::new(conn);
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let visual = screen.root_visual;
    let white = screen.white_pixel;
    let black = screen.black_pixel;

    // Atoms a real client needs.
    let selection = conn
        .intern_atom(false, format!("_NET_SYSTEM_TRAY_S{screen_num}").as_bytes())?
        .reply()?
        .atom;
    let opcode = conn
        .intern_atom(false, b"_NET_SYSTEM_TRAY_OPCODE")?
        .reply()?
        .atom;
    let manager = conn.intern_atom(false, b"MANAGER")?.reply()?.atom;
    let xembed = conn.intern_atom(false, b"_XEMBED")?.reply()?.atom;
    let xembed_info = conn.intern_atom(false, b"_XEMBED_INFO")?.reply()?.atom;

    // The icon window. We do NOT map it — the tray host does, once embedded.
    let icon = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        icon,
        root,
        0,
        0,
        ICON,
        ICON,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &CreateWindowAux::new().background_pixel(black).event_mask(
            EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::STRUCTURE_NOTIFY,
        ),
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        icon,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"winetray\0WineTray\0",
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        icon,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        title.as_bytes(),
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        icon,
        xembed_info,
        xembed_info,
        &[0, XEMBED_MAPPED],
    )?;

    // Listen for MANAGER announcements so we can (re-)dock when a tray appears.
    conn.change_window_attributes(
        root,
        &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
            .event_mask(EventMask::STRUCTURE_NOTIFY),
    )?;

    // A graphics context for painting. A font is optional — headless servers
    // may lack one, so we degrade gracefully to shapes only.
    let gc = conn.generate_id()?;
    conn.create_gc(
        gc,
        icon,
        &CreateGCAux::new().foreground(white).background(black),
    )?;
    let font = conn.generate_id()?;
    let has_font = conn.open_font(font, b"fixed").is_ok()
        && conn.change_gc(gc, &ChangeGCAux::new().font(font)).is_ok();

    // The context menu window, created up front and mapped on right-click.
    let menu = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        menu,
        root,
        0,
        0,
        MENU_W,
        MENU_H,
        1,
        WindowClass::INPUT_OUTPUT,
        visual,
        &CreateWindowAux::new()
            .override_redirect(1)
            .background_pixel(white)
            .border_pixel(black)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS),
    )?;

    conn.flush()?;
    try_dock(&conn, selection, opcode, icon)?;
    println!("winetray {icon:#010x} ({title:?}); left-click prints, right-click opens a menu");

    // Optional animation: a helper thread nudges us to repaint periodically.
    let frame = Arc::new(AtomicU32::new(0));
    if blink {
        spawn_blinker(conn.clone(), icon, frame.clone());
    }

    loop {
        match conn.wait_for_event()? {
            Event::Expose(ev) if ev.window == icon => {
                draw_icon(&conn, icon, gc, has_font, frame.load(Ordering::Relaxed))?;
            }
            Event::Expose(ev) if ev.window == menu => {
                draw_menu(&conn, menu, gc, black, white, has_font)?;
            }
            Event::ButtonPress(ev) if ev.event == icon => match ev.detail {
                1 => println!("left click"),
                2 => println!("middle click"),
                3 => show_menu(&conn, menu, ev.root_x, ev.root_y)?,
                4 | 5 => println!("scroll {}", if ev.detail == 4 { "up" } else { "down" }),
                _ => {}
            },
            Event::ButtonPress(ev) if ev.event == menu => {
                // Bottom half is "Quit".
                if ev.event_y as u16 > MENU_H / 2 {
                    println!("menu: Quit");
                    return Ok(());
                }
                println!("menu: Hello");
                conn.unmap_window(menu)?;
                conn.flush()?;
            }
            Event::ClientMessage(ev)
                if ev.type_ == manager && ev.data.as_data32()[1] == selection =>
            {
                println!("a system tray appeared; re-docking");
                try_dock(&conn, selection, opcode, icon)?;
            }
            Event::ClientMessage(ev) if ev.type_ == xembed => {
                eprintln!("received _XEMBED message opcode {}", ev.data.as_data32()[1]);
            }
            _ => {}
        }
    }
}

/// Find the tray selection owner and send a dock request, if a tray is running.
fn try_dock(conn: &impl Connection, selection: u32, opcode: u32, icon: Window) -> Res {
    let owner = conn.get_selection_owner(selection)?.reply()?.owner;
    if owner == x11rb::NONE {
        eprintln!("no system tray running yet; will dock when one appears");
        return Ok(());
    }
    let dock = ClientMessageEvent::new(
        32,
        owner,
        opcode,
        [x11rb::CURRENT_TIME, SYSTEM_TRAY_REQUEST_DOCK, icon, 0, 0],
    );
    conn.send_event(false, owner, EventMask::NO_EVENT, dock)?;
    conn.flush()?;
    Ok(())
}

fn spawn_blinker(
    conn: Arc<impl Connection + Send + Sync + 'static>,
    icon: Window,
    frame: Arc<AtomicU32>,
) {
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_millis(700));
            frame.fetch_add(1, Ordering::Relaxed);
            let expose = ExposeEvent {
                response_type: EXPOSE_EVENT,
                sequence: 0,
                window: icon,
                x: 0,
                y: 0,
                width: ICON,
                height: ICON,
                count: 0,
            };
            if conn
                .send_event(false, icon, EventMask::EXPOSURE, expose)
                .is_err()
            {
                break;
            }
            let _ = conn.flush();
        }
    });
}

fn draw_icon(conn: &impl Connection, icon: Window, gc: u32, has_font: bool, frame: u32) -> Res {
    let bg = PALETTE[frame as usize % PALETTE.len()];
    conn.change_gc(gc, &ChangeGCAux::new().foreground(bg))?;
    conn.poly_fill_rectangle(
        icon,
        gc,
        &[Rectangle {
            x: 0,
            y: 0,
            width: ICON,
            height: ICON,
        }],
    )?;
    // A white disc as the "logo".
    conn.change_gc(gc, &ChangeGCAux::new().foreground(0x00ff_ffff))?;
    conn.poly_fill_arc(
        icon,
        gc,
        &[XArc {
            x: 3,
            y: 3,
            width: ICON - 6,
            height: ICON - 6,
            angle1: 0,
            angle2: 360 * 64,
        }],
    )?;
    if has_font {
        conn.change_gc(gc, &ChangeGCAux::new().foreground(0))?;
        conn.image_text8(icon, gc, 8, (ICON / 2 + 4) as i16, b"W")?;
    }
    conn.flush()?;
    Ok(())
}

fn show_menu(conn: &impl Connection, menu: Window, x: i16, y: i16) -> Res {
    conn.configure_window(menu, &ConfigureWindowAux::new().x(x as i32).y(y as i32))?;
    conn.map_window(menu)?;
    conn.flush()?;
    Ok(())
}

fn draw_menu(
    conn: &impl Connection,
    menu: Window,
    gc: u32,
    black: u32,
    white: u32,
    has_font: bool,
) -> Res {
    conn.change_gc(gc, &ChangeGCAux::new().foreground(white))?;
    conn.poly_fill_rectangle(
        menu,
        gc,
        &[Rectangle {
            x: 0,
            y: 0,
            width: MENU_W,
            height: MENU_H,
        }],
    )?;
    conn.change_gc(gc, &ChangeGCAux::new().foreground(black))?;
    // A divider between the two items.
    conn.poly_fill_rectangle(
        menu,
        gc,
        &[Rectangle {
            x: 0,
            y: (MENU_H / 2) as i16,
            width: MENU_W,
            height: 1,
        }],
    )?;
    if has_font {
        conn.image_text8(menu, gc, 10, 18, b"Hello")?;
        conn.image_text8(menu, gc, 10, (MENU_H / 2 + 18) as i16, b"Quit")?;
    }
    conn.flush()?;
    Ok(())
}
