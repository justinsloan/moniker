//! The New Profile dialog (＋ in the Connection Profiles window).
//!
//! A profile cannot be built up field by field from blank on the profile
//! page: name, host and username are required before anything validates. So
//! ＋ asks for those (plus port and an optional password) in a small dialog,
//! creates the profile with [`form::create_profile`] (password to the keyring
//! first) on the keyring worker ([`keyring::run`], so a locked keyring never
//! freezes the window), and hands the new id back so the window selects it;
//! everything else is edited on its page. While the profile is being created,
//! Create is off. Errors are shown inside the dialog, which stays open so they
//! can be fixed, and never contain the password.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use zeroize::Zeroizing;

use crate::context::AppContext;
use crate::form::{self, NewProfile};
use crate::keyring;

/// The dialog's widgets.
struct Fields {
    name: adw::EntryRow,
    host: adw::EntryRow,
    username: adw::EntryRow,
    port: adw::SpinRow,
    password: adw::PasswordEntryRow,
    create: gtk::Button,
    error: gtk::Label,
    /// The profile is being created: Create stays off until that is back.
    busy: Cell<bool>,
}

impl Fields {
    /// Create is offered once every required field has something in it, and
    /// no profile is being created.
    fn update_create(&self) {
        let filled = |row: &adw::EntryRow| !row.text().trim().is_empty();
        self.create.set_sensitive(
            !self.busy.get() && filled(&self.name) && filled(&self.host) && filled(&self.username),
        );
    }

    fn values(&self) -> NewProfile {
        // A typed but uncommitted port counts.
        self.port.update();
        NewProfile {
            name: self.name.text().to_string(),
            host: self.host.text().to_string(),
            username: self.username.text().to_string(),
            // The spin row is limited to 1..=65535.
            port: u16::try_from(self.port.value().round() as i64).unwrap_or(0),
        }
    }
}

/// Open the dialog over `parent`. `on_created` gets the new profile's id
/// after it is saved and the dialog has closed.
pub(super) fn show(
    parent: &adw::Window,
    ctx: &Rc<AppContext>,
    on_created: impl Fn(&str) + 'static,
) {
    let entry = |title: &str| {
        adw::EntryRow::builder()
            .title(title)
            .use_markup(false)
            .build()
    };
    let fields = Rc::new(Fields {
        name: entry("Name"),
        host: entry("Host"),
        username: entry("Username"),
        port: adw::SpinRow::builder()
            .title("Port")
            .adjustment(&gtk::Adjustment::new(22.0, 1.0, 65535.0, 1.0, 10.0, 0.0))
            .numeric(true)
            .build(),
        password: adw::PasswordEntryRow::builder()
            .title("Password (optional)")
            .use_markup(false)
            .build(),
        create: gtk::Button::builder()
            .label("Create")
            .css_classes(["suggested-action"])
            .sensitive(false)
            .build(),
        error: gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .selectable(true)
            .css_classes(["error"])
            .visible(false)
            .build(),
        busy: Cell::new(false),
    });

    let group = adw::PreferencesGroup::new();
    group.add(&fields.name);
    group.add(&fields.host);
    group.add(&fields.username);
    group.add(&fields.port);
    group.add(&fields.password);
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    column.append(&group);
    column.append(&fields.error);

    let cancel = gtk::Button::with_label("Cancel");
    let header = adw::HeaderBar::builder()
        .show_start_title_buttons(false)
        .show_end_title_buttons(false)
        .build();
    header.pack_start(&cancel);
    header.pack_end(&fields.create);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&column));
    let dialog = adw::Dialog::builder()
        .title("New Profile")
        .content_width(420)
        .child(&view)
        .focus_widget(&fields.name)
        .build();

    for row in [&fields.name, &fields.host, &fields.username] {
        let weak = Rc::downgrade(&fields);
        row.connect_changed(move |_| {
            if let Some(fields) = weak.upgrade() {
                fields.update_create();
            }
        });
    }
    // Enter in any row creates, once Create is offered.
    for row in [&fields.name, &fields.host, &fields.username] {
        row.connect_entry_activated(glib::clone!(
            #[weak(rename_to = create)]
            fields.create,
            move |_| {
                if create.is_sensitive() {
                    create.emit_clicked();
                }
            }
        ));
    }
    fields.password.connect_entry_activated(glib::clone!(
        #[weak(rename_to = create)]
        fields.create,
        move |_| {
            if create.is_sensitive() {
                create.emit_clicked();
            }
        }
    ));

    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));

    let weak = Rc::downgrade(&fields);
    let ctx = Rc::clone(ctx);
    let on_created = Rc::new(on_created);
    fields.create.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            let Some(fields) = weak.upgrade() else {
                return;
            };
            if fields.busy.replace(true) {
                return;
            }
            // A second click must not create a duplicate.
            fields.update_create();
            // Copied once out of GTK into a wrapper that wipes it on drop.
            let typed = Zeroizing::new(fields.password.text().to_string());
            let values = fields.values();
            let store_path = ctx.store.borrow().path().to_path_buf();
            let weak = Rc::downgrade(&fields);
            let ctx = Rc::clone(&ctx);
            let on_created = Rc::clone(&on_created);
            let dialog = dialog.clone();
            keyring::run(
                &ctx.clone(),
                move |secrets| {
                    let store = keyring::own_store(&store_path)?;
                    form::create_profile(&store, secrets, &values, &typed)
                },
                // Held strongly: a profile created after Cancel was pressed
                // is still shown (the dialog is closed already then).
                move |created| {
                    let created = created.unwrap_or_else(|| {
                        Err(format!(
                            "Could not create the profile: {}.",
                            keyring::STOPPED
                        ))
                    });
                    match created {
                        Ok(id) => {
                            dialog.close();
                            ctx.profiles_changed();
                            on_created(&id);
                        }
                        Err(message) => {
                            if let Some(fields) = weak.upgrade() {
                                fields.busy.set(false);
                                fields.update_create();
                                fields.error.set_label(&message);
                                fields.error.set_visible(true);
                            }
                        }
                    }
                },
            );
        }
    ));

    // Never leave a typed password in a closed dialog. This handler also owns
    // the fields (the others hold them weakly), so they live as long as the
    // dialog does.
    dialog.connect_closed(move |_| fields.password.set_text(""));

    dialog.present(Some(parent));
}
