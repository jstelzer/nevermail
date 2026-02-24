use cosmic::iced::Length;
use cosmic::widget;
use cosmic::Element;

use crate::app::Message;
use crate::core::models::Folder;

/// Render the folder sidebar.
pub fn view<'a>(folders: &[Folder], selected: Option<usize>) -> Element<'a, Message> {
    let mut col = widget::column().spacing(4).padding(8);

    col = col.push(
        widget::button::suggested("Compose")
            .on_press(Message::ComposeNew)
            .width(Length::Fill),
    );
    col = col.push(widget::vertical_space().height(8));

    if folders.is_empty() {
        col = col.push(widget::text::body("No folders"));
    } else {
        for (i, folder) in folders.iter().enumerate() {
            let _is_selected = selected == Some(i);
            let label = if folder.unread_count > 0 {
                format!("{} ({})", folder.name, folder.unread_count)
            } else {
                folder.name.clone()
            };

            let btn = widget::button::text(label)
                .on_press(Message::SelectFolder(i))
                .width(Length::Fill);

            // TODO: Style differently when selected
            col = col.push(btn);
        }
    }

    widget::scrollable(col).height(Length::Fill).into()
}
