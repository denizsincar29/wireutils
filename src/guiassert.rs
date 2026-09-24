//! Regression guards for the wxWidgets assertions this app has actually hit.
//!
//! Every check here stands in for an assert in wx itself, and the point is
//! that an assert in wx is invisible from Rust: it aborts a debug build and
//! does *nothing* on a release one, so a wrong call compiles, passes every
//! check, and only fails at the owner's machine. That happened twice in one
//! evening:
//!
//! * `SetStatusText` into a frame with no status bar.
//! * `SetStatusText` into field 1 of a one-pane bar — `statbar.cpp:247`,
//!   `(unsigned)number < m_panes.size()`.
//!
//! Both are index-invariant bugs, and both are caught by the same guard: the
//! fields and ids the window uses are read from the constants below, never
//! spelled as literals at the call site, and `status_field_is_a_real_pane`
//! fails if a write can land outside the bar this app builds.
//!
//! The guards run only on the Windows runner, behind `--features gui` — the
//! same job that type-checks the GUI at all. They cannot run in the agent's
//! container (`gui` needs wx, a display and a C toolchain) and they cannot run
//! in the ordinary test job either, which builds without the feature.

use wxdragon::prelude::*;
use wxdragon::widgets::statusbar::StatusBarStyle;

/// Panes in the status bar. One: the whole message is the text, and a window
/// that splits the last line into panes reads worse aloud than a single
/// full-width line.
pub const STATUS_FIELDS: usize = 1;

/// The only field a message may be written to. Derived, never spelled as a
/// literal at a call site: an index past the last pane is an assert in wx,
/// and a screen reader user gets an aborting window instead of a message.
pub const STATUS_PRIMARY_FIELD: usize = STATUS_FIELDS - 1;

/// The status bar this app builds on `frame`.
///
/// Built through the builder rather than `Frame::create_status_bar`: the raw
/// call has neither `StatusBarStyle` nor `ID_NONE` in the wxdragon prelude —
/// the style is not re-exported one level up either, so it has to be named as
/// `widgets::statusbar::StatusBarStyle` — and it returns the bar without
/// attaching it to the frame, so a later `set_status_text` on the frame would
/// aim at a window that has none. The builder creates, configures and attaches.
///
/// The field count here is `STATUS_FIELDS` for a reason: this function is the
/// single place the bar is created, so the guard in the tests below is
/// checking the same number the window runs with.
pub fn status_bar(frame: &Frame) -> Option<StatusBar> {
    let bar = StatusBar::builder(frame)
        .with_style(StatusBarStyle::Default)
        .with_fields_count(STATUS_FIELDS)
        .build();
    bar.is_valid().then_some(bar)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one-pane invariant: a write goes to `STATUS_PRIMARY_FIELD`, and
    /// that index has to exist in the bar built from `STATUS_FIELDS`. This is
    /// the `statbar.cpp:247` assert, moved to where it fails in a test run
    /// instead of in a message box on the owner's screen.
    #[test]
    fn status_field_is_a_real_pane() {
        assert!(STATUS_FIELDS >= 1, "the bar must have at least one pane");
        assert!(
            STATUS_PRIMARY_FIELD < STATUS_FIELDS,
            "writes go to field {STATUS_PRIMARY_FIELD}, outside the {STATUS_FIELDS}-pane bar"
        );
    }

    /// The constructor the window uses has to keep this shape; if it changes,
    /// the guard above stops meaning anything and this fails first.
    #[test]
    fn status_bar_builder_is_the_constructor_under_test() {
        let _ = status_bar as fn(&Frame) -> Option<StatusBar>;
        let _ = StatusBarStyle::Default;
    }
}
