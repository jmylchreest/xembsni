//! X11 atoms interned once at startup.

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        MANAGER,
        _NET_SYSTEM_TRAY_OPCODE,
        _NET_SYSTEM_TRAY_ORIENTATION,
        _NET_SYSTEM_TRAY_VISUAL,
        _NET_SYSTEM_TRAY_MESSAGE_DATA,
        _XEMBED,
        _XEMBED_INFO,
        _NET_WM_NAME,
        UTF8_STRING,
        // Private atom used to wake the blocking event loop for shutdown.
        _XEMBSNI_WAKEUP,
    }
}
