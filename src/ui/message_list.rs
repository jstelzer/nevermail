use std::collections::{HashMap, HashSet};

use cosmic::iced::Length;
use cosmic::widget;
use cosmic::Element;

use crate::app::Message;
use neverlight_mail_core::models::MessageSummary;

use crate::dnd_models::DraggedMessage;

pub struct MessageListState<'a> {
    pub messages: &'a [MessageSummary],
    pub visible_indices: &'a [usize],
    pub selected: Option<usize>,
    pub has_more: bool,
    pub collapsed_threads: &'a HashSet<String>,
    pub thread_sizes: &'a HashMap<String, usize>,
    pub search_active: bool,
    pub search_query: &'a str,
}

pub fn search_input_id() -> widget::Id {
    widget::Id::new("search-input")
}

/// Render the message list for the selected folder.
pub fn view<'a>(state: MessageListState<'a>) -> Element<'a, Message> {
    let MessageListState {
        messages,
        visible_indices,
        selected,
        has_more,
        collapsed_threads,
        thread_sizes,
        search_active,
        search_query,
    } = state;
    let mut col = widget::column().spacing(2).padding(8);

    if search_active {
        let input = widget::text_input("Search all mail...", search_query)
            .on_input(Message::SearchQueryChanged)
            .on_submit(|_| Message::SearchExecute)
            .id(search_input_id());
        let clear_btn = widget::button::text("Clear").on_press(Message::SearchClear);
        col = col.push(
            widget::row()
                .push(widget::container(input).width(Length::Fill))
                .push(clear_btn)
                .spacing(4)
                .align_y(cosmic::iced::Alignment::Center),
        );
    }

    if messages.is_empty() {
        col = col.push(widget::text::body("No messages"));
    } else {
        for &real_index in visible_indices {
            let msg = &messages[real_index];
            let is_selected = selected == Some(real_index);

            let star = if msg.is_starred { "★ " } else { "" };
            let unread = if !msg.is_read { "● " } else { "" };

            // Thread collapse/expand indicator for root messages with children
            let thread_indicator = if msg.thread_depth == 0 {
                if let Some(ref tid) = msg.thread_id {
                    let size = thread_sizes.get(tid).copied().unwrap_or(1);
                    if size > 1 {
                        if collapsed_threads.contains(tid) {
                            format!("▶ ({}) ", size - 1)
                        } else {
                            "▼ ".to_string()
                        }
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            let subject_text = format!("{}{}{}{}", unread, star, thread_indicator, msg.subject);
            let subject = widget::text::body(subject_text);
            let meta = widget::text::caption(format!("{} — {}", msg.from, msg.date));

            let depth = msg.thread_depth.min(4);
            let indent = (depth as u16) * 16;
            let row_content = widget::column().push(subject).push(meta).spacing(2);
            let padded = widget::container(row_content).padding([0, 0, 0, indent]);

            let mut btn = widget::button::custom(padded)
                .on_press(Message::ViewBody(real_index))
                .width(Length::Fill);

            if is_selected {
                btn = btn.class(cosmic::theme::Button::Suggested);
            }

            let email_id = msg.email_id.clone();
            let mailbox_id = msg.mailbox_id.clone();
            let source_account_id = msg.account_id.clone();
            let source = widget::dnd_source::<Message, DraggedMessage>(btn)
                .drag_content(move || DraggedMessage {
                    source_account_id: source_account_id.clone(),
                    email_id: email_id.clone(),
                    source_mailbox_id: mailbox_id.clone(),
                })
                .drag_threshold(8.0);

            col = col.push(source);
        }

        if has_more {
            let load_more_btn = widget::button::text("Load more messages")
                .on_press(Message::LoadMoreMessages)
                .width(Length::Fill);
            col = col.push(widget::vertical_space().height(4));
            col = col.push(load_more_btn);
        }
    }

    widget::scrollable(col).height(Length::Fill).into()
}
