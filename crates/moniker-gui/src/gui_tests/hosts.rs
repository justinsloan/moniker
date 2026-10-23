//! Scenarios: the host status marks of the profile lists (issue #5). Every
//! check here is a fake set on the context (`hosts::Hosts::set_check`): no
//! test sends a ping or runs `ssh -G`.

use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::profile::Profile;

use super::main_window::{ctx_with, test_app};
use super::windows::{list_in, main_window, row};
use super::{all_widgets, pump_for, pump_until};
use crate::context::AppContext;
use crate::hosts::{MARK_CLASS, Reach};
use crate::{new_tab, preferences, profiles, window};

/// The host mark in `root` (a row), if it has one.
fn mark_in(root: &impl IsA<gtk::Widget>) -> Option<gtk::Box> {
    all_widgets::<gtk::Box>(root)
        .into_iter()
        .find(|b| b.has_css_class(MARK_CLASS))
}

/// What `mark` shows: its shape and color classes and its tooltip.
fn shown(mark: &gtk::Box) -> (String, String, String) {
    let shape = ["answers", "no-reply", "unknown"]
        .into_iter()
        .find(|c| mark.has_css_class(c))
        .unwrap_or("none");
    let color = ["success", "error", "dim-label"]
        .into_iter()
        .find(|c| mark.has_css_class(c))
        .unwrap_or("none");
    (
        shape.to_owned(),
        color.to_owned(),
        mark.tooltip_text()
            .map(|t| t.to_string())
            .unwrap_or_default(),
    )
}

fn showing(shape: &str, color: &str, text: &str) -> (String, String, String) {
    (shape.to_owned(), color.to_owned(), text.to_owned())
}

/// A check that answers by host, and the hosts it was asked about.
fn scripted(ctx: &AppContext) -> Arc<Mutex<Vec<String>>> {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&asked);
    ctx.hosts.set_check(Arc::new(move |p: &Profile| {
        let host = p.host.to_ascii_lowercase();
        record.lock().unwrap().push(host.clone());
        match host.as_str() {
            "up.example.test" => Reach::Answers,
            "down.example.test" => Reach::NoReply,
            "hop.example.test" => Reach::Proxied,
            _ => Reach::NotAllowed,
        }
    }));
    asked
}

fn up() -> Profile {
    Profile::new("Up", "Up.Example.Test", "alice")
}

/// Both lists show a mark before each profile's name: a filled dot in the
/// success color for a host that answers, a ring in the error color for one
/// that does not, a dimmed dash for one that is not pinged, each with its
/// text. Local terminal and the Sync entry have none. One status per host for
/// the whole app: two profiles on one host, shown in two lists, are one check.
pub(super) fn both_lists_mark_each_hosts_status(dir: &Path) {
    let up = up();
    // The same host as `up`, spelled otherwise.
    let twin = Profile::new("Twin", "up.example.test", "bob");
    let down = Profile::new("Down", "down.example.test", "alice");
    let hop = Profile::new("Hop", "hop.example.test", "alice");
    let ctx = ctx_with(dir, &[up.clone(), twin.clone(), down.clone(), hop.clone()]);
    let asked = scripted(&ctx);
    let app = test_app();
    let main = main_window(&app, &ctx);

    let page = new_tab::open(&main.view, &ctx);
    let list = list_in(&page).expect("the list");
    let mark = |p: &Profile| mark_in(&row(&list, &p.id)).expect("a mark");
    pump_until("the marks to be decided", || {
        shown(&mark(&down)).0 == "no-reply" && shown(&mark(&up)).0 == "answers"
    });
    assert_eq!(
        shown(&mark(&up)),
        showing("answers", "success", "Answers ping")
    );
    assert_eq!(
        shown(&mark(&twin)),
        shown(&mark(&up)),
        "one status per host"
    );
    assert_eq!(
        shown(&mark(&down)),
        showing("no-reply", "error", "No reply to ping")
    );
    pump_until("the proxied host's mark", || {
        shown(&mark(&hop)).2.contains("another host")
    });
    assert_eq!(
        shown(&mark(&hop)),
        showing(
            "unknown",
            "dim-label",
            "Not checked: reached through another host"
        )
    );
    assert!(
        mark(&up).is_visible() && !mark(&up).can_target(),
        "shown, not clickable"
    );
    assert!(
        mark_in(&row(&list, new_tab::LOCAL_ROW)).is_none(),
        "no mark on Local terminal"
    );

    let profiles_window = window::open_profiles(&app, &ctx);
    pump_for(Duration::from_millis(300));
    let sidebar = all_widgets::<gtk::ListBox>(&profiles_window)
        .into_iter()
        .find(|list| list.widget_name() == profiles::SIDEBAR_NAME)
        .expect("the sidebar");
    let side_mark = |p: &Profile| {
        all_widgets::<adw::ActionRow>(&sidebar)
            .into_iter()
            .find(|r| r.widget_name() == p.id.as_str())
            .and_then(|r| mark_in(&r))
            .expect("a sidebar mark")
    };
    assert_eq!(
        shown(&side_mark(&up)),
        showing("answers", "success", "Answers ping")
    );
    assert_eq!(
        shown(&side_mark(&down)),
        showing("no-reply", "error", "No reply to ping")
    );
    let sync_entries: Vec<gtk::ListBox> = all_widgets::<gtk::ListBox>(&profiles_window)
        .into_iter()
        .filter(|list| list.widget_name() != profiles::SIDEBAR_NAME)
        .collect();
    assert!(
        sync_entries.iter().all(|list| mark_in(list).is_none()),
        "no mark on the Sync entry"
    );

    let mut asked = asked.lock().unwrap().clone();
    asked.sort();
    assert_eq!(
        asked,
        ["down.example.test", "hop.example.test", "up.example.test"],
        "each host once, for two profiles and two lists"
    );
    profiles_window.destroy();
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// A mark is gray until a check has decided, and a later check keeps what it
/// showed until it decides in turn: nothing flickers. While a mark is in view
/// its hosts are checked again at an interval; with none in view, nothing is.
pub(super) fn a_mark_waits_for_its_check_and_keeps_its_state(dir: &Path) {
    let up = up();
    let ctx = ctx_with(dir, std::slice::from_ref(&up));
    let (answer, answers) = mpsc::channel::<Reach>();
    let answers = Arc::new(Mutex::new(answers));
    ctx.hosts.set_check(Arc::new(move |_: &Profile| {
        answers.lock().unwrap().recv().unwrap_or(Reach::Unknown)
    }));
    let app = test_app();
    let main = main_window(&app, &ctx);
    let page = new_tab::open(&main.view, &ctx);
    let list = list_in(&page).expect("the list");
    let mark = || mark_in(&row(&list, &up.id)).expect("a mark");
    pump_until("the mark to be shown", || mark().is_mapped());
    assert_eq!(
        shown(&mark()),
        showing("unknown", "dim-label", "Not checked yet")
    );
    assert!(ctx.hosts.timer_runs(), "re-checks while in view");
    answer.send(Reach::Answers).unwrap();
    pump_until("the check to decide", || shown(&mark()).0 == "answers");

    // The next check: the dot stays until it decides.
    ctx.hosts.make_stale();
    ctx.hosts.recheck_now();
    pump_for(Duration::from_millis(200));
    assert_eq!(
        shown(&mark()),
        showing("answers", "success", "Answers ping")
    );
    answer.send(Reach::NoReply).unwrap();
    pump_until("the next check to decide", || {
        shown(&mark()).0 == "no-reply"
    });

    // Out of view (the tab closed): no more re-checks.
    main.view.close_page(&page);
    pump_until("the list to go", || !mark().is_mapped());
    assert!(!ctx.hosts.timer_runs(), "nothing is in view");
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}

/// The Preferences switch (on by default): turned off, no mark is shown and no
/// check runs, not even for a list opened then; turned on again, the marks
/// come back as "not checked yet" and their hosts are checked.
pub(super) fn turned_off_there_is_no_mark_and_no_ping(dir: &Path) {
    let up = up();
    let ctx = ctx_with(dir, std::slice::from_ref(&up));
    let asked = scripted(&ctx);
    let app = test_app();
    let main = main_window(&app, &ctx);
    let first = new_tab::open(&main.view, &ctx);
    let first_list = list_in(&first).expect("the list");
    let first_mark = mark_in(&row(&first_list, &up.id)).expect("a mark");
    pump_until("the first check", || shown(&first_mark).0 == "answers");

    let dialog = preferences::present(&main.window, &app, &ctx);
    pump_until("the Preferences dialog", || {
        main.window.visible_dialog().is_some()
    });
    let switch = all_widgets::<adw::SwitchRow>(&dialog)
        .into_iter()
        .find(|r| r.title() == "Show whether hosts are reachable")
        .expect("the switch");
    assert!(switch.is_active(), "on by default");
    switch.set_active(false);
    assert!(!ctx.settings().ping_hosts, "saved");
    assert!(!first_mark.is_visible(), "no mark");
    dialog.force_close();
    pump_until("the dialog to close", || {
        main.window.visible_dialog().is_none()
    });

    let checks = asked.lock().unwrap().len();
    let second = new_tab::open(&main.view, &ctx);
    let second_list = list_in(&second).expect("the list");
    let second_mark = mark_in(&row(&second_list, &up.id)).expect("a mark");
    pump_for(Duration::from_millis(300));
    assert!(
        !second_mark.is_visible(),
        "no mark on a list opened while off"
    );
    assert_eq!(asked.lock().unwrap().len(), checks, "no check while off");

    // On again: what was known is gone, and the host is checked anew.
    let dialog = preferences::present(&main.window, &app, &ctx);
    pump_until("the Preferences dialog", || {
        main.window.visible_dialog().is_some()
    });
    let switch = all_widgets::<adw::SwitchRow>(&dialog)
        .into_iter()
        .find(|r| r.title() == "Show whether hosts are reachable")
        .expect("the switch");
    assert!(!switch.is_active(), "the dialog shows what was saved");
    switch.set_active(true);
    assert!(ctx.settings().ping_hosts, "saved");
    dialog.force_close();
    pump_until("the dialog to close", || {
        main.window.visible_dialog().is_none()
    });
    assert!(second_mark.is_visible());
    pump_until("a new check", || asked.lock().unwrap().len() > checks);
    pump_until("it to decide", || shown(&second_mark).0 == "answers");
    main.window.destroy();
    pump_for(Duration::from_millis(200));
}
