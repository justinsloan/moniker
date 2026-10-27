//! Saved sessions in a window (issue #12): a window's tabs and panes as the
//! records a session keeps ([`capture`]), and records added back to a window
//! as tabs ([`restore`]).
//!
//! # Capturing
//!
//! [`capture`] reads every tab of a window, in order: the arrangement of its
//! panes ([`panes::tree_of`]: each split's orientation and where its divider
//! is) and, for each pane, what it shows ([`PaneKind`]), the name the person
//! gave it and its text size, and which pane is the tab's focused one. Not
//! whether it saves a transcript: that is decided again when it is restored. A
//! local pane is recorded with the folder its terminal last reported for this
//! machine ([`terminal_tab::current_folder`], the shell's OSC 7); a
//! command started with `moniker -- COMMAND` is recorded as a local pane in that
//! folder, or the one it was started in, never as its command, and so is a pane
//! that runs one of Local terminal's startup sequences (issue #13), never its
//! commands. A pane that
//! waits for the keyring, or whose session has ended, is its profile; an empty
//! one is the list. Nothing else is read: no host, user name, password, command
//! line, sequence, environment or text of a terminal, and the folder is read
//! once, when the session is saved.
//!
//! # Restoring
//!
//! [`restore`] adds the tabs after the window's own, the first added selected.
//! Each tab's panes are made at their recorded text sizes and arranged as
//! recorded ([`panes::layout_from`]: a split takes its fraction once its tab is
//! first shown, as a tab not selected has no size before), the recorded names
//! given back as renames and the recorded pane made the focused one. Then the
//! panes are filled in reading order, tab after tab: a profile pane connects as
//! a plain click does ([`terminal_tab::start_profile`], with the profile's
//! default sequence), so the connections ask the keyring worker one after the
//! other in pane order (issue #4) and nothing here reads the keyring or the
//! profile file; a profile no longer saved shows the new-tab list with a note
//! naming it as the session did; a local pane opens a shell in its folder, or
//! in the home folder when it has none, or it is gone or cannot be entered,
//! running Local terminal's default sequence there first if it has one
//! ([`terminal_tab::start_local_in`], issue #13), as a profile pane runs its
//! profile's; a list pane shows the list. Local terminal's sequences and
//! switch are read from the settings file once per restore.
//!
//! A restored pane saves a transcript exactly as a plain click would now (user,
//! 2026-10-26), whatever it did when the session was saved: a profile's pane as
//! [`terminal_tab::default_save`] says for the profile's own "Save transcript"
//! switch and "Save transcripts of all sessions", a local pane as it says for
//! Local terminal's own switch (issue #13) and the latter. Turning them off
//! therefore stops restored panes too.

use std::rc::Rc;

use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::context::AppContext;
use crate::launch_args::one_line;
use crate::new_tab;
use crate::panes::{self, TabLayout, Tree};
use crate::session_file::{self, Kind, Node, PaneRecord, TabRecord};
use crate::settings::GuiSettings;
use crate::terminal_tab::{self, PaneHolder, PaneKind, TabTitle};

/// Every tab of `view`, in order, as a session records it.
pub(crate) fn capture(view: &adw::TabView) -> Vec<TabRecord> {
    (0..view.n_pages())
        .filter_map(|index| panes::layout_of_page(&view.nth_page(index)))
        .filter_map(|layout| tab_record(&layout))
        .collect()
}

/// The record of the tab whose panes `layout` holds; none for one whose panes
/// have all gone.
fn tab_record(layout: &TabLayout) -> Option<TabRecord> {
    let tree = panes::tree_of(layout)?;
    let focused_holder = layout.focused_holder();
    let mut index = 0;
    let mut focused = 0;
    let tree = tree.map(&mut |holder: PaneHolder| {
        if focused_holder.as_ref() == Some(&holder) {
            focused = index;
        }
        index += 1;
        pane_record(&holder)
    });
    Some(TabRecord {
        root: record_of_tree(tree),
        focused,
    })
}

/// What a session keeps of the pane of `holder`.
fn pane_record(holder: &PaneHolder) -> PaneRecord {
    let kind = match &*holder.kind().borrow() {
        PaneKind::Empty | PaneKind::List => Kind::List,
        PaneKind::Local { started_in } => Kind::Local {
            dir: terminal_tab::terminal_in(holder)
                .as_ref()
                .and_then(terminal_tab::current_folder)
                .or_else(|| started_in.clone()),
        },
        PaneKind::Profile { id, name } => Kind::Profile {
            id: id.clone(),
            name: kept(name),
        },
    };
    PaneRecord {
        kind,
        // A name of nothing is no name.
        name: holder
            .title()
            .borrow()
            .renamed()
            .filter(|name| !name.is_empty())
            .map(kept),
        scale: holder.scale().get(),
    }
}

/// `text` as a session keeps a name: on one line and at most
/// [`session_file::MAX_LABEL_CHARS`] characters long, as the file takes it.
fn kept(text: &str) -> String {
    one_line(text)
        .chars()
        .take(session_file::MAX_LABEL_CHARS)
        .collect()
}

/// Add `tabs` to `view`, after its own tabs, and select the first added
/// (module docs).
pub(crate) fn restore(view: &adw::TabView, ctx: &Rc<AppContext>, tabs: &[TabRecord]) {
    // Read once: whether every session saves a transcript, and Local
    // terminal's default sequence and own switch.
    let settings = ctx.settings();
    let mut added = Vec::new();
    let mut to_fill = Vec::new();
    for tab in tabs {
        let records = tree_of_record(&tab.root);
        // A list pane is called as one opened with ＋ is, or one made by a
        // split.
        let title = match records {
            Tree::Leaf(_) => new_tab::TITLE,
            Tree::Split { .. } => panes::NEW_PANE_TITLE,
        };
        let mut holders = Vec::new();
        let tree = records.map(&mut |record: &PaneRecord| {
            let holder = PaneHolder::new(TabTitle::remote(title.to_owned()), record.scale);
            holders.push((holder.clone(), record));
            holder
        });
        let layout = panes::layout_from(tree);
        let focused = holders
            .get(tab.focused)
            .or(holders.first())
            .map(|(holder, _)| holder.clone());
        if let Some(focused) = &focused {
            layout.set_focused(focused);
        }
        added.push((panes::append_layout(view, &layout), layout, focused));
        to_fill.extend(holders);
    }
    // In reading order, tab after tab: the profile panes ask the keyring
    // worker in that order.
    for (holder, record) in to_fill {
        if let Some(name) = &record.name {
            terminal_tab::rename_pane(&holder, name);
        }
        fill(&holder, record, ctx, &settings);
    }
    if let Some((page, layout, focused)) = added.first() {
        view.set_selected_page(page);
        // Selecting the tab gives the keyboard to the first of its panes that
        // takes it when the focused one cannot yet (it waits for the keyring),
        // which makes that one the focused pane: the recorded one is made so
        // again, and given the keyboard (or the keyboard taken from the
        // others) if it may have it.
        if let Some(focused) = focused {
            layout.set_focused(focused);
            if panes::in_front(focused) {
                panes::focus_pane(focused);
            }
        }
    }
}

/// Fill the fresh pane of `holder` with what `record` says it was, with the
/// `settings` read for the whole restore: a local pane runs Local terminal's
/// default sequence, as a profile pane runs its profile's. A transcript is
/// saved as a plain click would save one (module docs).
fn fill(holder: &PaneHolder, record: &PaneRecord, ctx: &Rc<AppContext>, settings: &GuiSettings) {
    match &record.kind {
        Kind::Profile { id, name } => {
            // Bind first so the store borrow ends before connecting.
            let profile = ctx.store.borrow().get(id).cloned();
            match profile {
                Some(profile) => {
                    let save = terminal_tab::default_save(
                        profile.transcript,
                        settings.save_all_transcripts,
                    );
                    terminal_tab::start_profile(
                        holder,
                        ctx,
                        &profile,
                        profile.startup_choice(),
                        save,
                    );
                }
                None => new_tab::fill_with_note(holder, ctx, &gone_note(name)),
            }
        }
        Kind::Local { dir } => {
            let dir = dir.clone().unwrap_or_else(glib::home_dir);
            let save = terminal_tab::default_local_save(settings);
            let transcript = terminal_tab::local_transcript_spec(ctx, save);
            let sequence = settings.local_terminal.default_sequence();
            terminal_tab::start_local_in(holder, &dir, sequence, transcript);
        }
        Kind::List => new_tab::fill(holder, ctx),
    }
}

/// What a pane whose profile has gone says above the list: the profile named
/// as the session recorded it (plain text), or, for a record with no name at
/// all (only a hand-edited file has one), the pane itself.
fn gone_note(name: &str) -> String {
    if name.trim().is_empty() {
        "This pane's profile is no longer saved.".to_owned()
    } else {
        format!("{name} is no longer saved.")
    }
}

/// A tab's record as a tree of its panes' records.
fn tree_of_record(node: &Node) -> Tree<&PaneRecord> {
    match node {
        Node::Pane(pane) => Tree::Leaf(pane),
        Node::Split {
            horizontal,
            fraction,
            start,
            end,
        } => Tree::Split {
            orientation: if *horizontal {
                gtk::Orientation::Horizontal
            } else {
                gtk::Orientation::Vertical
            },
            fraction: *fraction,
            start: Box::new(tree_of_record(start)),
            end: Box::new(tree_of_record(end)),
        },
    }
}

/// The record of a tree of panes' records.
fn record_of_tree(tree: Tree<PaneRecord>) -> Node {
    match tree {
        Tree::Leaf(pane) => Node::Pane(pane),
        Tree::Split {
            orientation,
            fraction,
            start,
            end,
        } => Node::Split {
            horizontal: orientation == gtk::Orientation::Horizontal,
            fraction,
            start: Box::new(record_of_tree(*start)),
            end: Box::new(record_of_tree(*end)),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::session_file::Kind;

    fn pane(kind: Kind, name: Option<&str>, scale: f64) -> PaneRecord {
        PaneRecord {
            kind,
            name: name.map(str::to_owned),
            scale,
        }
    }

    /// Side by side: a local pane, then one above the other a profile and the
    /// list; the second split nested in the first.
    fn three_ways() -> Node {
        Node::Split {
            horizontal: true,
            fraction: 0.3,
            start: Box::new(Node::Pane(pane(
                Kind::Local {
                    dir: Some(PathBuf::from("/home/u/My Files/Ünï")),
                },
                None,
                1.0,
            ))),
            end: Box::new(Node::Split {
                horizontal: false,
                fraction: 0.7,
                start: Box::new(Node::Pane(pane(
                    Kind::Profile {
                        id: "id-1".to_owned(),
                        name: "web".to_owned(),
                    },
                    Some("Logs"),
                    1.5,
                ))),
                end: Box::new(Node::Pane(pane(Kind::List, None, 0.8))),
            }),
        }
    }

    #[test]
    fn a_gone_profile_is_named_or_the_pane_is() {
        assert_eq!(gone_note("web"), "web is no longer saved.");
        assert_eq!(
            gone_note("<b>x</b> &amp;"),
            "<b>x</b> &amp; is no longer saved.",
            "as it is"
        );
        for none in ["", "  ", "\t"] {
            assert_eq!(
                gone_note(none),
                "This pane's profile is no longer saved.",
                "{none:?}"
            );
        }
    }

    #[test]
    fn records_and_trees_convert_both_ways() {
        let node = three_ways();
        let tree = tree_of_record(&node);
        // The shape, the orientations and the fractions, the panes in reading
        // order.
        let Tree::Split {
            orientation,
            fraction,
            start,
            end,
        } = &tree
        else {
            panic!("a split at the top: {tree:?}");
        };
        assert_eq!(*orientation, gtk::Orientation::Horizontal);
        assert_eq!(*fraction, 0.3);
        assert!(matches!(
            **start,
            Tree::Leaf(PaneRecord {
                kind: Kind::Local { .. },
                ..
            })
        ));
        let Tree::Split {
            orientation,
            fraction,
            start: upper,
            end: lower,
        } = &**end
        else {
            panic!("a split on the right: {end:?}");
        };
        assert_eq!(*orientation, gtk::Orientation::Vertical);
        assert_eq!(*fraction, 0.7);
        assert!(matches!(
            **upper,
            Tree::Leaf(PaneRecord { name: Some(_), .. })
        ));
        assert!(matches!(
            **lower,
            Tree::Leaf(PaneRecord {
                kind: Kind::List,
                ..
            })
        ));
        let mut order = Vec::new();
        let back = record_of_tree(tree.map(&mut |pane: &PaneRecord| {
            order.push(pane.scale);
            pane.clone()
        }));
        assert_eq!(order, [1.0, 1.5, 0.8], "reading order");
        assert_eq!(back, node, "and back again, the same");

        // A tab of one pane is that pane.
        let alone = Node::Pane(pane(Kind::List, Some("x"), 1.0));
        let tree = tree_of_record(&alone);
        assert!(matches!(tree, Tree::Leaf(_)));
        assert_eq!(record_of_tree(tree.map(&mut |pane| pane.clone())), alone);
    }
}
