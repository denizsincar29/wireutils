//! An icon in the notification area with a menu on it.
//!
//! The owner's ask, in one line: «сделай тогда хрень, которая в трее такая
//! висит, и можно из меню обновить, включить / выключить» — plus «и сама
//! обновляет файл каждые 10 мин».
//!
//! So the icon owns the whole lifecycle of one AmnesiaWG config: it fetches it
//! from the panel, writes the store copy, restarts the tunnel when the bytes
//! changed, and polls every ten minutes so a config edited on the server
//! arrives without anyone touching the client machine.
//!
//! There is no main window. On Windows a tray app with a window is a window
//! that can be closed by mistake and a process that lingers after it; the menu
//! is the whole interface, and `set_exit_on_frame_delete(false)` is what keeps
//! the process alive when wxWidgets thinks it has nothing left to show.
//!
//! The tray is deliberately thin. Everything it does is one call into
//! `wireutils::sync`, the same functions the CLI's `conf-fetch` and `tunnel`
//! commands call — so the behaviour is covered by the library's tests, and the
//! part that only compiles on CI is the part with no logic in it.

use std::cell::RefCell;
use std::rc::Rc;

use wxdragon::prelude::*;

use crate::sync;

/// How often the fetch runs on its own. The owner asked for ten minutes; it
/// is a constant so the menu label and the timer cannot drift apart.
const POLL_MINUTES: i32 = 10;

// Menu ids. Out of the range wxWidgets reserves and stable, because the
// handler matches on them.
const ID_UPDATE_NOW: i32 = 2001;
const ID_TOGGLE_TUNNEL: i32 = 2002;
const ID_SHOW_STORE: i32 = 2003;
const ID_EXIT: i32 = 2004;

/// Everything the menu callbacks share.
struct State {
    /// Recipient name, as served by the panel: `<tunnel>.conf`.
    tunnel: String,
    /// Where the configs come from.
    url: String,
    /// Mirrors whether the tunnel is meant to be up. The tray cannot ask the
    /// service manager cheaply, so it remembers what it last did and says so
    /// in the menu label.
    running: bool,
}

pub fn run(tunnel: String, url: String) -> i32 {
    let state = Rc::new(RefCell::new(State {
        tunnel,
        url,
        running: true,
    }));
    // The startup fetch happens on this side of `main`, because `main` takes a
    // closure and a closure cannot hand a value back out. The icon itself is
    // built inside, so it travels through this cell: `Some` once the fetched
    // result is known. That is also what keeps `set_icon` in one function —
    // `TaskBarIcon` does not implement `Copy`, and every `let x = icon` moves
    // it, so the second one fails to compile.
    let startup: Rc<RefCell<Option<(Rc<TaskBarIcon>, String)>>> = Rc::new(RefCell::new(None));
    // The cell the main loop writes into and this side reads back, and the
    // cell this side keeps. It has to be cloned here: `main` takes a `move`
    // closure, so the handle that travels inside is a second owner, and the
    // original cannot follow it.
    let startup_clone = startup.clone();

    let outcome = wxdragon::main(move |app| {
        // wxWidgets ends the main loop when the last top-level window closes,
        // and this app deliberately has no window at all. Without this the
        // tray would vanish the instant it appeared.
        app.set_exit_on_frame_delete(false);

        let mut menu = Menu::builder()
            // Three arguments, not four: wxdragon's check item has no initial
            // state to give — it is appended unchecked and `check_item` sets
            // the mark. The menu is therefore built unchecked and marked
            // immediately below, before it can be opened.
            .append_check_item(
                ID_TOGGLE_TUNNEL,
                "Туннель включён",
                "Start or stop the tunnel service",
            )
            .append_separator()
            .append_item(
                ID_UPDATE_NOW,
                "Обновить конфиг сейчас",
                "Fetch the config and restart the tunnel if it changed",
            )
            .append_item(
                ID_SHOW_STORE,
                "Где лежит конфиг",
                "Show the path the client reads",
            )
            .append_separator()
            .append_item(ID_EXIT, "Выход", "Quit the tray")
            .build();

        // The tunnel is started on the very first fetch, so the mark belongs
        // on from the beginning. `State::running` starts true and this is what
        // makes the menu agree with it.
        menu.check_item(ID_TOGGLE_TUNNEL, true);

        let icon = Rc::new(
            TaskBarIcon::builder()
                .with_icon_type(TaskBarIconType::CustomStatusItem)
                .build(),
        );

        // The one place an icon is painted. Every caller below goes through
        // it, both because the bitmap choice is the same everywhere and
        // because `set_icon` takes `&self` and needs the handle *unmoved* —
        // handed to a closure by reference, not by value.
        let paint = {
            let icon = icon.clone();
            move |outcome: &str| {
                if let Some(bmp) = &bitmap_for(outcome) {
                    icon.set_icon(bmp, &format!("wireutils — {outcome}"));
                }
            }
        };
        // No icon art ships with wxWidgets that means "a tunnel is up", so the
        // stock art carries the one bit a glance can absorb: this build wanted
        // to have fetched something.
        icon.set_popup_menu(&mut menu);
        // Seed the slot so the icon exists before the first fetch returns.
        // `bitmap_for` reaches for the same stock art, and its fallback is
        // why a missing bitmap here is survivable: the tray shows the icon,
        // just without a picture until the fetch says something.
        if let Some(bmp) = bitmap_for("") {
            icon.set_icon(&bmp, "wireutils");
        }

        // One timer for the lifetime of the app. `Timer` needs an owner that
        // implements `WxEvtHandler`, and the icon is one — but the handle is
        // an `Rc<TaskBarIcon>`, and `Rc<T>` is not itself an event handler.
        // Dereferencing gives `&TaskBarIcon`, which is: the `Rc` stays the
        // owner of the lifetime, the timer only borrows the handler.
        let timer = Timer::new(&*icon);
        {
            let state = state.clone();
            let paint = paint.clone();
            let mut last: Option<String> = None;
            timer.on_tick(move |_| {
                let now = fetch_and_install(&state);
                // A tray app has no scrollback, so the tooltip is the only
                // place a quiet failure can surface. Rewriting it every ten
                // minutes with the same sentence would be worse than useless,
                // so it is written when the verdict changes and not otherwise.
                if last.as_deref() != Some(now.as_str()) {
                    paint(&now);
                    last = Some(now);
                }
            });
        }
        timer.start(POLL_MINUTES * 60 * 1000, false);

        // Fetch once at startup rather than waiting ten minutes for the first
        // one: a recipient double-clicks the binary precisely because the
        // config should be in place now.
        let first = fetch_and_install(&state);
        paint(&first);
        let last = Rc::new(RefCell::new(Some(first.clone())));
        *startup.borrow_mut() = Some((icon.clone(), first));

        {
            let state = state.clone();
            let paint = paint.clone();
            let template = Rc::new(RefCell::new(menu));
            let last = last.clone();
            // Cloned, not moved: `Timer::new(&*icon)` holds a borrow of the
            // binding for as long as the app lives, so the closure below may
            // not take the binding over. The clone is what the menu uses to
            // tear the icon down on exit.
            let mine = icon.clone();
            icon.on_menu(move |event| {
                let settle = |text: String| {
                    paint(&text);
                    *last.borrow_mut() = Some(text.clone());
                    // A menu click is the owner asking a direct question and
                    // it deserves an answer even when nothing changed; a
                    // silent poll is the opposite case and says nothing.
                    if event.get_id() == ID_UPDATE_NOW {
                        answer(&text);
                    }
                };
                match event.get_id() {
                    ID_UPDATE_NOW => settle(fetch_and_install(&state)),
                    ID_TOGGLE_TUNNEL => {
                        let (tunnel, want) = {
                            let mut s = state.borrow_mut();
                            s.running = !s.running;
                            (s.tunnel.clone(), s.running)
                        };
                        let result = if want {
                            sync::restart_tunnel(&tunnel)
                        } else {
                            sync::stop_tunnel(&tunnel).map(|_| "туннель остановлен".to_string())
                        };
                        match result {
                            Ok(what) => {
                                // The template menu is what the popup is
                                // rebuilt from on every open, so the check
                                // mark has to be set here rather than on the
                                // menu already on screen.
                                template
                                    .borrow()
                                    .check_item(ID_TOGGLE_TUNNEL, want);
                                settle(format!("{tunnel}: {what}"));
                            }
                            Err(e) => {
                                // Put the flag back where it was: the thing
                                // the owner asked for did not happen, and the
                                // menu must not go on saying it did.
                                state.borrow_mut().running = !want;
                                settle(format!("{tunnel}: не удалось — {e}"));
                            }
                        }
                    }
                    ID_SHOW_STORE => {
                        settle(format!(
                            "конфиг клиента лежит в {}",
                            sync::config_path(&state.borrow().tunnel)
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|e| e)
                        ));
                    }
                    ID_EXIT => {
                        mine.remove_icon();
                        std::process::exit(0);
                    }
                    other => settle(format!("неизвестный пункт меню: {other}")),
                }
            });
        }
    });

    // `Some` unless the closure panicked before the startup fetch, and the
    // paint is worth doing: the main loop is over, but the notification-area
    // icon outlives it for as long as the process does, and this is the last
    // chance to leave the truth on it.
    // `main` took its own clone of the cell — the `move` closure above cannot
    // borrow from here — so this side still holds the original. Drained and
    // matched by value: an `Rc` is not `Copy`, which is both why the pattern
    // takes it out rather than looking at it, and why the clone below is not
    // optional.
    if let Some((icon, verdict)) = startup_clone.borrow_mut().take() {
        if let Some(bmp) = bitmap_for(&verdict) {
            icon.set_icon(&bmp, &format!("wireutils — {verdict}"));
        }
    }

    match outcome {
        Ok(()) => 0,
        Err(e) => {
            // No console on a GUI-subsystem Windows build, so this cannot be
            // printed anywhere the owner will see; the exit code is the part
            // that survives, and it is what a wrapper script reads.
            let _ = e;
            1
        }
    }
}

/// Fetch, install, and restart the tunnel only if something changed.
///
/// Returns one line describing what happened, in the shape the tooltip and
/// the menu answer both want. A failure to restart a tunnel whose config did
/// change is not the same as a failure to fetch, and the two are worded
/// differently on purpose: the first means the client is running with new
/// bytes and an old tunnel, which the owner has to know.
fn fetch_and_install(state: &Rc<RefCell<State>>) -> String {
    let (tunnel, url) = {
        let s = state.borrow();
        (s.tunnel.clone(), s.url.clone())
    };
    match sync::install(&url, &tunnel) {
        Ok(install) if install.changed => match sync::restart_tunnel(&tunnel) {
            Ok(what) => {
                state.borrow_mut().running = true;
                format!("{tunnel}: обновлён, {what}")
            }
            Err(e) => format!("{tunnel}: обновлён, но туннель не поднялся — {e}"),
        },
        Ok(_) => format!("{tunnel}: без изменений"),
        Err(e) => format!("{tunnel}: ошибка — {e}"),
    }
}

/// The bitmap that matches a verdict, or `None` when the art provider has
/// nothing to give and the icon is better left alone than blanked.
fn bitmap_for(outcome: &str) -> Option<Bitmap> {
    let bad = ["ошибка", "не удалось", "не поднялся"];
    let art = if bad.iter().any(|w| outcome.contains(w)) {
        ArtId::Warning
    } else {
        ArtId::Information
    };
    ArtProvider::get_bitmap(art, ArtClient::Menu, Some(Size::new(16, 16)))
        .or_else(|| ArtProvider::get_bitmap(ArtId::Help, ArtClient::Menu, Some(Size::new(16, 16))))
}

/// A place for a menu answer to be delivered out-of-band, if one is ever
/// wanted. Today it deliberately delivers nothing.
///
/// A direct answer to a menu click does not belong in a balloon: the owner
/// clicked, the popup closed, and a balloon arriving afterwards is a second
/// thing to read — the tooltip already carries the same sentence by the time
/// this is called.
fn answer(_text: &str) {
    // Nothing to do: the caller has already put this same sentence on the
    // icon's tooltip, which is where a tray app's output belongs. Left as a
    // named seam rather than deleted so a future balloon — `show_balloon` on
    // `TaskBarIcon` is the real Windows notification for exactly this — has an
    // obvious place to land.
}
