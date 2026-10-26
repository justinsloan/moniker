//! A profile kept on this machine only (issue #10): the two questions the
//! profile page asks before it changes, and their texts.
//!
//! Keeping a profile here deletes it from the other machines the next time they
//! sync, with its stored password, and syncing it again copies it out with the
//! password, which cannot be called back. So both directions ask first, in an
//! `AdwAlertDialog` that names the profile. The name comes from a profile that
//! may have arrived from another machine, so the dialogs show it as plain text,
//! never as Pango markup.

use libadwaita as adw;
use libadwaita::prelude::*;

/// The title of the profile page's switch.
pub(crate) const ROW_TITLE: &str = "Sync to other machines";

/// The switch's subtitle while the profile syncs.
pub(crate) const SYNCS_SUBTITLE: &str = "Copied to your other machines when they sync";

/// The switch's subtitle while the profile is kept on this machine only.
pub(crate) const KEPT_SUBTITLE: &str = "On this machine only";

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
        .heading(format!("Keep \"{name}\" on This Machine Only?"))
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
        .heading(format!("Sync \"{name}\" to Your Other Machines?"))
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
