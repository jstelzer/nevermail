use cosmic::iced::Length;
use cosmic::widget;
use cosmic::Element;

use crate::app::{AccountState, ConnectionState, Message};
use crate::core::models::DraggedMessage;

/// Render the folder sidebar with multi-account sections.
pub fn view<'a>(
    accounts: &'a [AccountState],
    active_account: Option<usize>,
    selected_folder: Option<usize>,
    drag_target: Option<usize>,
) -> Element<'a, Message> {
    let mut col = widget::column().spacing(4).padding(8);

    col = col.push(
        widget::button::suggested("Compose")
            .on_press(Message::ComposeNew)
            .width(Length::Fill),
    );
    col = col.push(widget::vertical_space().height(8));

    if accounts.is_empty() {
        col = col.push(widget::text::body("No accounts configured"));
        col = col.push(
            widget::button::standard("Add Account")
                .on_press(Message::AccountAdd)
                .width(Length::Fill),
        );
    } else {
        // Track a global folder index offset for drag targets
        let mut global_folder_offset: usize = 0;

        for (acct_idx, acct) in accounts.iter().enumerate() {
            let is_active_account = active_account == Some(acct_idx);

            // Account header row: collapse toggle + label + status + edit/remove
            let collapse_icon = if acct.collapsed { "▶" } else { "▼" };
            let status_icon = match &acct.conn_state {
                ConnectionState::Connected => "●",
                ConnectionState::Connecting | ConnectionState::Syncing => "◌",
                ConnectionState::Error(_) => "✖",
                ConnectionState::Disconnected => "○",
            };

            let header_label = format!(
                "{} {} {}",
                collapse_icon, acct.config.label, status_icon
            );

            let aid_edit = acct.config.id.clone();
            let aid_remove = acct.config.id.clone();

            let header_row = widget::row()
                .spacing(2)
                .align_y(cosmic::iced::Alignment::Center)
                .push(
                    widget::button::text(header_label)
                        .on_press(Message::ToggleAccountCollapse(acct_idx))
                        .width(Length::Fill),
                )
                .push(
                    widget::button::icon(widget::icon::from_name("document-properties-symbolic"))
                        .on_press(Message::AccountEdit(aid_edit))
                        .padding(4)
                        .class(cosmic::theme::Button::Text),
                )
                .push(
                    widget::button::icon(widget::icon::from_name("edit-delete-symbolic"))
                        .on_press(Message::AccountRemove(aid_remove))
                        .padding(4)
                        .class(cosmic::theme::Button::Text),
                );

            col = col.push(header_row);

            // Show connection error inline if present
            if let ConnectionState::Error(ref e) = acct.conn_state {
                let short_err = if e.len() > 40 {
                    format!("{}...", &e[..37])
                } else {
                    e.clone()
                };
                let aid = acct.config.id.clone();
                col = col.push(
                    widget::button::custom(
                        widget::container(
                            widget::text::caption(format!("  {short_err} (retry)"))
                        ).padding([2, 8])
                    )
                    .on_press(Message::ForceReconnect(aid))
                    .class(cosmic::theme::Button::Text)
                    .width(Length::Fill),
                );
            }

            // Folder list (when not collapsed)
            if !acct.collapsed {
                if acct.folders.is_empty() {
                    match &acct.conn_state {
                        ConnectionState::Connecting | ConnectionState::Syncing => {
                            col = col.push(widget::text::caption("  Loading..."));
                        }
                        _ => {
                            col = col.push(widget::text::caption("  No folders"));
                        }
                    }
                } else {
                    for (folder_idx, folder) in acct.folders.iter().enumerate() {
                        let global_idx = global_folder_offset + folder_idx;
                        let label = if folder.unread_count > 0 {
                            format!("  {} ({})", folder.name, folder.unread_count)
                        } else {
                            format!("  {}", folder.name)
                        };

                        let is_selected = is_active_account && selected_folder == Some(folder_idx);
                        let is_drag_target = drag_target == Some(global_idx);

                        let ai = acct_idx;
                        let fi = folder_idx;
                        let mut btn = widget::button::text(label)
                            .on_press(Message::SelectFolder(ai, fi))
                            .width(Length::Fill);

                        if is_selected || is_drag_target {
                            btn = btn.class(cosmic::theme::Button::Suggested);
                        }

                        let mailbox_hash = folder.mailbox_hash;
                        let dest =
                            widget::dnd_destination::dnd_destination_for_data::<DraggedMessage, _>(
                                btn,
                                move |data, _action| match data {
                                    Some(msg) => Message::DragMessageToFolder {
                                        envelope_hash: msg.envelope_hash,
                                        source_mailbox: msg.source_mailbox,
                                        dest_mailbox: mailbox_hash,
                                    },
                                    None => Message::Noop,
                                },
                            )
                            .on_enter(move |_x, _y, _mimes| Message::FolderDragEnter(global_idx))
                            .on_leave(|| Message::FolderDragLeave);

                        col = col.push(dest);
                    }
                }
            }

            global_folder_offset += acct.folders.len();

            // Separator between accounts
            if acct_idx < accounts.len() - 1 {
                col = col.push(widget::vertical_space().height(4));
            }
        }

        // Add Account button at the bottom
        col = col.push(widget::vertical_space().height(8));
        col = col.push(
            widget::button::standard("+ Add Account")
                .on_press(Message::AccountAdd)
                .width(Length::Fill),
        );
    }

    let scrollable_folders = widget::scrollable(col).height(Length::Fill);

    widget::column()
        .push(scrollable_folders)
        .height(Length::Fill)
        .into()
}
