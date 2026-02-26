use cosmic::iced::Length;
use cosmic::widget;
use cosmic::widget::text_editor;
use cosmic::Element;

use crate::app::Message;
use nevermail_core::models::AttachmentData;

#[derive(Debug, Clone, PartialEq)]
pub enum ComposeMode {
    New,
    Reply,
    Forward,
}

fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[allow(clippy::too_many_arguments)]
pub fn view<'a>(
    mode: &ComposeMode,
    account_labels: &'a [String],
    selected_account: usize,
    from_addresses: &'a [String],
    from_selected: usize,
    to: &'a str,
    subject: &'a str,
    body: &'a text_editor::Content,
    attachments: &[AttachmentData],
    error: Option<&'a str>,
    is_sending: bool,
    drag_hover: bool,
) -> Element<'a, Message> {
    let title = match mode {
        ComposeMode::New => "New Message",
        ComposeMode::Reply => "Reply",
        ComposeMode::Forward => "Forward",
    };

    let mut controls = widget::column().spacing(12);

    // Account selector (shown when >1 account)
    if account_labels.len() > 1 {
        controls = controls.push(
            widget::column()
                .spacing(4)
                .push(widget::text::body("Account"))
                .push(widget::dropdown(
                    account_labels,
                    Some(selected_account),
                    Message::ComposeAccountChanged,
                )),
        );
    }

    if from_addresses.len() > 1 {
        controls = controls.push(
            widget::column()
                .spacing(4)
                .push(widget::text::body("From"))
                .push(widget::dropdown(
                    from_addresses,
                    Some(from_selected),
                    Message::ComposeFromChanged,
                )),
        );
    } else if let Some(addr) = from_addresses.first() {
        controls = controls.push(
            widget::column()
                .spacing(4)
                .push(widget::text::body("From"))
                .push(widget::text::caption(addr)),
        );
    }

    controls = controls
        .push(
            widget::text_input("recipient@example.com", to)
                .label("To")
                .on_input(Message::ComposeToChanged),
        )
        .push(
            widget::text_input("Subject", subject)
                .label("Subject")
                .on_input(Message::ComposeSubjectChanged),
        )
        .push(
            widget::text_editor(body)
                .placeholder("Write your message...")
                .on_action(Message::ComposeBodyAction)
                .height(Length::Fixed(300.0)),
        );

    // Attachment section (visual only — actual DnD destination is in the main view
    // because COSMIC dialog overlays don't propagate drag_destinations to the compositor)
    let mut attach_col = widget::column().spacing(6);
    let attach_label = if drag_hover {
        "Drop files to attach"
    } else {
        "Attach files"
    };
    attach_col = attach_col.push(
        widget::button::standard(attach_label).on_press(Message::ComposeAttach),
    );
    if !attachments.is_empty() {
        for (i, att) in attachments.iter().enumerate() {
            let label = format!("{} ({})", att.filename, format_size(att.data.len()));
            let row = widget::row()
                .spacing(8)
                .align_y(cosmic::iced::Alignment::Center)
                .push(widget::text::body(label))
                .push(
                    widget::button::destructive("Remove")
                        .on_press(Message::ComposeRemoveAttachment(i)),
                );
            attach_col = attach_col.push(row);
        }
    }
    controls = controls.push(attach_col);

    let send_label = if is_sending { "Sending..." } else { "Send" };
    let send_btn = if is_sending {
        widget::button::suggested(send_label)
    } else {
        widget::button::suggested(send_label).on_press(Message::ComposeSend)
    };

    let mut dialog = widget::dialog()
        .title(title)
        .control(controls)
        .primary_action(send_btn)
        .secondary_action(widget::button::standard("Cancel").on_press(Message::ComposeCancel));

    if let Some(err) = error {
        dialog = dialog.body(err);
    }

    dialog.into()
}
