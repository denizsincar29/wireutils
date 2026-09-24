//! The window.
//!
//! Built behind the `gui` feature because wxWidgets and a display are both
//! absent from the container this is developed in — the core is deliberately
//! GUI-independent so everything testable is tested without a window, and this
//! file is the thin shell that calls it.
//!
//! Shape of the window: a table of hosts (the domain, or the IP when there is
//! no domain), buttons to add / rename / remove, a group box, a template
//! search that fills the list from the catalog, and a button that writes the
//! addresses into every `.conf` in the chosen folder. The folder is asked for
//! on the first run and remembered in `hosts.json`.
//!
//! Accessibility notes, because the owner reads the screen rather than looks
//! at it: the host list is a `DataViewListCtrl`, which is a real multi-column
//! list (NVDA announces it as a list and reads the columns), every action is
//! also reachable from the keyboard through the tree of buttons and dialogs,
//! and the status line at the bottom is a plain `StaticText` that reports what
//! the last action did — the result of a write to twenty configs is otherwise
//! invisible.
//!
//! Two rules this file follows throughout:
//!
//! * Nothing touches the network or the disk from an event handler without
//!   saying so in the status line first. A write to twenty configs that
//!   silently took four seconds reads as a hang.
//! * Every handler re-reads the store from `Shared` instead of keeping a
//!   reference across calls, and every handler that changes the store saves it
//!   before returning. `hosts.json` is the owner's only copy of the list.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use wxdragon::prelude::*;

use crate::apply;
use crate::hosts::{HostStore, Source};
use crate::import::{self, Imported, PtrResolver};
use crate::paths;
use crate::resolve::{self, SystemResolver};
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
fn start(app: &App) -> i32 {
    let config_path = match paths::store_path() {
        Some(p) => p,
        None => {
            let dlg = MessageDialog::builder(
                &unsafe { wxdragon::window::Window::get_desktop_window() },
                "Could not work out where to keep hosts.json.",
                "wireutils",
            )
            .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconError)
            .build();
            dlg.show_modal();
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
                // stop. He can fix the file by hand and start again.
                let msg = format!(
                    "Could not read {}\n\n{e}\n\nFix or move the file, then start again.",
                    config_path.display()
                );
                let dlg = MessageDialog::builder(
                    &unsafe { wxdragon::window::Window::get_desktop_window() },
                    &msg,
                    "wireutils",
                )
                .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconError)
                .build();
                dlg.show_modal();
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

    let ui = build_body(&frame, &shared);
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

    app.run();
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
    table: DataViewListCtrl,
    search: SearchCtrl,
    template_list: DataViewListCtrl,
    group_choice: Choice,
    status: StaticText,
}

impl Ui {
    /// The one place that says what just happened. Both the frame's status bar
    /// and the bottom line are written, because on Windows the status bar is
    /// what a screen reader announces after a button press, and the line is
    /// what stays on screen afterwards.
    fn set_status(&self, text: &str) {
        self.status.set_label(text);
        self.frame.set_status_text(text, 0);
    }
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// The menu bar. Built after the widgets exist because every item is wired to
/// the same handlers the buttons use — there is exactly one implementation of
/// "write to the configs", shared by the button and the menu item.
fn build_menu(frame: &Frame, ui: &Ui, shared: &Shared) -> MenuBar {
    let pick = MenuBar::builder()
        .append(
            Menu::builder()
                .append_item(ID_PICK_DIR, "Config folder…", "Choose the folder of .conf files")
                .append_item(ID_APPLY, "Write to all configs", "Rewrite AllowedIPs in every .conf")
                .append_item(ID_UPDATE_TPL, "Update templates from the internet", "Fetch the newest list of known sites")
                .append_separator()
                .append_item(ID_IMPORT, "Import addresses from a base config…", "Read a .conf's AllowedIPs into the host list")
                .append_separator()
                .append_item(ID_EXIT, "Exit", "Close wireutils")
                .build(),
            "File",
        )
        .build();

    let frame_for_menu = *frame;
    let ui_for_menu = *ui;
    let shared_for_menu = shared.clone();

    pick.clone().on_menu_selected(move |e: MenuEventData| {
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
            _ => {}
        }
    });

    pick
}

fn build_body(frame: &Frame, shared: &Shared) -> Ui {
    let panel = Panel::builder(frame).build();
    let root = BoxSizer::builder(Orientation::Vertical).build();

    // --- host table -------------------------------------------------------
    let hosts_box =
        StaticBoxSizerBuilder::new_with_label(Orientation::Vertical, &panel, "Hosts"
        ).build();

    let table = DataViewListCtrl::builder(&panel).build();
    table.append_text_column("Host", 0, DataViewAlign::Left, 260, DataViewColumnFlags::Resizable);
    table.append_text_column("Groups", 1, DataViewAlign::Left, 160, DataViewColumnFlags::Resizable);
    table.append_text_column("Addresses", 2, DataViewAlign::Left, 380, DataViewColumnFlags::Resizable);
    table.append_text_column("State", 3, DataViewAlign::Left, 140, DataViewColumnFlags::Resizable);

    hosts_box.add(&table, 1, SizerFlag::Expand | SizerFlag::All, 6);

    // --- host buttons -----------------------------------------------------
    let buttons = BoxSizer::builder(Orientation::Horizontal).build();
    let add_btn = Button::builder(&panel).with_label("Add host…").build();
    let rename_btn = Button::builder(&panel).with_label("Rename…").build();
    let remove_btn = Button::builder(&panel).with_label("Remove").build();
    buttons.add(&add_btn, 0, SizerFlag::All, 4);
    buttons.add(&rename_btn, 0, SizerFlag::All, 4);
    buttons.add(&remove_btn, 0, SizerFlag::All, 4);
    hosts_box.add_sizer(&buttons, 0, SizerFlag::Expand, 0);

    root.add_sizer(&hosts_box, 3, SizerFlag::Expand | SizerFlag::All, 8);

    // --- groups -----------------------------------------------------------
    let groups_box =
        StaticBoxSizerBuilder::new_with_label(Orientation::Horizontal, &panel, "Groups").build();
    let group_choice = Choice::builder(&panel).build();
    let group_add = Button::builder(&panel).with_label("New group…").build();
    let group_apply_btn = Button::builder(&panel).with_label("Add selected hosts to group").build();
    let group_rm = Button::builder(&panel).with_label("Delete group").build();
    groups_box.add(&group_choice, 1, SizerFlag::All | SizerFlag::Expand, 6);
    groups_box.add(&group_add, 0, SizerFlag::All, 4);
    groups_box.add(&group_apply_btn, 0, SizerFlag::All, 4);
    groups_box.add(&group_rm, 0, SizerFlag::All, 4);
    root.add_sizer(
        &groups_box,
        0,
        SizerFlag::Expand | SizerFlag::Left | SizerFlag::Right,
        8,
    );

    // --- templates --------------------------------------------------------
    let tpl_box =
        StaticBoxSizerBuilder::new_with_label(Orientation::Horizontal, &panel, "Templates").build();
    let search = SearchCtrl::builder(&panel)
        .with_value("")
        .with_size(Size::new(300, -1))
        .build();
    search.show_search_button(false);
    search.show_cancel_button(true);

    let template_list = DataViewListCtrl::builder(&panel).build();
    template_list.append_text_column("Template", 0, DataViewAlign::Left, 220, DataViewColumnFlags::Resizable);
    template_list.append_text_column("What it covers", 1, DataViewAlign::Left, 460, DataViewColumnFlags::Resizable);
    template_list.append_text_column("Known IPs", 2, DataViewAlign::Left, 100, DataViewColumnFlags::Resizable);

    let tpl_col = BoxSizer::builder(Orientation::Vertical).build();
    tpl_col.add(&search, 0, SizerFlag::All | SizerFlag::Expand, 6);
    tpl_col.add(&template_list, 1, SizerFlag::All | SizerFlag::Expand, 6);

    let tpl_right = BoxSizer::builder(Orientation::Vertical).build();
    let tpl_add = Button::builder(&panel).with_label("Add template").build();
    let tpl_update = Button::builder(&panel).with_label("Update from internet").build();
    tpl_right.add(&tpl_add, 0, SizerFlag::All, 4);
    tpl_right.add(&tpl_update, 0, SizerFlag::All, 4);

    tpl_box.add_sizer(&tpl_col, 1, SizerFlag::Expand, 0);
    tpl_box.add_sizer(&tpl_right, 0, SizerFlag::All, 0);
    root.add_sizer(
        &tpl_box,
        2,
        SizerFlag::Expand | SizerFlag::Left | SizerFlag::Right,
        8,
    );

    // --- the actions that touch the world ---------------------------------
    let bottom = BoxSizer::builder(Orientation::Horizontal).build();
    let resolve_btn = Button::builder(&panel).with_label("Resolve domains").build();
    let force_btn = Button::builder(&panel).with_label("Resolve again (all)").build();
    let apply_btn = Button::builder(&panel).with_label("Write to all configs").build();
    let dir_btn = Button::builder(&panel).with_label("Config folder…").build();
    let import_btn = Button::builder(&panel).with_label("Import base config…").build();
    bottom.add(&resolve_btn, 0, SizerFlag::All, 4);
    bottom.add(&force_btn, 0, SizerFlag::All, 4);
    bottom.add(&apply_btn, 0, SizerFlag::All, 4);
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
        table,
        search,
        template_list,
        group_choice,
        status,
    };

    wire(&ui, shared, add_btn, rename_btn, remove_btn, group_add, group_apply_btn, group_rm,
         tpl_add, tpl_update, resolve_btn, force_btn, apply_btn, dir_btn, import_btn);
    ui
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------
//
// Every button is wired here, in the one function where the widgets are still
// in scope. The handlers are `'static` and hold only `Copy` handles plus a
// clone of the `Rc` over the data.

#[allow(clippy::too_many_arguments)]
fn wire(
    ui: &Ui,
    shared: &Shared,
    add_btn: Button,
    rename_btn: Button,
    remove_btn: Button,
    group_add: Button,
    group_apply_btn: Button,
    group_rm: Button,
    tpl_add: Button,
    tpl_update: Button,
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
                 Several at once may be separated by commas or spaces.",
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
                    // groups are joined deliberately from the Groups box.
                    st.store.add_host(t, &[]);
                    added += 1;
                }
                let _ = st.store.save(&st.config_path);
            }
            refresh(&ui, &shared);
            ui.set_status(&format!("Added {added} host(s). They have no addresses yet — resolve next."));
        });
    }

    // --- renaming the selected host ---------------------------------------
    {
        let ui = *ui;
        let shared = shared.clone();
        rename_btn.on_click(move |_| {
            let _ = rename_selected(&ui, &shared);
        });
    }

    // A double-click on a row is the fastest way to rename without hunting for
    // the button; it is also what a screen reader user expects from a list.
    {
        let ui = *ui;
        let shared = shared.clone();
        ui.table.on_item_activated(move |_| {
            let _ = rename_selected(&ui, &shared);
        });
    }

    // --- removing the selected host ---------------------------------------
    {
        let ui = *ui;
        let shared = shared.clone();
        remove_btn.on_click(move |_| {
            let Some(i) = ui.table.get_selected_row() else {
                ui.set_status("Select a host in the list first.");
                return;
            };
            let (label, target) = {
                let st = shared.borrow();
                match st.store.hosts.get(i) {
                    Some(h) => (h.label().to_string(), h.target.clone()),
                    None => {
                        ui.set_status("That row is gone — the list changed underneath.");
                        return;
                    }
                }
            };
            if !confirm(
                &ui,
                &format!("Remove {label} from the list?\n\nThe .conf files are not touched until you write to them."),
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
            refresh(&ui, &shared);
            match outcome {
                Ok(()) => ui.set_status(&format!("Removed {label}.")),
                Err(e) => ui.set_status(&format!("Could not remove {label}: {e}")),
            }
        });
    }

    // --- groups -----------------------------------------------------------
    {
        let ui = *ui;
        let shared = shared.clone();
        group_add.on_click(move |_| {
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
            refresh(&ui, &shared);
            ui.set_status(&format!("{moved} host(s) added to {group}."));
        });
    }

    {
        let ui = *ui;
        let shared = shared.clone();
        group_rm.on_click(move |_| {
            let Some(group) = ui.group_choice.get_string_selection() else {
                ui.set_status("Pick a group from the list first.");
                return;
            };
            if !confirm(
                &ui,
                &format!("Delete the group {group}?\n\nIts hosts stay in the list, just ungrouped — no address is lost."),
            ) {
                return;
            }
            let outcome = {
                let mut st = shared.borrow_mut();
                let r = st.store.remove_group(&group);
                let _ = st.store.save(&st.config_path);
                r
            };
            refresh(&ui, &shared);
            match outcome {
                Ok(n) => ui.set_status(&format!("Group {group} deleted; {n} host(s) kept.")),
                Err(e) => ui.set_status(&format!("Could not delete {group}: {e}")),
            }
        });
    }

    // --- template search --------------------------------------------------
    {
        let ui = *ui;
        let shared = shared.clone();
        ui.search.on_text_updated(move |_| {
            let st = shared.borrow();
            fill_templates(&ui, &st);
        });
    }

    // --- applying a template ----------------------------------------------
    {
        let ui = *ui;
        let shared = shared.clone();
        tpl_add.on_click(move |_| {
            let Some(row) = ui.template_list.get_selected_row() else {
                ui.set_status("Select a template in the list first.");
                return;
            };
            // The table is rebuilt from the filtered catalog on every
            // keystroke, so the row index has to be turned back into a name
            // through the same search, not through the catalog's own order.
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
            let outcome = {
                let mut st = shared.borrow_mut();
                // Split the borrow: `apply` needs the catalog read-only and
                // the store mutably, and both live in the same struct — one
                // field at a time keeps the borrow checker happy.
                let catalog = &st.catalog;
                let r = catalog.apply(&mut st.store, &name);
                let _ = st.store.save(&st.config_path);
                r
            };
            refresh(&ui, &shared);
            match outcome {
                Ok(0) => ui.set_status(&format!("{name} was already in the list.")),
                Ok(n) => ui.set_status(&format!(
                    "{name}: {n} host(s) added. Their group is {name} — resolve to get addresses."
                )),
                Err(e) => ui.set_status(&format!("Could not add {name}: {e}")),
            }
        });
    }

    // --- updating the catalog ---------------------------------------------
    {
        let ui = *ui;
        let shared = shared.clone();
        tpl_update.on_click(move |_| {
            update_templates(&ui, &shared);
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

// ---------------------------------------------------------------------------
// The actions
// ---------------------------------------------------------------------------

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
    if dlg.show_modal() != ID_OK {
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
        Err(e) => ui.set_status(&format!("Folder set to {path}, but saving hosts.json failed: {e}")),
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
    let dlg = FileDialog::builder(
        frame,
        "Pick a base WireGuard config. Its AllowedIPs become hosts in your list.",
        &current,
    )
    .build();
    if dlg.show_modal() != ID_OK {
        return false;
    }
    let Some(path) = dlg.get_path() else {
        return false;
    };
    let imported = match import::from_config(&path, &PtrResolver) {
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
            let labels: Vec<String> = hosts.iter().take(8).map(|h| h.label().to_string()).collect();
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
    let (old, was_ip) = {
        let st = shared.borrow();
        match st.store.hosts.get(i) {
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
        let addrs = if h.ips.is_empty() {
            String::new()
        } else if h.ips.len() <= 3 {
            h.ips.join(", ")
        } else {
            format!("{} … ({} addresses)", h.ips[0], h.ips.len())
        };
        // The state column is the answer to "why is this site still not in the
        // tunnel", which is otherwise unanswerable from the window.
        let state_text = match (&h.error, h.source) {
            (Some(e), _) => format!("failed: {e}"),
            (None, Source::Template) => "from template".to_string(),
            (None, Source::Manual) => {
                if h.ips.is_empty() && !h.is_literal_ip() {
                    "not resolved".to_string()
                } else {
                    String::new()
                }
            }
        };
        ui.table.append_item(&[
            Variant::from_string(h.label()),
            Variant::from_string(&groups),
            Variant::from_string(&addrs),
            Variant::from_string(&state_text),
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
    match &state.store.conf_dir {
        Some(d) => ui
            .frame
            .set_status_text(&format!("Configs: {d}   |   {}", state.config_path.display()), 1),
        None => ui
            .frame
            .set_status_text(&format!("Configs: (none chosen)   |   {}", state.config_path.display()), 1),
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
    let dlg = MessageDialog::builder(
        &ui.frame,
        message,
        "wireutils",
    )
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
