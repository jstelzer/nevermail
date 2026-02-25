use cosmic::app::Task;
use cosmic::widget;

use super::{AppModel, Message};

impl AppModel {
    pub(super) fn handle_search(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SearchActivate => {
                if self.show_setup_dialog || self.show_compose_dialog || self.search_focused {
                    return Task::none();
                }
                self.search_active = true;
                self.search_focused = true;
                self.search_query.clear();
                return widget::text_input::focus(
                    crate::ui::message_list::search_input_id(),
                );
            }
            Message::SearchQueryChanged(q) => {
                self.search_query = q;
            }
            Message::SearchExecute => {
                let query = self.search_query.trim().to_string();
                if query.is_empty() {
                    return Task::none();
                }
                if let Some(cache) = &self.cache {
                    let cache = cache.clone();
                    self.status_message = "Searching...".into();
                    return cosmic::task::future(async move {
                        Message::SearchResultsLoaded(cache.search(query).await)
                    });
                }
            }
            Message::SearchResultsLoaded(Ok(results)) => {
                let count = results.len();
                let query = self.search_query.clone();
                self.messages = results;
                self.selected_message = None;
                self.preview_body.clear();
                self.preview_markdown.clear();
                self.preview_attachments.clear();
                self.preview_image_handles.clear();
                self.collapsed_threads.clear();
                self.has_more_messages = false;
                self.recompute_visible();
                self.search_focused = false;
                if count > 0 {
                    self.status_message = format!("Search: {} results for \"{}\"", count, query);
                } else {
                    self.status_message = format!("Search: no results for \"{}\"", query);
                }
            }
            Message::SearchResultsLoaded(Err(e)) => {
                self.search_focused = false;
                self.status_message = format!("Search failed: {}", e);
                log::error!("Search failed: {}", e);
            }
            Message::SearchClear => {
                if self.search_active {
                    self.search_active = false;
                    self.search_focused = false;
                    self.search_query.clear();
                    // Restore previous folder view
                    if let Some(idx) = self.selected_folder {
                        return self.dispatch(Message::SelectFolder(idx));
                    }
                } else {
                    // Not searching — Escape cancels compose dialog
                    self.show_compose_dialog = false;
                    self.is_sending = false;
                }
            }

            _ => {}
        }
        Task::none()
    }
}
