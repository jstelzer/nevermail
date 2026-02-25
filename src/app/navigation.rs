use cosmic::app::Task;

use super::{AppModel, Message};

impl AppModel {
    pub(super) fn handle_navigation(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SelectionDown => {
                if self.messages.is_empty() {
                    return Task::none();
                }
                let current_vis_pos = self
                    .selected_message
                    .and_then(|sel| self.visible_indices.iter().position(|&ri| ri == sel));
                let new_vis_pos = match current_vis_pos {
                    Some(pos) => (pos + 1).min(self.visible_indices.len().saturating_sub(1)),
                    None => 0,
                };
                if let Some(&real_index) = self.visible_indices.get(new_vis_pos) {
                    self.selected_message = Some(real_index);
                    return self.dispatch(Message::ViewBody(real_index));
                }
            }

            Message::SelectionUp => {
                if self.messages.is_empty() {
                    return Task::none();
                }
                let current_vis_pos = self
                    .selected_message
                    .and_then(|sel| self.visible_indices.iter().position(|&ri| ri == sel));
                let new_vis_pos = match current_vis_pos {
                    Some(pos) => pos.saturating_sub(1),
                    None => 0,
                };
                if let Some(&real_index) = self.visible_indices.get(new_vis_pos) {
                    self.selected_message = Some(real_index);
                    return self.dispatch(Message::ViewBody(real_index));
                }
            }

            Message::ActivateSelection => {
                if let Some(index) = self.selected_message {
                    return self.dispatch(Message::ViewBody(index));
                }
            }

            Message::ToggleThreadCollapse => {
                if let Some(index) = self.selected_message {
                    if let Some(msg) = self.messages.get(index) {
                        if let Some(tid) = msg.thread_id {
                            let size = self.thread_sizes.get(&tid).copied().unwrap_or(1);
                            if size > 1 {
                                if self.collapsed_threads.contains(&tid) {
                                    // Expand
                                    self.collapsed_threads.remove(&tid);
                                } else {
                                    // Collapse — if selected message is a child, jump to root
                                    self.collapsed_threads.insert(tid);
                                    if msg.thread_depth > 0 {
                                        // Find the thread root (first message with this thread_id and depth 0)
                                        if let Some(root_idx) = self.messages.iter().position(|m| {
                                            m.thread_id == Some(tid) && m.thread_depth == 0
                                        }) {
                                            self.selected_message = Some(root_idx);
                                        }
                                    }
                                }
                                self.recompute_visible();
                            }
                        }
                    }
                }
            }

            _ => {}
        }
        Task::none()
    }

    /// Rebuild `visible_indices` and `thread_sizes` based on current messages
    /// and collapsed state.
    pub(super) fn recompute_visible(&mut self) {
        // Rebuild thread_sizes
        self.thread_sizes.clear();
        for msg in &self.messages {
            if let Some(tid) = msg.thread_id {
                *self.thread_sizes.entry(tid).or_insert(0) += 1;
            }
        }

        // Rebuild visible_indices: hide children of collapsed threads
        self.visible_indices.clear();
        for (i, msg) in self.messages.iter().enumerate() {
            if msg.thread_depth > 0 {
                if let Some(tid) = msg.thread_id {
                    if self.collapsed_threads.contains(&tid) {
                        continue; // hidden child
                    }
                }
            }
            self.visible_indices.push(i);
        }
    }
}
