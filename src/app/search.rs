use cosmic::app::Task;
use cosmic::widget;
use futures::future::{AbortHandle, Abortable};

use super::{AppModel, Message, Phase};

fn should_apply_search_results(
    current_epoch: u64,
    incoming_epoch: u64,
    current_query: &str,
    incoming_query: &str,
) -> bool {
    current_epoch == incoming_epoch && current_query.trim() == incoming_query
}

impl AppModel {
    pub(super) fn handle_search(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SearchActivate => {
                if self.setup_model.is_some() || self.show_compose_dialog || self.search_focused {
                    return Task::none();
                }
                self.search_active = true;
                self.search_focused = true;
                self.search_query.clear();
                return widget::text_input::focus(crate::ui::message_list::search_input_id());
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
                    self.search_epoch = self.search_epoch.saturating_add(1);
                    let epoch = self.search_epoch;
                    self.status_message = "Searching...".into();
                    self.phase = Phase::Searching;
                    if let Some(handle) = self.search_abort.take() {
                        handle.abort();
                    }
                    let (abort_handle, abort_reg) = AbortHandle::new_pair();
                    self.search_abort = Some(abort_handle);
                    return cosmic::task::future(async move {
                        match Abortable::new(cache.search(query.clone()), abort_reg).await {
                            Ok(result) => Message::SearchResultsLoaded {
                                query,
                                epoch,
                                result,
                            },
                            Err(_) => Message::Noop,
                        }
                    });
                }
            }
            Message::SearchResultsLoaded {
                query,
                epoch,
                result: Ok(results),
            } => {
                if !should_apply_search_results(
                    self.search_epoch,
                    epoch,
                    self.search_query.as_str(),
                    query.as_str(),
                ) {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                self.search_abort = None;
                let count = results.len();
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
                self.clear_error_surface();
                self.phase = Phase::Idle;
            }
            Message::SearchResultsLoaded {
                epoch,
                result: Err(e),
                ..
            } => {
                if epoch != self.search_epoch {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                self.search_abort = None;
                self.search_focused = false;
                log::error!("Search failed: {}", e);
                self.set_status_error(format!("Search failed: {}", e));
            }
            Message::SearchClear => {
                if self.search_active {
                    if let Some(handle) = self.search_abort.take() {
                        handle.abort();
                    }
                    self.search_active = false;
                    self.search_focused = false;
                    self.search_query.clear();
                    // Restore previous folder view
                    if let Some(acct_idx) = self.active_account {
                        if let Some(folder_idx) = self.selected_folder {
                            self.phase = Phase::Loading;
                            return self.dispatch(Message::SelectFolder(acct_idx, folder_idx));
                        }
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

#[cfg(test)]
mod tests {
    use super::should_apply_search_results;

    #[test]
    fn search_results_apply_when_epoch_and_query_match() {
        assert!(should_apply_search_results(8, 8, "inbox", "inbox"));
        assert!(should_apply_search_results(8, 8, "  inbox  ", "inbox"));
    }

    #[test]
    fn search_results_drop_on_epoch_mismatch() {
        assert!(!should_apply_search_results(9, 8, "inbox", "inbox"));
    }

    #[test]
    fn search_results_drop_on_query_mismatch() {
        assert!(!should_apply_search_results(8, 8, "inbox", "sent"));
    }
}
