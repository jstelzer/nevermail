use cosmic::iced::Length;
use cosmic::widget;
use cosmic::widget::text_editor;
use cosmic::Element;

use crate::app::Message;

#[derive(Debug, Clone, PartialEq)]
pub enum ComposeMode {
    New,
    Reply,
    Forward,
}

pub fn view<'a>(
    mode: &ComposeMode,
    from_addresses: &'a [String],
    from_selected: usize,
    to: &'a str,
    subject: &'a str,
    body: &'a text_editor::Content,
    error: Option<&'a str>,
    is_sending: bool,
) -> Element<'a, Message> {
    let title = match mode {
        ComposeMode::New => "New Message",
        ComposeMode::Reply => "Reply",
        ComposeMode::Forward => "Forward",
    };

    let mut controls = widget::column().spacing(12);

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
