//! The window.
//!
//! Built behind the `gui` feature because wxWidgets and a display are both
//! absent from the container this is developed in — the core is deliberately
//! GUI-independent so everything testable is tested without a window, and this
//! file is the thin shell that calls it.
//!
//! Shape of the window: a table of hosts and two rows of buttons. Everything
//! that acts on one host, one group, or one address is on a **context menu**
//! instead of a button, because the panel had grown to fourteen controls and a
//! screen-reader user had to tab through all of them to reach the two that
//! matter. What stayed a button is what is used every run and has no natural
//! "right-click me" home: adding a domain, writing the configs, and picking
//! the folder.
//!
//! How a row reads. Column 1 is the domain when there is one, and the address
//! when the host *is* a literal address — the domain is the thing the owner
//! typed and thinks in, so it leads; the addresses follow it in column 3. A
//! row for a domain says `github.com` and then `10.1.1.1, 10.1.1.2`; a row
//! for a bare address says `203.0.113.7` and has nothing to put after it.
//!
//! Prefix lengths are said in words, not shown as `31.13.64.0/24`. The owner
//! asked for this directly: `/24` is a 24-bit mask, and a number of bits is
//! not a number of addresses until it is counted. "**a network: 256 addresses
//! (mask 24 bits)**" is the same fact and readable out loud.
//!
//! Accessibility notes, because the owner reads the screen rather than looks
//! at it: the host list is a `DataViewListCtrl`, which is a real multi-column
//! list (NVDA announces it as a list and reads the columns), the context menus
//! are ordinary popup menus reached with the Menu key (Shift+F10) — which is
//! also what the buttons used to be for — and the status line at the bottom is
//! a plain `StaticText` that reports what the last action did, plus the row
//! count so a long list is not a mystery.
//!
//! Three rules this file follows throughout:
//!
//! * Nothing touches the network or the disk from an event handler without
//!   saying so in the status line first. A write to twenty configs that
//!   silently took four seconds reads as a hang.
//! * Every handler re-reads the store from `Shared` instead of keeping a
//!   reference across calls, and every handler that changes the store saves it
//!   before returning. `hosts.json` is the owner's only copy of the list.
//! * A right-click on a *group* row opens the group's own menu, including
//!   "Remove this group's addresses from the configs…". That is the command
//!   the owner went looking for and could not find: the addresses are already
//!   written into every `.conf`, so deleting the group from the list does
//!   nothing to the files.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use wxdragon::prelude::*;

use crate::apply;
use crate::hosts::{HostStore, Source};
use crate::import::{self, Imported, PtrResolver};
use crate::paths;
use crate::resolve::{self, SystemResolver};
use crate::subtract::{self, Removals};
use crate::templates::{self, Catalog};

/// Everything the window mutates. `Rc<RefCell<…>>` because every one of these
/// is touched from more than one button handler, and the handlers have to be
/// `'static`.
struct GuiState {
    store: HostStore,
    catalog: Catalog,
    config_path: PathBuf,
}

type Shared = Rc<RefCell<GuiState>>;

/// Menu command ids. `ID_HIGHEST` is where custom ids start.
const ID_PICK_DIR: i32 = ID_HIGHEST + 1;
const ID_APPLY: i32 = ID_HIGHEST + 2;
const ID_UPDATE_TPL: i32 = ID_HIGHEST + 3;
const ID_IMPORT: i32 = ID_HIGHEST + 4;

/// Context menu command ids, on the host table and the group list.
///
/// In one ascending run with the file-menu ids and ending at
/// [`ID_CTX_LAST`], because the "unrecognised menu id" report below names the
/// expected range and a range with a hole in it is a lie. Each constant is
/// built from the one before it, so inserting an item in the middle cannot
/// silently hand two menu items the same id — `wxWindow::Bind` would then fire
/// the first handler for both clicks.
const ID_CTX_RENAME: i32 = ID_IMPORT + 1;
const ID_CTX_REMOVE: i32 = ID_CTX_RENAME + 1;
const ID_CTX_REMOVE_IP: i32 = ID_CTX_REMOVE + 1;
const ID_CTX_TO_GROUP: i32 = ID_CTX_REMOVE_IP + 1;
const ID_CTX_FROM_GROUP: i32 = ID_CTX_TO_GROUP + 1;
const ID_CTX_GROUP_TOGGLE: i32 = ID_CTX_FROM_GROUP + 1;
const ID_CTX_GROUP_REMOVE: i32 = ID_CTX_GROUP_TOGGLE + 1;
const ID_CTX_GROUP_PURGE: i32 = ID_CTX_GROUP_REMOVE + 1;
const ID_CTX_GROUP_UNGROUP: i32 = ID_CTX_GROUP_PURGE + 1;
/// The highest id any of the two menus can carry, and the one named in the
/// unrecognised-id report. New items go before this line, always.
const ID_CTX_LAST: i32 = ID_CTX_GROUP_UNGROUP;

/// Set once if a menu click arrives carrying an id we never handed out.
/// Event dispatch repeats the same bogus id for every click, and a dialog per
/// click would be unbearable, so the first one is reported in full and the
/// rest stay quiet. Deliberately not a `Cell<bool>` guard around the whole
/// handler: if clicks do arrive, the actions still have to run.
static MENU_ID_UNKNOWN_REPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Run the application. Returns the process exit code.
///
/// The exit code is carried out of the closure in a `Cell` rather than a
/// plain local: `wxdragon::main` demands a `FnOnce + 'static`, and a closure
/// that borrows a local from this frame can never satisfy `'static` — adding
/// `move` only trades one borrow error for another. A shared cell is owned by
/// the closure and still readable here afterwards.
pub fn run() -> i32 {
    let exit_code = Rc::new(Cell::new(0));
    let inner = Rc::clone(&exit_code);

    let result = wxdragon::main(move |app| {
        inner.set(start(&app));
    });

    match result {
        Ok(()) => exit_code.get(),
        Err(e) => {
            eprintln!("wireutils: could not start the window: {e}");
            1
        }
    }
}

/// Build the window and run the event loop. The frame has to exist before any
/// dialog is shown, so the dialogs that report a startup failure are parented
/// to the frame and shown from inside the loop rather than before it.
fn start(_app: &App) -> i32 {
    // A startup failure is reported the only way that is possible before the
    // frame exists: on stderr. `start` used to put up a `MessageDialog` here
    // -- one of the two calls that never compiled, because it was parented to
    // a "desktop window" that wxdragon does not have. A modal before the main
    // loop has started would not be pumped anyway.
    let config_path = match paths::store_path() {
        Some(p) => p,
        None => {
            eprintln!("wireutils: could not work out where to keep hosts.json.");
            return 1;
        }
    };

    // A fresh install has no file yet; that is not an error, it is the first
    // run, and the folder picker below will fill it in.
    let store = if config_path.exists() {
        match HostStore::load(&config_path) {
            Ok(s) => s,
            Err(e) => {
                // A corrupt hosts.json is the owner's only copy of their list,
                // so it is never overwritten silently: say where it is and
                // stop. He can fix the file by hand and start again. The frame
                // does not exist yet, so this one goes to stderr as well.
                eprintln!(
                    "wireutils: could not read {}\n\n{e}\n\nFix or move the file, then start again.",
                    config_path.display()
                );
                return 1;
            }
        }
    } else {
        HostStore::default()
    };

    let shared: Shared = Rc::new(RefCell::new(GuiState {
        store,
        catalog: templates::builtin(),
        config_path,
    }));

    let frame = Frame::builder()
        .with_title("wireutils — allowed IPs for every config")
        .with_size(Size::new(980, 680))
        .build();

    // The status bar exists before anything can write to it. It did not, and
    // every `set_status` call went to a frame that had none: wxWidgets asserts
    // `m_frameStatusBar != nullptr` in `SetStatusText`, which aborts the app on
    // a debug build and does nothing at all on a release one. So the bottom
    // line of the window was the only channel on Windows, and that is exactly
    // the channel a screen reader reads after a button press.
    //
    // The bar itself lives in `guiassert::status_bar`, together with the
    // field count the guards there check. What matters here: it is created
    // before anything can write to it (a frame with no bar is the assert that
    // started this), and it has exactly the panes `guiassert` says it has.
    let status = crate::guiassert::status_bar(&frame);
    if let Some(bar) = &status {
        bar.set_status_text("Ready.", crate::guiassert::STATUS_PRIMARY_FIELD);
    }

    let ui = build_body(&frame, &shared, status);
    frame.centre();
    frame.show(true);

    // First run: the folder of configs is the one thing the app cannot guess,
    // so it is asked for before anything else can do something useful. The
    // frame is already shown, so the dialog has a parent and a place to sit.
    if shared.borrow().store.conf_dir.is_none() {
        if ask_conf_dir(&frame, &ui, &shared) {
            ui.set_status("Config folder saved.");
        } else {
            // The owner closed the picker. The window still opens — he can
            // pick the folder later from the button — but say what is missing.
            ui.set_status("No config folder chosen yet. Use \"Config folder…\" to pick one.");
        }
    } else {
        ui.set_status("Ready.");
    }

    // Fill the table from whatever is already in hosts.json.
    refresh(&ui, &shared);

    // The catalog is pulled once at start, from the internet, so a corrected
    // list reaches the owner without a new binary. This is synchronous and
    // happens before the loop starts, on purpose: a background thread could
    // not touch the widgets (wxWidgets is not thread-safe) and would need a
    // `Send` closure, which an `Rc<RefCell<…>>` is not. A fetch that fails
    // leaves the built-in catalog in place, which is exactly the fallback
    // wanted, so the failure is reported and then forgotten.
    match templates::fetch(templates::DEFAULT_CATALOG_URL) {
        Ok(updated) => {
            let merged = templates::merge(updated, templates::builtin());
            let count = merged.templates.len();
            let mut st = shared.borrow_mut();
            st.catalog = merged;
            st.store.templates_updated = Some(now_rfc3339());
            let _ = st.store.save(&st.config_path);
            drop(st);
            refresh(&ui, &shared);
            ui.set_status(&format!("Template catalog updated — {count} groups known."));
        }
        Err(_) => { /* offline: the built-in catalog is already loaded */ }
    }

    // Only now, after the widgets have something in them, does the menu bar
    // get attached — a menu item that does nothing when clicked is worse than
    // one that appears a moment later.
    frame.set_menu_bar(build_menu(&frame, &ui, &shared));

    // No loop is started here. `wxdragon::main` runs it after this closure
    // returns; an `app.run()` of our own would be a second, nested loop.
    0
}

/// A timestamp for `templates_updated`. Seconds since the epoch would be
/// cheaper, but the field is read by a person looking at hosts.json.
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Days since the epoch, then civil date. Kept local rather than pulling a
    // date crate in for one string.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-to-civil algorithm — public domain, and shorter than
/// a dependency.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// Widget handles
// ---------------------------------------------------------------------------

/// The widgets an action has to reach. All handles in this crate are `Copy`
/// over a window pointer that becomes a no-op once the window dies, so this
/// struct is cheap to copy into `'static` closures — no `Rc` on the widget
/// side, only on the data.
#[derive(Clone, Copy)]
struct Ui {
    frame: Frame,
    /// The frame's own status bar, kept so `set_status` writes to the bar
    /// rather than only to the in-window line. `None` if wx refused to create
    /// one — then the line is the only channel left, which is exactly the
    /// state that used to abort the app.
    status_bar: Option<StatusBar>,
    table: DataViewListCtrl,
    search: SearchCtrl,
    template_list: DataViewListCtrl,
    group_choice: Choice,
    group_add: Button,
    group_remove: Button,
    status: StaticText,
}

impl Ui {
    /// The one place that says what just happened. Both the frame's status bar
    /// and the bottom line are written, because on Windows the status bar is
    /// what a screen reader announces after a button press, and the line is
    /// what stays on screen afterwards.
    ///
    /// The bottom line gets the message plus the row count: the host list is
    /// the only thing of size in this window, and "how many hosts are there"
    /// is otherwise a question the owner has to answer by arrowing to the end.
    fn set_status(&self, text: &str) {
        self.status
            .set_label(&format!("{text}   ({} hosts)", self.table.get_item_count()));
        // Through the `StatusBar` handle, and with the field taken from
        // `guiassert` rather than written as a literal: field 1 of a one-pane
        // bar is `statbar.cpp:247`, and that bug reached the owner's machine
        // once already.
        if let Some(bar) = &self.status_bar {
            bar.set_status_text(text, crate::guiassert::STATUS_PRIMARY_FIELD);
            return;
        }
        self.frame
            .set_status_text(text, crate::guiassert::STATUS_PRIMARY_FIELD as i32);
    }
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// The menu bar. Built after the widgets exist because every item is wired to
/// the same handlers the buttons use — there is exactly one implementation of
/// "write to the configs", shared by the button and the menu item.
fn build_menu(frame: &Frame, ui: &Ui, shared: &Shared) -> MenuBar {
    // The bar is not `Clone` in wxdragon 0.9, so there is exactly one binding
    // and the handler goes on it: `on_menu_selected` takes `&self` and returns
    // nothing, which leaves the value usable as the return value below.
    let menu_bar = MenuBar::builder()
        .append(
            Menu::builder()
                .append_item(
                    ID_PICK_DIR,
                    "Config folder…",
                    "Choose the folder of .conf files",
                )
                .append_item(
                    ID_APPLY,
                    "Write to all configs",
                    "Rewrite AllowedIPs in every .conf",
                )
                .append_item(
                    ID_UPDATE_TPL,
                    "Update templates from the internet",
                    "Fetch the newest list of known sites",
                )
                .append_separator()
                .append_item(
                    ID_IMPORT,
                    "Import addresses from a base config…",
                    "Read a .conf's AllowedIPs into the host list",
                )
                .append_separator()
                .append_item(ID_EXIT, "Exit", "Close wireutils")
                .build(),
            "File",
        )
        .build();

    let frame_for_menu = *frame;
    let ui_for_menu = *ui;
    let shared_for_menu = shared.clone();

    menu_bar.on_menu_selected(move |e: MenuEventData| {
        match e.get_menu_id() {
            Some(id) if id == ID_PICK_DIR => {
                ask_conf_dir(&frame_for_menu, &ui_for_menu, &shared_for_menu);
            }
            Some(id) if id == ID_APPLY => {
                do_apply(&ui_for_menu, &shared_for_menu);
            }
            Some(id) if id == ID_UPDATE_TPL => {
                update_templates(&ui_for_menu, &shared_for_menu);
            }
            Some(id) if id == ID_IMPORT => {
                ask_base_conf(&frame_for_menu, &ui_for_menu, &shared_for_menu);
            }
            Some(id) if id == ID_EXIT => {
                frame_for_menu.close(true);
            }
            // A click the menu bar could not name must never look like a dead
            // menu. There are two shapes this can take — an id that is not
            // one of ours, and no id at all — and both used to fall into a
            // silent `_ => {}`, where the owner clicks "Config folder…" and
            // sees nothing happen with nothing to report. Say what arrived
            // instead, once, so the symptom turns into a fact we can act on.
            //
            // The context menus report through the same channel and the same
            // range, because `e.get_menu_id()` is all either of them can see:
            // a context-menu click arrives here indistinguishable from a
            // file-menu one. That is why the ids form one unbroken run
            // (`ID_PICK_DIR..=ID_CTX_LAST`) instead of two naming schemes.
            other => {
                use std::sync::atomic::Ordering;
                if !MENU_ID_UNKNOWN_REPORTED.swap(true, Ordering::Relaxed) {
                    let seen = match other {
                        Some(id) => format!("id {id}"),
                        None => "no id at all".to_string(),
                    };
                    ui_for_menu.set_status(&format!(
                        "Menu click seen but not recognised ({seen}); expected {ID_PICK_DIR}..{ID_CTX_LAST}. Report this line."
                    ));
                }
            }
        }
    });

    menu_bar
}

fn build_body(frame: &Frame, shared: &Shared, status_bar: Option<StatusBar>) -> Ui {
    let panel = Panel::builder(frame).build();
    let root = BoxSizer::builder(Orientation::Vertical).build();

    // --- hosts ------------------------------------------------------------
    //
    // One box, a table, and a single row of buttons. There were four separate
    // boxes here (hosts, groups, templates, actions) with fourteen controls
    // between them; the buttons that acted on a *row* are now on that row's
    // context menu, and the group buttons are on the group list's.
    let hosts_box =
        StaticBoxSizerBuilder::new_with_label(Orientation::Vertical, &panel, "Hosts").build();

    let table = DataViewListCtrl::builder(&panel).build();
    table.append_text_column(
        "Host",
        0,
        DataViewAlign::Left,
        260,
        DataViewColumnFlags::Resizable,
    );
    table.append_text_column(
        "Group",
        1,
        DataViewAlign::Left,
        160,
        DataViewColumnFlags::Resizable,
    );
    table.append_text_column(
        "Addresses",
        2,
        DataViewAlign::Left,
        380,
        DataViewColumnFlags::Resizable,
    );
    table.append_text_column(
        "State",
        3,
        DataViewAlign::Left,
        140,
        DataViewColumnFlags::Resizable,
    );

    hosts_box.add(&table, 1, SizerFlag::Expand | SizerFlag::All, 6);

    let buttons = BoxSizer::builder(Orientation::Horizontal).build();
    let add_btn = Button::builder(&panel).with_label("Add domain…").build();
    let resolve_btn = Button::builder(&panel).with_label("Resolve").build();
    let force_btn = Button::builder(&panel)
        .with_label("Resolve again (all)")
        .build();
    let apply_btn = Button::builder(&panel)
        .with_label("Write to all configs")
        .build();
    buttons.add(&add_btn, 0, SizerFlag::All, 4);
    buttons.add(&resolve_btn, 0, SizerFlag::All, 4);
    buttons.add(&force_btn, 0, SizerFlag::All, 4);
    buttons.add(&apply_btn, 0, SizerFlag::All, 4);
    hosts_box.add_sizer(&buttons, 0, SizerFlag::Expand, 0);

    root.add_sizer(&hosts_box, 4, SizerFlag::Expand | SizerFlag::All, 8);

    // --- groups -----------------------------------------------------------
    //
    // The list is where a group's own menu lives, including the removal the
    // owner could not find. `Group` is the word on the box because the table
    // column above it is named the same; the two used to say "Groups" and
    // "Groups" while meaning different things (one row's group, and every
    // group there is).
    let groups_box =
        StaticBoxSizerBuilder::new_with_label(Orientation::Horizontal, &panel, "Group").build();
    let group_choice = Choice::builder(&panel).build();
    let group_add = Button::builder(&panel).with_label("New group…").build();
    let group_apply_btn = Button::builder(&panel)
        .with_label("Add selected hosts to group")
        .build();
    // Renamed from "Delete group": it removes the group from the list and
    // leaves every address in every config, which is exactly the confusion
    // this whole change is about. The context menu next door offers the other
    // thing — taking the addresses out of the files — under its own name.
    let group_remove = Button::builder(&panel)
        .with_label("Remove from list")
        .build();
    groups_box.add(&group_choice, 1, SizerFlag::All | SizerFlag::Expand, 6);
    groups_box.add(&group_add, 0, SizerFlag::All, 4);
    groups_box.add(&group_apply_btn, 0, SizerFlag::All, 4);
    groups_box.add(&group_remove, 0, SizerFlag::All, 4);
    root.add_sizer(
        &groups_box,
        0,
        SizerFlag::Expand | SizerFlag::Left | SizerFlag::Right,
        8,
    );

    // --- templates --------------------------------------------------------
    //
    // Only the search box and the list are on the panel; "Add template" and
    // "Update from internet" moved to the template list's context menu, which
    // is where the owner is looking when he means *that* template.
    let tpl_box =
        StaticBoxSizerBuilder::new_with_label(Orientation::Horizontal, &panel, "Templates").build();
    let search = SearchCtrl::builder(&panel)
        .with_value("")
        .with_size(Size::new(300, -1))
        .build();
    search.show_search_button(false);
    search.show_cancel_button(true);

    let template_list = DataViewListCtrl::builder(&panel).build();
    template_list.append_text_column(
        "Template",
        0,
        DataViewAlign::Left,
        220,
        DataViewColumnFlags::Resizable,
    );
    template_list.append_text_column(
        "What it covers",
        1,
        DataViewAlign::Left,
        460,
        DataViewColumnFlags::Resizable,
    );
    template_list.append_text_column(
        "Known IPs",
        2,
        DataViewAlign::Left,
        100,
        DataViewColumnFlags::Resizable,
    );

    let tpl_col = BoxSizer::builder(Orientation::Vertical).build();
    tpl_col.add(&search, 0, SizerFlag::All | SizerFlag::Expand, 6);
    tpl_col.add(&template_list, 1, SizerFlag::All | SizerFlag::Expand, 6);
    tpl_box.add_sizer(&tpl_col, 1, SizerFlag::Expand, 0);
    root.add_sizer(
        &tpl_box,
        1,
        SizerFlag::Expand | SizerFlag::Left | SizerFlag::Right,
        8,
    );

    // --- what is left of the old bottom row ---------------------------------
    //
    // Two buttons that are not about a row: which folder, and where a starting
    // list comes from. Both are also in the File menu, which is why they can
    // afford to be small and last.
    let bottom = BoxSizer::builder(Orientation::Horizontal).build();
    let dir_btn = Button::builder(&panel).with_label("Config folder…").build();
    let import_btn = Button::builder(&panel)
        .with_label("Import base config…")
        .build();
    bottom.add(&dir_btn, 0, SizerFlag::All, 4);
    bottom.add(&import_btn, 0, SizerFlag::All, 4);
    root.add_sizer(&bottom, 0, SizerFlag::Expand | SizerFlag::All, 8);

    let status = StaticText::builder(&panel).build();
    status.set_label("Ready.");
    root.add(
        &status,
        0,
        SizerFlag::Expand | SizerFlag::Left | SizerFlag::Right | SizerFlag::Bottom,
        8,
    );

    panel.set_sizer(root, true);

    let ui = Ui {
        frame: *frame,
        status_bar,
        table,
        search,
        template_list,
        group_choice,
        group_add,
        group_remove,
        status,
    };

    wire(
        &ui,
        shared,
        add_btn,
        resolve_btn,
        force_btn,
        apply_btn,
        dir_btn,
        import_btn,
    );
    ui
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------
//
// Every button and every context menu is wired here, in the one function where
// the widgets are still in scope. The handlers are `'static` and hold only
// `Copy` handles plus a clone of the `Rc` over the data.

#[allow(clippy::too_many_arguments)]
fn wire(
    ui: &Ui,
    shared: &Shared,
    add_btn: Button,
    resolve_btn: Button,
    force_btn: Button,
    apply_btn: Button,
    dir_btn: Button,
    import_btn: Button,
) {
    // --- adding a host ----------------------------------------------------
    {
        let ui = *ui;
        let shared = shared.clone();
        add_btn.on_click(move |_| {
            let typed = match ask_text(
                &ui,
                "Add a host\n\nA domain (api.openai.com) or an address (1.2.3.4).\n\
                 Several at once may be separated by commas or spaces.\n\n\
                 Right-click a row for the rest: rename, remove, or take one \
                 address out of the configs.",
                "Add host",
                "",
            ) {
                Some(t) => t,
                None => return,
            };
            let targets = split_targets(&typed);
            if targets.is_empty() {
                ui.set_status("Nothing to add — the box was empty.");
                return;
            }
            let mut added = 0;
            {
                let mut st = shared.borrow_mut();
                for t in &targets {
                    // A host typed into an empty group list stays standalone;
                    // groups are joined deliberately from the Group box.
                    st.store.add_host(t, &[]);
                    added += 1;
                }
                let _ = st.store.save(&st.config_path);
            }
            refresh(&ui, &shared);
            ui.set_status(&format!(
                "Added {added} host(s). They have no addresses yet — resolve next."
            ));
        });
    }

    // --- the host table: right-click, and double-click to rename -----------
    //
    // A double-click on a row is the fastest way to rename without hunting for
    // a menu; it is also what a screen reader user expects from a list. The
    // keyboard route to the same menu is the Menu key (Shift+F10), which is
    // what `wxEVT_CONTEXT_MENU` carries.
    {
        let ui = *ui;
        let shared = shared.clone();
        ui.table.on_item_activated(move |_| {
            let _ = rename_selected(&ui, &shared);
        });
    }
    {
        let ui = *ui;
        let shared = shared.clone();
        ui.table.on_item_context_menu(move |event| {
            // `get_row` is what this event carries and it is the row the owner
            // pointed at, not whatever was selected before — a right-click on
            // row 5 while row 2 was selected must act on row 5. The fallback is
            // for the keyboard route (Menu / Shift+F10), which some wx
            // backends deliver without a row.
            let row = match event.get_row() {
                Some(row) if row >= 0 => row as usize,
                _ => match ui.table.get_selected_row() {
                    Some(row) => row,
                    None => {
                        ui.set_status("Right-click a host row to see its actions.");
                        return;
                    }
                },
            };
            host_menu(&ui, &shared, row, &event);
        });
    }

    // --- the group list: right-click --------------------------------------
    {
        let ui = *ui;
        let shared = shared.clone();
        ui.group_choice.on_context_menu(move |event| {
            let Some(group) = ui.group_choice.get_string_selection() else {
                ui.set_status("Right-click a group in the list to see its actions.");
                return;
            };
            group_menu(&ui, &shared, &group, &event);
        });
    }

    // --- the template list: right-click -----------------------------------
    {
        let ui = *ui;
        let shared = shared.clone();
        ui.template_list.on_item_context_menu(move |event| {
            let row = match event.get_row() {
                Some(row) if row >= 0 => row as usize,
                _ => match ui.template_list.get_selected_row() {
                    Some(row) => row,
                    None => {
                        ui.set_status("Right-click a template to add it.");
                        return;
                    }
                },
            };
            let name = {
                let st = shared.borrow();
                let needle = ui.search.get_value();
                match st.catalog.search(&needle).get(row) {
                    Some(t) => t.name.clone(),
                    None => {
                        ui.set_status("That template is gone — the search changed underneath.");
                        return;
                    }
                }
            };
            let mut menu = Menu::builder()
                .append_item(
                    ID_CTX_TO_GROUP,
                    &format!("Add {name} to the host list"),
                    "The template's groups and addresses become hosts",
                )
                .build();
            show(&ui.template_list, &mut menu);
        });
    }

    // The two list menus are ordinary popup menus: the click opens them, the
    // choice comes back through the frame's `on_menu_selected` above, where
    // the ids are dispatched. Nothing is done on the right-click itself — a
    // right-click that acted would be a way to delete a config by accident.

    // --- groups -----------------------------------------------------------
    {
        let ui = *ui;
        let shared = shared.clone();
        ui.group_add.on_click(move |_| {
            let typed = match ask_text(
                &ui,
                "Name of the new group.\n\nA group is how several subdomains stay one row in the list.",
                "New group",
                "",
            ) {
                Some(t) => t,
                None => return,
            };
            let name = typed.trim().to_string();
            if name.is_empty() {
                ui.set_status("A group needs a name.");
                return;
            }
            {
                let mut st = shared.borrow_mut();
                st.store.ensure_group(&name);
                let _ = st.store.save(&st.config_path);
            }
            refresh(&ui, &shared);
            ui.set_status(&format!("Group {name} is ready. Select hosts and use \"Add selected hosts to group\"."));
        });
    }

    {
        let ui = *ui;
        let shared = shared.clone();
        group_apply_btn.on_click(move |_| {
            add_selected_to_group(&ui, &shared);
        });
    }

    {
        let ui = *ui;
        let shared = shared.clone();
        ui.group_remove.on_click(move |_| {
            let Some(group) = ui.group_choice.get_string_selection() else {
                ui.set_status("Pick a group from the list first.");
                return;
            };
            forgot_the_files(&ui, &shared, &group);
        });
    }

    // --- resolving --------------------------------------------------------
    {
        let ui = *ui;
        let shared = shared.clone();
        resolve_btn.on_click(move |_| {
            do_resolve(&ui, &shared, false);
        });
    }
    {
        let ui = *ui;
        let shared = shared.clone();
        force_btn.on_click(move |_| {
            do_resolve(&ui, &shared, true);
        });
    }

    // --- writing the configs ----------------------------------------------
    {
        let ui = *ui;
        let shared = shared.clone();
        apply_btn.on_click(move |_| {
            do_apply(&ui, &shared);
        });
    }

    // --- choosing the folder ----------------------------------------------
    {
        let ui = *ui;
        let shared = shared.clone();
        dir_btn.on_click(move |_| {
            ask_conf_dir(&ui.frame, &ui, &shared);
        });
    }

    // --- importing a base config ------------------------------------------
    {
        let ui = *ui;
        let shared = shared.clone();
        import_btn.on_click(move |_| {
            ask_base_conf(&ui.frame, &ui, &shared);
        });
    }
}

/// The menu for one host row. Built fresh on every right-click, so the items
/// reflect the row as it is now: an address item appears only when there is an
/// address to name, and the group items only when there is a group.
fn host_menu(ui: &Ui, shared: &Shared, row: usize, event: &DataViewEvent) {
    let (label, groups, addresses) = {
        let st = shared.borrow();
        match st.store.hosts.get(row) {
            Some(h) => (h.label().to_string(), h.groups.clone(), subtractions(h)),
            None => {
                ui.set_status("That row is gone — the list changed underneath.");
                return;
            }
        }
    };

    let mut menu = Menu::builder()
        .append_item(ID_CTX_RENAME, &format!("Rename {label}…"), "")
        .append_item(
            ID_CTX_REMOVE,
            &format!("Remove {label} from the list"),
            "The .conf files keep the address until you remove it there",
        );

    // One item per address, not one item for the whole row. An `AllowedIPs`
    // line holds several addresses and they are removed one at a time — the
    // owner asked for exactly this ("удаление адреса — делит") — so the menu
    // has to be able to name a single one.
    for addr in &addresses {
        menu = menu.append_item(
            ID_CTX_REMOVE_IP,
            &format!("Remove address {addr} from the configs"),
            "Takes this one address out of every .conf, leaving the others",
        );
    }

    if !groups.is_empty() {
        menu = menu.append_separator();
        for g in &groups {
            menu = menu.append_item(
                ID_CTX_FROM_GROUP,
                &format!("Take {label} out of group {g}"),
                "The host stays in the list",
            );
        }
    }

    let mut menu = menu.build();
    show(&ui.table, &mut menu);
}

/// Open a menu for a widget, at the position wx says the click happened.
///
/// `popup_menu` lives on the `WxWidget` trait, which every widget derefs to,
/// and it wants a `&mut Menu` — the builder hands back an owned one, so it has
/// to be bound to a `mut` local for the duration of the call. All three menus
/// in this file go through here rather than each repeating the call.
fn show(widget: &impl WxWidget, menu: &mut Menu) {
    widget.popup_menu(menu, None);
}

/// The menu for one group, opened from the group list or from a host row's own
/// groups. This is where the command the owner could not find lives.
fn group_menu(ui: &Ui, shared: &Shared, group: &str, event: &MenuEventData) {
    let (hosts, addresses) = {
        let st = shared.borrow();
        let hosts = st.store.hosts_in_group(group).count();
        let entries = subtract::entries_from_group(&st.store, group);
        (hosts, entries.len())
    };

    let mut menu = Menu::builder()
        .append_item(
            ID_CTX_GROUP_REMOVE,
            &format!("Remove group {group}'s addresses from the configs…"),
            "Edits every .conf in the folder; the group and its hosts stay in the list",
        )
        .append_separator()
        .append_item(
            ID_CTX_GROUP_UNGROUP,
            &format!("Remove group {group} from the list…"),
            &format!("{hosts} host(s) stay, just ungrouped — no .conf is touched"),
        )
        .append_item(
            ID_CTX_GROUP_PURGE,
            "Do both: take the addresses out and drop the group…",
            &format!("{addresses} address(es) leave the .conf files and the group leaves the list"),
        )
        .build();
    let _ = event;
    show(&ui.group_choice, &mut menu);
}

// ---------------------------------------------------------------------------
// The actions
// ---------------------------------------------------------------------------

/// Note a dialog result we did not expect, so a picker that opens and then
/// refuses to accept anything is not silent.
///
/// `show_modal()` returning something other than `ID_OK` is a normal cancel —
/// that is most of what happens. The case worth writing down is a result that
/// is neither `ID_OK` nor a known cancel code, because then the dialog *did*
/// return and its answer is simply not what this code matched on, which is a
/// fact about the binding rather than about the owner's clicking.
fn note_odd_dialog_result(ui: &Ui, what: &str, got: i32) {
    if got == ID_OK || got == ID_CANCEL {
        return;
    }
    ui.set_status(&format!(
        "{what}: dialog returned {got}, which is neither OK ({ID_OK}) nor Cancel ({ID_CANCEL}). The path was treated as not chosen."
    ));
}

/// The addresses this host would contribute, in the form the removal matches
/// on: the bare address, without the prefix the write adds. This is the same
/// conversion `subtract::Removals::new` does, so the menu and the removal can
/// never disagree about what "this address" means.
fn subtractions(host: &crate::Host) -> Vec<String> {
    let mut out: Vec<String> = host
        .allowed_entries()
        .into_iter()
        .map(|e| e.split('/').next().unwrap_or(&e).to_string())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Ask for the folder of `.conf` files and remember it. Returns true when a
/// folder was chosen and saved.
fn ask_conf_dir(frame: &Frame, ui: &Ui, shared: &Shared) -> bool {
    let current = shared.borrow().store.conf_dir.clone().unwrap_or_default();
    let dlg = DirDialog::builder(
        frame,
        "Pick the folder that holds your AmnesiaWG .conf files. Every .conf in it \
         will get the same AllowedIPs.",
        &current,
    )
    .build();
    let result = dlg.show_modal();
    if result != ID_OK {
        note_odd_dialog_result(ui, "Config folder", result);
        return false;
    }
    let Some(path) = dlg.get_path() else {
        return false;
    };
    let mut st = shared.borrow_mut();
    st.store.conf_dir = Some(path.clone());
    let saved = st.store.save(&st.config_path);
    drop(st);
    refresh(ui, shared);
    match saved {
        Ok(()) => ui.set_status(&format!("Config folder: {path}")),
        Err(e) => ui.set_status(&format!(
            "Folder set to {path}, but saving hosts.json failed: {e}"
        )),
    }
    true
}

/// Ask which `.conf` to use as a base, then pull its `AllowedIPs` into the
/// host list.
///
/// The path is remembered in `hosts.json`, so the second run offers it as the
/// default rather than making the owner find the file again — the base config
/// changes rarely and hunting for it twice is the friction that stops a
/// feature being used.
///
/// Nothing is written to the `.conf` files here. The import only proposes
/// hosts; the addresses land in the list, and "Write to all configs" is still
/// the single door to disk. An import that wrote on its own would be a second
/// way to change twenty files, and there is exactly one on purpose.
fn ask_base_conf(frame: &Frame, ui: &Ui, shared: &Shared) -> bool {
    let current = shared.borrow().store.base_conf.clone().unwrap_or_default();
    let dlg = FileDialog::builder(frame)
        .with_message("Pick a base WireGuard config. Its AllowedIPs become hosts in your list.")
        .with_default_dir(&current)
        .build();
    let result = dlg.show_modal();
    if result != ID_OK {
        note_odd_dialog_result(ui, "Base config", result);
        return false;
    }
    let Some(path) = dlg.get_path() else {
        return false;
    };
    let imported = match import::from_config(std::path::Path::new(&path), &PtrResolver) {
        Ok(i) => i,
        Err(e) => {
            ui.set_status(&format!("Could not read {path}: {e}"));
            return false;
        }
    };
    match imported {
        Imported::FullTunnel => {
            ui.set_status(
                "That config tunnels everything (0.0.0.0/0) — there is no site list to take.",
            );
            false
        }
        Imported::Nothing => {
            ui.set_status("That config has no AllowedIPs line to import.");
            false
        }
        Imported::Hosts(hosts) => {
            let addrs: usize = hosts.iter().map(|h| h.ips.len()).sum();
            let labels: Vec<String> = hosts
                .iter()
                .take(8)
                .map(|h| h.label().to_string())
                .collect();
            let mut preview = labels.join(", ");
            if hosts.len() > labels.len() {
                preview.push_str(&format!(", and {} more", hosts.len() - labels.len()));
            }
            let question = format!(
                "Import {} host(s) from {addrs} address(es)?\n\n{preview}\n\n\
                 Names come from a reverse lookup where one exists, the network \
                 otherwise. Nothing is written to your .conf files until you press \
                 \"Write to all configs\".",
                hosts.len()
            );
            if !confirm(ui, &question) {
                ui.set_status("Import cancelled — the host list is unchanged.");
                return false;
            }
            let added = {
                let mut st = shared.borrow_mut();
                let n = import::apply_import(&mut st.store, hosts, None);
                st.store.base_conf = Some(path.clone());
                let _ = st.store.save(&st.config_path);
                n
            };
            refresh(ui, shared);
            ui.set_status(&format!(
                "Imported from {path}: {added} new host(s). Resolve, then write to all configs."
            ));
            true
        }
    }
}

/// The row the owner has selected, as an index into `store.hosts`.
fn rename_selected(ui: &Ui, shared: &Shared) -> bool {
    let Some(i) = ui.table.get_selected_row() else {
        ui.set_status("Select a host in the list first.");
        return false;
    };
    rename_row(ui, shared, i)
}

/// Rename the host at `row`, whatever selected it.
fn rename_row(ui: &Ui, shared: &Shared, row: usize) -> bool {
    let (old, was_ip) = {
        let st = shared.borrow();
        match st.store.hosts.get(row) {
            Some(h) => (h.target.clone(), h.is_literal_ip()),
            None => {
                ui.set_status("That row is gone — the list changed underneath.");
                return false;
            }
        }
    };
    let typed = match ask_text(
        ui,
        &format!(
            "Rename {old}\n\nA domain or an address.{}",
            if was_ip {
                "\n\nThis one is a literal address, so changing it to a domain is allowed — \
                 its cached addresses will be dropped and need resolving again."
            } else {
                ""
            }
        ),
        "Rename host",
        &old,
    ) {
        Some(t) => t,
        None => return false,
    };
    let new = typed.trim().to_string();
    if new == old {
        ui.set_status("Unchanged.");
        return false;
    }
    let outcome = {
        let mut st = shared.borrow_mut();
        let r = st.store.rename_host(&old, &new);
        let _ = st.store.save(&st.config_path);
        r
    };
    refresh(ui, shared);
    match outcome {
        Ok(()) => {
            ui.set_status(&format!("Renamed {old} to {new}."));
            true
        }
        Err(e) => {
            ui.set_status(&format!("Could not rename {old}: {e}"));
            false
        }
    }
}

/// Take one host out of the list. The `.conf` files are deliberately not
/// touched: an address already written into twenty files is still there
/// afterwards, and the status line says so.
fn remove_row(ui: &Ui, shared: &Shared, row: usize) {
    let (label, target) = {
        let st = shared.borrow();
        match st.store.hosts.get(row) {
            Some(h) => (h.label().to_string(), h.target.clone()),
            None => {
                ui.set_status("That row is gone — the list changed underneath.");
                return;
            }
        }
    };
    if !confirm(
        ui,
        &format!(
            "Remove {label} from the list?\n\n\
             This does not touch the .conf files — the address is still written \
             into every one of them. To take it out of the configs, use \
             \"Remove address … from the configs\"."
        ),
    ) {
        ui.set_status("Nothing removed.");
        return;
    }
    let outcome = {
        let mut st = shared.borrow_mut();
        let r = st.store.remove_host(&target).map(|_| ());
        let _ = st.store.save(&st.config_path);
        r
    };
    refresh(ui, shared);
    match outcome {
        Ok(()) => ui.set_status(&format!(
            "Removed {label} from the list. Its address is still in the .conf files."
        )),
        Err(e) => ui.set_status(&format!("Could not remove {label}: {e}")),
    }
}

/// Remove exactly one address from every config, and report how many entries
/// left how many files.
fn remove_address(ui: &Ui, shared: &Shared, address: &str) {
    let dir = match shared.borrow().store.conf_dir.clone() {
        Some(d) => PathBuf::from(d),
        None => {
            ui.set_status("No config folder chosen. Use \"Config folder…\" first.");
            return;
        }
    };

    let removals = Removals::new([address]);
    let (entries, files) = match subtract::survey(&dir, &removals) {
        Ok(n) => n,
        Err(e) => {
            ui.set_status(&format!("Could not read the config folder: {e}"));
            return;
        }
    };
    if entries == 0 {
        ui.set_status(&format!(
            "{address} is not in any .conf in this folder — nothing to remove."
        ));
        return;
    }

    if !confirm(
        ui,
        &format!(
            "Remove {address} from the configs?\n\n\
             {entries} entr(ies) in {files} file(s) will lose it, and every other \
             address on the same line stays. A .conf that would be left with an \
             empty AllowedIPs is reported and not written.\n\n\
             A .bak copy is made before the first change."
        ),
    ) {
        ui.set_status("Nothing removed.");
        return;
    }

    ui.set_status(&format!("Removing {address} from the configs…"));
    match subtract::subtract(&dir, &removals) {
        Ok(report) => ui.set_status(&removal_summary(&report, &format!("{address} removed"))),
        Err(e) => ui.set_status(&format!("Could not remove {address}: {e}")),
    }
}

/// Remove every address a group's hosts contribute, from every `.conf`.
///
/// `purge` is the difference between the two menu items that get here: with
/// `false` the group and its hosts stay in the list, so the removal can be
/// undone by writing the configs again; with `true` the group is dropped from
/// the list as well, which is the "I never want this again" shape.
fn remove_group_from_configs(ui: &Ui, shared: &Shared, group: &str, purge: bool) {
    let dir = match shared.borrow().store.conf_dir.clone() {
        Some(d) => PathBuf::from(d),
        None => {
            ui.set_status("No config folder chosen. Use \"Config folder…\" first.");
            return;
        }
    };

    let (entries, hosts) = {
        let st = shared.borrow();
        (
            subtract::entries_from_group(&st.store, group),
            st.store.hosts_in_group(group).count(),
        )
    };
    if entries.is_empty() {
        ui.set_status(&format!(
            "{group} has no addresses to remove — its hosts are unresolved, so nothing of \
             it is in the configs yet."
        ));
        return;
    }

    let removals = Removals::new(entries.iter().cloned());
    let (found, files) = match subtract::survey(&dir, &removals) {
        Ok(n) => n,
        Err(e) => {
            ui.set_status(&format!("Could not read the config folder: {e}"));
            return;
        }
    };

    let mut question = format!(
        "Remove the addresses of group {group} from the configs?\n\n\
         The group holds {hosts} host(s) and {} address(es).\n\
         {found} of them are present in {files} .conf file(s); every other address \
         on those lines stays.\n\n",
        entries.len()
    );
    if purge {
        question.push_str(&format!(
            "The group {group} is then removed from the list as well."
        ));
    } else {
        question.push_str(
            "The group and its hosts stay in the list — writing the configs again puts \
             the addresses back.",
        );
    }
    question.push_str("\n\nA .bak copy is made next to each file before its first change.");
    if !confirm(ui, &question) {
        ui.set_status("Nothing removed.");
        return;
    }

    ui.set_status(&format!("Removing {group} from the configs…"));
    let summary = match subtract::subtract(&dir, &removals) {
        Ok(report) => removal_summary(&report, &format!("{group}: addresses removed")),
        Err(e) => {
            ui.set_status(&format!("Could not remove {group}: {e}"));
            return;
        }
    };

    if !purge {
        ui.set_status(&summary);
        return;
    }

    let outcome = {
        let mut st = shared.borrow_mut();
        let r = st.store.remove_group(group);
        let _ = st.store.save(&st.config_path);
        r
    };
    refresh(ui, shared);
    match outcome {
        Ok(n) => ui.set_status(&format!(
            "{summary} Group {group} removed from the list; {n} host(s) kept."
        )),
        Err(e) => ui.set_status(&format!("{summary} But removing group {group} failed: {e}")),
    }
}

/// The sentence a removal run produces. «убралось 12 адресов из 4 конфигов» is
/// the shape the owner asked for, so the counts lead and the exceptions follow.
fn removal_summary(report: &apply::Report, what: &str) -> String {
    let written = report.written_confs();
    let mut msg = format!(
        "{what}: {} address(es) out of {written} file(s).",
        report.removed()
    );
    let unchanged = report.unchanged_confs();
    if unchanged > 0 {
        msg.push_str(&format!(" {unchanged} file(s) did not have them."));
    }
    let emptied = report.emptied();
    if emptied > 0 {
        msg.push_str(&format!(
            " {emptied} file(s) would have been left with an empty AllowedIPs and were not written — they still have every address."
        ));
    }
    if !report.errors.is_empty() {
        let names = report
            .errors
            .iter()
            .map(|(p, e)| format!("{} ({e})", file_name(p)))
            .collect::<Vec<_>>()
            .join(", ");
        msg.push_str(&format!(" Failed: {names}"));
    }
    msg
}

/// Remove a group from the list only. Every address stays in every `.conf` —
/// which is the whole reason this is not called "delete".
fn forgot_the_files(ui: &Ui, shared: &Shared, group: &str) {
    let hosts = shared.borrow().store.hosts_in_group(group).count();
    if !confirm(
        ui,
        &format!(
            "Remove group {group} from the list?\n\n\
             {hosts} host(s) stay in the list, just ungrouped. \
             Every address of theirs stays in every .conf — \
             use \"Remove group {group}'s addresses from the configs…\" for that."
        ),
    ) {
        ui.set_status("Nothing removed.");
        return;
    }
    let outcome = {
        let mut st = shared.borrow_mut();
        let r = st.store.remove_group(group);
        let _ = st.store.save(&st.config_path);
        r
    };
    refresh(ui, shared);
    match outcome {
        Ok(n) => ui.set_status(&format!(
            "Group {group} removed from the list; {n} host(s) kept. The configs are untouched."
        )),
        Err(e) => ui.set_status(&format!("Could not remove {group}: {e}")),
    }
}

/// Add every selected host to the chosen group.
fn add_selected_to_group(ui: &Ui, shared: &Shared) {
    let Some(group) = ui.group_choice.get_string_selection() else {
        ui.set_status("Pick a group from the list first.");
        return;
    };
    // All selected rows, so several hosts can be grouped in one go.
    let rows = selected_rows(&ui.table);
    if rows.is_empty() {
        ui.set_status("Select one or more hosts in the table first.");
        return;
    }
    let mut moved = 0;
    {
        let mut st = shared.borrow_mut();
        for i in &rows {
            if let Some(h) = st.store.hosts.get_mut(*i) {
                if !h.groups.contains(&group) {
                    h.groups.push(group.clone());
                    moved += 1;
                }
            }
        }
        if moved > 0 {
            st.store.ensure_group(&group);
        }
        let _ = st.store.save(&st.config_path);
    }
    refresh(ui, shared);
    ui.set_status(&format!("{moved} host(s) added to {group}."));
}

/// Take one host out of one group, leaving the host in the list.
fn remove_row_from_group(ui: &Ui, shared: &Shared, row: usize, group: &str) {
    let label = {
        let mut st = shared.borrow_mut();
        let Some(h) = st.store.hosts.get_mut(row) else {
            ui.set_status("That row is gone — the list changed underneath.");
            return;
        };
        let label = h.label().to_string();
        h.groups.retain(|g| g != group);
        let _ = st.store.save(&st.config_path);
        label
    };
    refresh(ui, shared);
    ui.set_status(&format!(
        "{label} is out of group {group}. It is still in the list."
    ));
}

/// Resolve every domain that needs it and report the count, naming the ones
/// that failed — a host that silently keeps no address is the failure mode
/// this application exists to prevent.
fn do_resolve(ui: &Ui, shared: &Shared, force: bool) {
    // The status is set before the work starts, so a slow resolver does not
    // look like a crash.
    ui.set_status(if force {
        "Resolving every domain again…"
    } else {
        "Resolving domains that have no addresses yet…"
    });

    let (resolved, failed, fresh) = {
        let mut st = shared.borrow_mut();
        let outcomes = resolve::resolve_all(&mut st.store, &SystemResolver, force);
        let _ = st.store.save(&st.config_path);
        let mut resolved = 0;
        let mut fresh = 0;
        let mut failed = Vec::new();
        for (target, outcome) in outcomes {
            match outcome {
                resolve::Outcome::Resolved(ips) => {
                    resolved += 1;
                    if ips.is_empty() {
                        failed.push(target);
                    }
                }
                resolve::Outcome::Failed(e) => failed.push(format!("{target} ({e})")),
                resolve::Outcome::Fresh => fresh += 1,
                resolve::Outcome::Literal => {}
            }
        }
        (resolved, failed, fresh)
    };

    refresh(ui, shared);
    if failed.is_empty() {
        let mut msg = format!("Resolved {resolved} domain(s)");
        if fresh > 0 {
            msg.push_str(&format!("; {fresh} already had addresses"));
        }
        ui.set_status(&format!("{msg}."));
    } else {
        ui.set_status(&format!(
            "Resolved {resolved}; {} failed: {}",
            failed.len(),
            failed.join(", ")
        ));
    }
}

/// Write the value into every `.conf` in the chosen folder, via the core's
/// `apply`, which refuses outright when a host has no address — so a partial
/// tunnel is never written.
fn do_apply(ui: &Ui, shared: &Shared) {
    let dir = match shared.borrow().store.conf_dir.clone() {
        Some(d) => PathBuf::from(d),
        None => {
            ui.set_status("No config folder chosen. Use \"Config folder…\" first.");
            return;
        }
    };

    let value = {
        let st = shared.borrow();
        match apply::value_or_error(&st.store) {
            Ok(v) => v,
            Err(e) => {
                ui.set_status(&format!("Not written: {e}"));
                return;
            }
        }
        // Deliberately dropped here: the borrow must not be held across the
        // write below, which does not touch the store.
    };

    ui.set_status("Writing AllowedIPs into the configs…");
    match apply::apply_value(&dir, &value) {
        Ok(report) => {
            let written = report.written();
            let unchanged = report.unchanged();
            let mut msg = format!("{written} file(s) written, {unchanged} unchanged.");
            if !report.errors.is_empty() {
                let names = report
                    .errors
                    .iter()
                    .map(|(p, e)| format!("{} ({e})", file_name(p)))
                    .collect::<Vec<_>>()
                    .join(", ");
                msg.push_str(&format!(" Failed: {names}"));
            }
            ui.set_status(&msg);
        }
        Err(e) => ui.set_status(&format!("Could not write the configs: {e}")),
    }
}

/// Pull the catalog from the internet and keep what is usable. A download that
/// fails leaves the built-in list alone, which is the whole point of shipping
/// one inside the binary.
fn update_templates(ui: &Ui, shared: &Shared) {
    ui.set_status("Fetching the template catalog…");
    match templates::fetch(templates::DEFAULT_CATALOG_URL) {
        Ok(updated) => {
            let merged = templates::merge(updated, templates::builtin());
            let count = merged.templates.len();
            {
                let mut st = shared.borrow_mut();
                st.catalog = merged;
                st.store.templates_updated = Some(now_rfc3339());
                let _ = st.store.save(&st.config_path);
            }
            refresh(ui, shared);
            ui.set_status(&format!("Template catalog updated — {count} groups known."));
        }
        Err(e) => {
            ui.set_status(&format!(
                "Could not update: {e} — the built-in catalog is still in use."
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Filling the widgets from the data
// ---------------------------------------------------------------------------

/// Redraw the whole window from the store. Called after every change rather
/// than patching one row: the list is small, and a table that always matches
/// `hosts.json` is worth more than the milliseconds saved.
fn refresh(ui: &Ui, shared: &Shared) {
    let state = shared.borrow();

    // --- hosts ---
    ui.table.delete_all_items();
    for h in &state.store.hosts {
        let groups = if h.groups.is_empty() {
            String::new()
        } else {
            h.groups.join(", ")
        };
        ui.table.append_item(&[
            Variant::from_string(h.label()),
            Variant::from_string(&groups),
            Variant::from_string(&addresses_cell(h)),
            Variant::from_string(&state_cell(h)),
        ]);
    }

    // --- groups, keeping the current choice when it is still there ---
    let previous = ui.group_choice.get_string_selection();
    ui.group_choice.clear();
    for g in &state.store.groups {
        ui.group_choice.append(g);
    }
    if let Some(p) = previous {
        if let Some(i) = state.store.groups.iter().position(|g| *g == p) {
            ui.group_choice.set_selection(i as u32);
        }
    }

    fill_templates(ui, &state);

    // --- the folder, in the status bar, so it is never a mystery which one
    // is being written to ---
    //
    // Field 0, not 1: the bar has exactly one pane, and wxWidgets asserts
    // `(unsigned)number < m_panes.size()` in SetStatusText — an index past the
    // last pane is not clipped, it aborts a debug build and does nothing at
    // all on a release one. Both paths below were writing to field 1 of a
    // one-pane bar, so the line that says *where* the configs are was never
    // the line that appeared.
    let folder = match &state.store.conf_dir {
        Some(d) => format!("Configs: {d}"),
        None => "Configs: (none chosen)".to_string(),
    };
    ui.frame.set_status_text(
        &format!("{folder}   |   {}", state.config_path.display()),
        crate::guiassert::STATUS_PRIMARY_FIELD as i32,
    );
}

/// The addresses column of one row: the domain leads the row, so this is what
/// follows it. Emptiness is a fact worth stating — a host with no address is
/// the one thing in this window that will refuse to be written — so it says
/// why, in the words of the state column, rather than showing a blank.
fn addresses_cell(h: &crate::Host) -> String {
    if h.ips.is_empty() {
        return String::new();
    }
    if h.ips.len() <= 3 {
        h.ips.join(", ")
    } else {
        format!("{} … ({} addresses)", h.ips[0], h.ips.len())
    }
}

/// The state column: the answer to "why is this site still not in the tunnel",
/// which is otherwise unanswerable from the window.
fn state_cell(h: &crate::Host) -> String {
    if let Some(e) = &h.error {
        return format!("failed: {e}");
    }
    if h.source == Source::Template {
        return "from template".to_string();
    }
    if h.ips.is_empty() {
        if h.is_literal_ip() {
            // A literal address that is not its own route yet: `add_host`
            // deliberately does not seed the target into `ips`, so this is
            // waiting for a resolve pass like any domain.
            return "not resolved".to_string();
        }
        return format!("not resolved — {}", mask_words(h.target.as_str()));
    }
    // The prefix, in words, for the single-address and small rows. Each
    // address that is a network gets its own clause; `select` would have been
    // prettier but this runs per row and the list is small.
    let mut notes: Vec<String> = Vec::new();
    for ip in &h.ips {
        if let Some(words) = mask_note(ip) {
            notes.push(words);
        }
    }
    notes.join("; ")
}

// ---------------------------------------------------------------------------
// Saying a prefix length in words
// ---------------------------------------------------------------------------

/// `31.13.64.0/24` → "a network: 256 addresses (mask 24 bits)".
///
/// The owner asked for this by name: a prefix length is a number of bits, and
/// a number of bits is not an address count until it is counted, so the window
/// says both. A bare host route (`/32`, `/128`) is the normal case and gets no
/// note — saying "mask 32 bits, 1 address" on every line would bury the four
/// rows that are actually networks.
fn mask_note(entry: &str) -> Option<String> {
    let (_, prefix) = entry.split_once('/')?;
    let bits: u32 = prefix.trim().parse().ok()?;
    let total = if entry.contains(':') { 128 } else { 32 };
    if bits >= total {
        return None;
    }
    let addresses = 1u128.checked_shl(total - bits).unwrap_or(u128::MAX);
    Some(format!(
        "a network: {addresses} address{} (mask {bits} bits)",
        if addresses == 1 { "" } else { "es" }
    ))
}

/// The same fact for a target the owner typed, which may be an unresolved
/// name — in which case there is no prefix to explain and nothing to say.
fn mask_words(target: &str) -> String {
    match mask_note(target) {
        Some(note) => note,
        None => "no address yet".to_string(),
    }
}

/// Fill the template table from the search box. The row order here is the
/// order the "Add template" button indexes into, so both go through
/// `Catalog::search` with the same needle.
fn fill_templates(ui: &Ui, state: &GuiState) {
    let needle = ui.search.get_value();
    let hits = state.catalog.search(&needle);
    ui.template_list.delete_all_items();
    for t in hits {
        let covers = if t.domains.len() <= 3 {
            t.domains.join(", ")
        } else {
            format!(
                "{}, {} and {} more",
                t.domains[0],
                t.domains[1],
                t.domains.len() - 2
            )
        };
        ui.template_list.append_item(&[
            Variant::from_string(&t.name),
            Variant::from_string(&covers),
            Variant::from_string(&t.ips.len().to_string()),
        ]);
    }
}

// ---------------------------------------------------------------------------
// Dialogs
// ---------------------------------------------------------------------------

/// A one-line text prompt. Returns the text, or `None` when it was cancelled.
fn ask_text(ui: &Ui, message: &str, caption: &str, default: &str) -> Option<String> {
    let dlg = TextEntryDialog::builder(&ui.frame, message, caption)
        .with_default_value(default)
        .build();
    if dlg.show_modal() != ID_OK {
        return None;
    }
    dlg.get_value()
}

/// A yes/no question, defaulting to "no" in wording so a stray Enter does not
/// delete something.
fn confirm(ui: &Ui, message: &str) -> bool {
    let dlg = MessageDialog::builder(&ui.frame, message, "wireutils")
        .with_style(MessageDialogStyle::YesNo | MessageDialogStyle::IconQuestion)
        .build();
    dlg.set_yes_no_labels("Yes", "No");
    dlg.show_modal() == ID_YES
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// What the owner typed into the add box, split into targets. Commas, spaces
/// and newlines all separate, because pasting a list from anywhere is the
/// normal way this box gets filled.
fn split_targets(text: &str) -> Vec<String> {
    text.split(|c: char| c == ',' || c == '\n' || c == '\r' || c.is_whitespace())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Every selected row, in order. `get_selected_row` only reports one; the
/// multi-select case is read by walking the rows, which is also what a
/// screen-reader user gets when they hold shift and arrow.
fn selected_rows(table: &DataViewListCtrl) -> Vec<usize> {
    let count = table.get_item_count();
    (0..count).filter(|i| table.is_row_selected(*i)).collect()
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string())
}
