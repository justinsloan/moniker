//! A profile kept on this machine only (issue #10): the two questions the
//! profile page asks before it changes, their texts, and the mark the profile
//! lists show on a kept profile.
//!
//! Keeping a profile here deletes it from the other machines the next time they
//! sync, with its stored password, and syncing it again copies it out with the
//! password, which cannot be called back. So both directions ask first, in an
//! `AdwAlertDialog` that names the profile. The name comes from a profile that
//! may have arrived from another machine, so the dialogs show it as plain text,
//! never as Pango markup.

use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use moniker_core::profile::Profile;

/// The title of the profile page's switch.
pub(crate) const ROW_TITLE: &str = "Sync to other machines";

/// The switch's subtitle while the profile syncs.
pub(crate) const SYNCS_SUBTITLE: &str = "Copied to your other machines when they sync";

/// The switch's subtitle while the profile is kept on this machine only.
pub(crate) const KEPT_SUBTITLE: &str = "On this machine only";

/// What the mark of a kept profile says: its tooltip, and its name for a screen
/// reader. A constant, not profile data.
const MARK_TEXT: &str = "On this machine only";

/// What the next sync does to a profile that is kept here, said before it is.
const KEEP_BODY: &str = "It will be deleted from your other machines, with its stored password, \
     the next time they sync. It stays on this machine.";

/// What the next sync does to a kept profile that syncs again.
const SYNC_BODY: &str =
    "It will be copied to your other machines the next time they sync, with its stored password.";

/// The question before a profile is kept on this machine only. Cancel (also
/// Escape) is the default; "Keep on This Machine Only" is the response `keep`.
pub(crate) fn keep_here_dialog(name: &str) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::builder()
        .heading(format!("Keep \u{201c}{name}\u{201d} on This Machine Only?"))
        .heading_use_markup(false)
        .body(KEEP_BODY)
        .body_use_markup(false)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("keep", "Keep on This Machine Only");
    dialog.set_response_appearance("keep", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog
}

/// The question before a kept profile syncs again. Cancel (also Escape) is the
/// default; "Sync" is the response `sync`.
pub(crate) fn sync_again_dialog(name: &str) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::builder()
        .heading(format!(
            "Sync \u{201c}{name}\u{201d} to Your Other Machines?"
        ))
        .heading_use_markup(false)
        .body(SYNC_BODY)
        .body_use_markup(false)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("sync", "Sync");
    dialog.set_response_appearance("sync", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog
}

/// The mark a profile list shows on a profile that is kept on this machine only:
/// a dimmed computer icon, to go in the row as a suffix (in the new-tab list
/// before the "›" button). `None` for a profile that syncs, as every profile
/// does unless kept: only the exception is marked.
///
/// A plain image that says "On this machine only" in its tooltip and to a screen
/// reader (role image, with that as its name). It is not focusable: it is
/// something to read, not something to operate. Both texts are constants, set
/// as plain text.
pub(crate) fn mark(profile: &Profile) -> Option<gtk::Image> {
    profile.local_only.then(|| {
        let mark = gtk::Image::builder()
            .icon_name("computer-symbolic")
            .css_classes(["dim-label"])
            .accessible_role(gtk::AccessibleRole::Img)
            .focusable(false)
            .valign(gtk::Align::Center)
            .build();
        mark.set_tooltip_text(Some(MARK_TEXT));
        mark.update_property(&[gtk::accessible::Property::Label(MARK_TEXT)]);
        mark
    })
}
