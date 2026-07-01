# xembsni

**XEmbed → StatusNotifierItem bridge for Wayland.**

Many X11 applications — notably Wine/Proton games and launchers — create a
system-tray icon using the legacy freedesktop **System Tray Protocol** (the
"XEMBED tray"). Modern Wayland bars (waybar, and anything speaking
**StatusNotifierItem**) don't understand that protocol, so those icons either
vanish or leave a stray Wine window lying around.

`xembsni` is a small daemon that:

1. Pretends to be an X11 system tray — it owns the `_NET_SYSTEM_TRAY_S<n>`
   selection and accepts icons that apps try to dock.
2. Republishes each docked icon as a modern **StatusNotifierItem** on D-Bus, so
   it shows up cleanly in `waybar`'s `tray` module (or any SNI host).
3. Proxies interaction — clicks, scrolls, and context menus — back to the
   original icon, and hides the stray Wine window.

It works with anything that speaks the System Tray Protocol, not just Wine.

## Status

The core bridge works end-to-end: docked icons are embedded, captured, and
republished as StatusNotifierItems, and host interactions are forwarded back to
the X11 icon. Verified headlessly (Xvfb + a private D-Bus session) by the test
suite, and by a manual run with the bundled `mktray` helper.

Roadmap:

- [x] **M1 — Tray host spine:** own `_NET_SYSTEM_TRAY_S<n>`, announce `MANAGER`,
  handle `SYSTEM_TRAY_REQUEST_DOCK`.
- [x] **M2 — Embed & publish:** XEMBED docked icons into offscreen containers,
  capture their pixmaps via the Composite + Damage extensions, and export a
  `StatusNotifierItem` per icon (each on its own bus name, so removal is clean).
- [x] **M3 — Interaction:** `Activate` / `SecondaryActivate` / `ContextMenu` /
  `Scroll` are forwarded to the X11 icon as synthetic pointer events. Most
  legacy tray icons pop their own native menu on right-click.
- [~] **M4 — Polish:** done — `_XEMBED_INFO` map/unmap tracking driving SNI
  `Status` (Active/Passive), clean un-dock handling, `--help`/`--version`,
  a `Makefile` installer, and a Wine-like `winetray` test app. Remaining:
  icon-theme/`IconName` lookup, richer tooltips, `com.canonical.dbusmenu`
  passthrough, distro packaging.

### Known limitations

- Interactions are delivered via `XSendEvent`. This works for Wine and most
  toolkits; a few apps that ignore synthetic events may not respond. A future
  option is XTest-based delivery.
- Only 24/32-bpp icon visuals are captured (what tray icons use in practice);
  other depths fall back to a blank image.

## Architecture

Cargo workspace under `crates/`:

| Crate                 | Role |
|-----------------------|------|
| `xembsni`             | Binary/daemon: lifecycle, signals, wiring. |
| `xembsni-tray-host`   | X11 side: owns the tray selection, XEMBED host. |
| `xembsni-sni`         | D-Bus side: publishes `StatusNotifierItem`s (placeholder). |
| `xembsni-bridge`      | Glue: maps X11 icons ↔ SNI items (placeholder). |

Async (Tokio) is used only at I/O boundaries (signals, and later D-Bus); the
X11 event loop is synchronous and runs on its own thread.

## Running

Requires an X server. On Wayland, that's **Xwayland** (started automatically by
most compositors); for headless development, an **Xvfb** display works too.

```sh
cargo run -p xembsni
# increase logging
RUST_LOG=debug cargo run -p xembsni
```

If another system tray already owns the selection on your display, `xembsni`
exits immediately rather than fighting over it.

### Trying it without a Wine app

Two example clients dock into a running `xembsni` so you can test the daemon
and your bar without Wine:

- **`mktray`** — the minimal case: a solid-colour icon that prints forwarded
  clicks.
- **`winetray`** — a faithful stand-in for a Wine/Proton tray app: it paints a
  real icon, sets `_XEMBED_INFO`, re-docks if the tray restarts, and pops its
  own native menu on right-click (just like Wine does).

```sh
# Terminal 1
cargo run -p xembsni
# Terminal 2 — pick one:
cargo run -p xembsni-tray-host --example mktray -- 00ff8800 "My Fake Icon"
cargo run -p xembsni-tray-host --example winetray -- "Fake Wine App" --blink
```

The icon should appear in your bar's tray; left-clicking prints in the example's
terminal, and right-clicking `winetray` opens its menu.

### Options

```
xembsni --help       # usage
xembsni --version    # version
```

Logging verbosity is controlled by `RUST_LOG` (e.g. `RUST_LOG=debug`).

## Testing

The suite runs headlessly against a throwaway X server and D-Bus session:

```sh
# X-only tests (embedding, capture, pixel conversion)
xvfb-run -a cargo test -p xembsni-tray-host

# Everything, including the full publish + interaction end-to-end test
xvfb-run -a dbus-run-session -- cargo test --workspace
```

Tests that need an X server or bus **skip cleanly** when one isn't present, so
`cargo test` stays green in a bare environment too. The end-to-end test stands
up a fake `StatusNotifierWatcher`, docks a fake icon, and asserts it's published
with the right pixmap and that `Activate`/`ContextMenu` reach the X11 window.

### As a user systemd service

Intended to run as a user service under compositors like niri and Hyprland.
The `Makefile` installs the binary and unit for you:

```sh
make install                 # -> ~/.local/bin + ~/.config/systemd/user
systemctl --user daemon-reload
systemctl --user enable --now xembsni.service
journalctl --user -u xembsni -f
```

Or by hand:

```sh
cargo build --release
install -Dm755 target/release/xembsni ~/.local/bin/xembsni
install -Dm644 contrib/systemd/xembsni.service \
  ~/.config/systemd/user/xembsni.service
```

The unit is bound to `graphical-session.target` so it starts and stops with your
compositor.

## Prior art

KDE Plasma ships [`xembed-sni-proxy`][xesp], which solves the same problem
inside a Plasma session. `xembsni` is an independent, compositor-agnostic
implementation aimed at bare wlroots/niri/Hyprland setups. It was written from
the freedesktop [System Tray][systray-spec] and [XEMBED][xembed-spec]
specifications; `xembed-sni-proxy` was consulted as design inspiration only —
no code was copied.

[xesp]: https://invent.kde.org/plasma/plasma-workspace/tree/master/xembed-sni-proxy
[systray-spec]: https://specifications.freedesktop.org/systemtray-spec/
[xembed-spec]: https://specifications.freedesktop.org/xembed-spec/

## License

Dual-licensed under either MIT or Apache-2.0, at your option.
