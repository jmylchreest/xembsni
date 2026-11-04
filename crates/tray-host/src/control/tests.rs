use super::*;
use std::thread;
use std::time::{Duration, Instant};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{ChangeWindowAttributesAux, CreateWindowAux, PropMode, WindowClass};
use x11rb::wrapper::ConnectionExt as _;

fn connect() -> Option<TrayControl> {
    match TrayControl::connect() {
        Ok(control) => Some(control),
        Err(err) => {
            eprintln!("skipping: no X server available ({err})");
            None
        }
    }
}

fn window(conn: &RustConnection, parent: u32, offset: i16, mask: EventMask) -> Result<u32> {
    let id = conn.generate_id()?;
    conn.create_window(
        0,
        id,
        parent,
        offset,
        offset,
        24,
        24,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().event_mask(mask),
    )?
    .check()?;
    conn.map_window(id)?.check()?;
    Ok(id)
}

fn buttons(conn: &RustConnection, count: usize) -> Result<Vec<ButtonPressEvent>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut events = Vec::new();
    while Instant::now() < deadline {
        while let Some(event) = conn.poll_for_event()? {
            match event {
                Event::ButtonPress(event) | Event::ButtonRelease(event) => events.push(event),
                _ => {}
            }
        }
        if events.len() >= count {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(events)
}

#[test]
fn clicks_resolve_each_listener_and_translate_coordinates() -> Result<()> {
    let Some(control) = connect() else {
        return Ok(());
    };
    // A separate client selects events: the injector's your_event_mask is empty.
    let (client, _) = x11rb::connect(None)?;
    let both = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE;
    let icon = window(&client, control.root, 0, both)?;
    let child = window(&client, icon, 4, EventMask::NO_EVENT)?;

    control.activate(icon, 100, 100)?;
    let events = buttons(&client, 2)?;
    assert_eq!(events.len(), 2);
    for event in &events {
        assert_eq!(
            (event.event, event.child, event.event_x, event.event_y),
            (icon, child, 12, 12)
        );
    }

    client
        .change_window_attributes(child, &ChangeWindowAttributesAux::new().event_mask(both))?
        .check()?;
    control.activate(icon, 100, 100)?;
    let events = buttons(&client, 2)?;
    assert_eq!(events.len(), 2);
    for event in &events {
        assert_eq!(
            (event.event, event.child, event.event_x, event.event_y),
            (child, x11rb::NONE, 8, 8)
        );
    }

    client
        .change_window_attributes(
            child,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::BUTTON_PRESS),
        )?
        .check()?;
    control.activate(icon, 100, 100)?;
    let events = buttons(&client, 2)?;
    assert_eq!(events.len(), 2);
    assert_eq!(
        (events[0].event, events[0].event_x, events[0].event_y),
        (child, 8, 8)
    );
    assert_eq!(
        (events[1].event, events[1].event_x, events[1].event_y),
        (icon, 12, 12)
    );
    assert_eq!(
        events[0].response_type & 0x7f,
        x11rb::protocol::xproto::BUTTON_PRESS_EVENT
    );
    assert_eq!(
        events[1].response_type & 0x7f,
        x11rb::protocol::xproto::BUTTON_RELEASE_EVENT
    );
    Ok(())
}

#[test]
fn clicks_respect_do_not_propagate() -> Result<()> {
    let Some(control) = connect() else {
        return Ok(());
    };
    let (client, _) = x11rb::connect(None)?;
    let both = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE;
    let icon = window(&client, control.root, 0, both)?;
    let child = window(&client, icon, 4, EventMask::NO_EVENT)?;
    client
        .change_window_attributes(
            child,
            &ChangeWindowAttributesAux::new().do_not_propogate_mask(both),
        )?
        .check()?;
    control.activate(icon, 100, 100)?;
    assert!(buttons(&client, 1)?.is_empty());
    Ok(())
}

#[test]
fn work_area_tracks_current_desktop_and_handles_invalid_properties() -> Result<()> {
    let Some(mut control) = connect() else {
        return Ok(());
    };
    // Put EWMH properties on a private window instead of modifying the user's root.
    let root = control.conn.generate_id()?;
    control
        .conn
        .create_window(
            0,
            root,
            control.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new(),
        )?
        .check()?;
    control.root = root;
    let whole = (
        0,
        0,
        i32::from(control.screen_size.0),
        i32::from(control.screen_size.1),
    );
    assert_eq!(control.work_area(), whole);
    control.conn.change_property32(
        PropMode::REPLACE,
        root,
        control.workarea_atom,
        AtomEnum::CARDINAL,
        &[0, 0, 1000, 700, 0, 40, 1000, 660],
    )?;
    assert_eq!(control.work_area(), (0, 0, 1000, 700));
    control.conn.change_property32(
        PropMode::REPLACE,
        root,
        control.current_desktop_atom,
        AtomEnum::CARDINAL,
        &[1],
    )?;
    assert_eq!(control.work_area(), (0, 40, 1000, 660));
    assert_eq!(control.clamp_to_work_area(100, 10), (100, 40));
    assert_eq!(control.anchor_point(24, 24), (988, 52));
    for desktop in [0, 1, 2, u32::MAX] {
        control.conn.change_property32(
            PropMode::REPLACE,
            root,
            control.current_desktop_atom,
            AtomEnum::CARDINAL,
            &[desktop],
        )?;
        let expected = match desktop {
            0 => (0, 0, 1000, 700),
            1 => (0, 40, 1000, 660),
            _ => whole,
        };
        assert_eq!(control.work_area(), expected);
    }
    control.conn.change_property8(
        PropMode::REPLACE,
        root,
        control.current_desktop_atom,
        AtomEnum::STRING,
        b"invalid",
    )?;
    assert_eq!(control.work_area(), (0, 0, 1000, 700));
    Ok(())
}
