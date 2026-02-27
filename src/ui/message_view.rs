use cosmic::iced::{ContentFit, Length};
use cosmic::widget;
use cosmic::widget::{image, markdown};
use cosmic::Element;

use crate::app::Message;
use neverlight_mail_core::models::{AttachmentData, MessageSummary};

/// Render the message preview pane with an action toolbar when a message is selected.
pub fn view<'a>(
    markdown_items: &'a [markdown::Item],
    selected: Option<(usize, &'a MessageSummary)>,
    attachments: &[AttachmentData],
    image_handles: &[Option<image::Handle>],
) -> Element<'a, Message> {
    if markdown_items.is_empty() && attachments.is_empty() {
        return widget::container(widget::text::body("Select a message to read"))
            .padding(16)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }

    let mut col = widget::column().spacing(0);

    if let Some((index, msg)) = selected {
        let star_label = if msg.is_starred {
            "\u{2605}"
        } else {
            "\u{2606}"
        };
        let read_label = if msg.is_read {
            "Mark unread"
        } else {
            "Mark read"
        };

        let toolbar = widget::row()
            .spacing(8)
            .push(widget::button::text("Reply").on_press(Message::ComposeReply))
            .push(widget::button::text("Forward").on_press(Message::ComposeForward))
            .push(widget::button::text(star_label).on_press(Message::ToggleStar(index)))
            .push(widget::button::text(read_label).on_press(Message::ToggleRead(index)))
            .push(widget::button::text("Archive").on_press(Message::Archive(index)))
            .push(widget::button::text("Copy").on_press(Message::CopyBody))
            .push(widget::button::destructive("Trash").on_press(Message::Trash(index)));

        col = col.push(
            widget::container(toolbar)
                .padding([8, 16])
                .width(Length::Fill),
        );

        // Message header info
        col = col.push(
            widget::container(message_header(msg))
                .padding([4, 16])
                .width(Length::Fill)
                .class(cosmic::style::Container::Card),
        );
    }

    if !markdown_items.is_empty() {
        let md = markdown::view(
            markdown_items,
            markdown::Settings::default(),
            markdown::Style::from_palette(cosmic::iced::Theme::Dark.palette()),
        )
        .map(Message::LinkClicked);

        col = col.push(widget::container(md).padding(16).width(Length::Fill));
    }

    // Attachments section
    if !attachments.is_empty() {
        let mut att_col = widget::column().spacing(8);

        att_col = att_col.push(widget::text::heading(format!(
            "Attachments ({})",
            attachments.len()
        )));

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
                .push(widget::button::suggested("Save").on_press(Message::SaveAttachment(i)));

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

fn header_row<'a>(label: &'a str, value: &'a str) -> Element<'a, Message> {
    widget::row()
        .spacing(8)
        .push(
            widget::text::body(label)
                .width(Length::Fixed(80.0))
                .font(cosmic::iced::Font {
                    weight: cosmic::iced::font::Weight::Bold,
                    ..Default::default()
                }),
        )
        .push(widget::text::body(value).width(Length::Fill))
        .into()
}

fn message_header<'a>(msg: &'a MessageSummary) -> Element<'a, Message> {
    let mut col = widget::column().spacing(4);
    col = col.push(header_row("From:", &msg.from));
    if !msg.to.is_empty() {
        col = col.push(header_row("To:", &msg.to));
    }
    col = col.push(header_row("Subject:", &msg.subject));
    col = col.push(header_row("Date:", &msg.date));
    if let Some(ref reply_to) = msg.reply_to {
        col = col.push(header_row("Reply-To:", reply_to));
    }
    col.into()
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
