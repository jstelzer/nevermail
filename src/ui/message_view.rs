use cosmic::iced::Length;
use cosmic::widget;
use cosmic::Element;

use crate::app::Message;
use crate::core::models::MessageSummary;

/// Render the message preview pane with an action toolbar when a message is selected.
pub fn view<'a>(body: &'a str, selected: Option<(usize, &MessageSummary)>) -> Element<'a, Message> {
    if body.is_empty() {
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

    let body_content = widget::scrollable(
        widget::container(widget::text::body(body))
            .padding(16)
            .width(Length::Fill),
    )
    .height(Length::Fill);

    col = col.push(body_content);

    col.height(Length::Fill).into()
}
