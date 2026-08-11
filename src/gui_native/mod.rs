//! Native Win32 tray UI, built on winsafe.
//!
//! Replaces the wxWidgets GUI in `gui/`: same behaviour, no bundled toolkit.
//! Lives alongside it until every dialog is ported, because wx and winsafe each
//! want to own the thread's message loop and cannot both drive one.

pub mod tooltip;
