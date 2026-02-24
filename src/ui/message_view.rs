use cosmic::iced::{ContentFit, Length};
use cosmic::widget;
use cosmic::widget::{image, text_editor};
use cosmic::Element;

use crate::app::Message;
use crate::core::models::{AttachmentData, MessageSummary};

/// Render the message preview pane with an action toolbar when a message is selected.
pub fn view<'a>(
    body: &'a text_editor::Content,
    selected: Option<(usize, &MessageSummary)>,
    attachments: &[AttachmentData],
    image_handles: &[Option<image::Handle>],
) -> Element<'a, Message> {
    if body.text().is_empty() && attachments.is_empty() {
        return widget::container(widget::text::body("Select a message to read"))
            .padding(16)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }

    let mut col = widget::column().spacing(0);

    if let Some((index, msg)) = selected {
        let star_label = if msg.is_starred { "\u{2605}" } else { "\u{2606}" };
        let read_label = if msg.is_read { "Mark unread" } else { "Mark read" };

        let toolbar = widget::row()
            .spacing(8)
            .push(widget::button::text("Reply").on_press(Message::ComposeReply))
            .push(widget::button::text("Forward").on_press(Message::ComposeForward))
            .push(widget::button::text(star_label).on_press(Message::ToggleStar(index)))
            .push(widget::button::text(read_label).on_press(Message::ToggleRead(index)))
            .push(widget::button::text("Archive").on_press(Message::ArchiveMessage(index)))
            .push(widget::button::destructive("Trash").on_press(Message::TrashMessage(index)));

        col = col.push(
            widget::container(toolbar)
                .padding([8, 16])
                .width(Length::Fill),
        );
    }

    if !body.text().is_empty() {
        let body_content = widget::text_editor(body)
            .on_action(Message::PreviewAction)
            .padding(16)
            .height(Length::Shrink);

        col = col.push(body_content);
    }

    // Attachments section
    if !attachments.is_empty() {
        let mut att_col = widget::column().spacing(8);

        att_col = att_col.push(
            widget::text::heading(format!("Attachments ({})", attachments.len())),
        );

        for (i, att) in attachments.iter().enumerate() {
            let mut card = widget::column().spacing(4);

            // Image preview
            if let Some(Some(handle)) = image_handles.get(i) {
                card = card.push(
                    widget::Image::new(handle.clone())
                        .content_fit(ContentFit::Contain)
                        .width(Length::Fill),
                );
            }

            // Filename, size, save button
            let size_str = human_size(att.data.len());
            let info = widget::row()
                .spacing(8)
                .align_y(cosmic::iced::Alignment::Center)
                .push(
                    widget::text::body(format!("{} ({})", att.filename, size_str))
                        .width(Length::Fill),
                )
                .push(
                    widget::button::suggested("Save")
                        .on_press(Message::SaveAttachment(i)),
                );

            card = card.push(info);

            att_col = att_col.push(
                widget::container(card)
                    .padding(8)
                    .width(Length::Fill)
                    .class(cosmic::style::Container::Card),
            );
        }

        col = col.push(
            widget::container(att_col)
                .padding([8, 16])
                .width(Length::Fill),
        );
    }

    widget::scrollable(col)
        .height(Length::Fill)
        .width(Length::Fill)
        .into()
}

fn human_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
