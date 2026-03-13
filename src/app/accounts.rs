use cosmic::app::Task;

use neverlight_mail_core::client::JmapClient;
use neverlight_mail_core::config::ConfigNeedsInput;
use neverlight_mail_core::models::Folder;
use neverlight_mail_core::setup::SetupModel;

use super::{AppModel, Message, Phase};

fn revalidated_selected_folder_index(
    selected_mailbox_id: Option<&str>,
    selected_folder_index: Option<usize>,
    folders: &[Folder],
) -> (Option<usize>, Option<String>, bool) {
    let canonical_id = selected_mailbox_id
        .map(|s| s.to_string())
        .or_else(|| selected_folder_index.and_then(|idx| folders.get(idx).map(|f| f.mailbox_id.clone())));
    let Some(ref id) = canonical_id else {
        return (selected_folder_index, None, false);
    };
    if let Some(folder_idx) = folders.iter().position(|f| f.mailbox_id == *id) {
        return (Some(folder_idx), Some(id.clone()), false);
    }
    (None, None, true)
}

impl AppModel {
    fn clear_active_selection(&mut self) {
        self.active_account = None;
        self.selected_folder = None;
        self.selected_mailbox_id = None;
        self.selected_folder_evicted = false;
        self.clear_selected_folder_projection();
    }

    fn select_default_folder_for_account(&mut self, account_idx: usize) -> Task<Message> {
        let Some(acct) = self.accounts.get(account_idx) else {
            return Task::none();
        };
        let folder_idx = acct
            .folders
            .iter()
            .position(|f| f.path == "INBOX")
            .or_else(|| (!acct.folders.is_empty()).then_some(0));

        self.active_account = Some(account_idx);
        if let Some(folder_idx) = folder_idx {
            self.selected_folder = Some(folder_idx);
            self.selected_mailbox_id = Some(acct.folders[folder_idx].mailbox_id.clone());
            self.selected_folder_evicted = false;
            return self.dispatch(Message::SelectFolder(account_idx, folder_idx));
        }

        self.selected_folder = None;
        self.selected_mailbox_id = None;
        self.selected_folder_evicted = false;
        self.clear_selected_folder_projection();
        Task::none()
    }

    fn delete_account_now(&mut self, id: &str) -> Task<Message> {
        let Some(idx) = self.account_index(id) else {
            return Task::none();
        };

        let removed = self.accounts.remove(idx);
        if let Err(e) = self.save_multi_account_config() {
            self.accounts.insert(idx, removed);
            self.set_status_error(format!("Failed to remove account from config: {e}"));
            return Task::none();
        }

        let removed_id = removed.config.id.clone();
        let removed_username = removed.config.username.clone();
        let removed_jmap_url = removed.config.jmap_url.clone();

        // Keep compose account index valid.
        if self.accounts.is_empty() {
            self.compose_account = 0;
        } else if self.compose_account >= self.accounts.len() {
            self.compose_account = self.accounts.len() - 1;
        }
        self.refresh_compose_cache();

        // Adjust active account selection.
        let mut follow_up = Task::none();
        if self.accounts.is_empty() {
            self.clear_active_selection();
        } else if let Some(active) = self.active_account {
            if active == idx {
                let next_idx = idx.min(self.accounts.len() - 1);
                follow_up = self.select_default_folder_for_account(next_idx);
            } else if active > idx {
                self.active_account = Some(active - 1);
                self.revalidate_selected_folder();
            }
        }

        // Clean up keyring token
        if let Err(e) = neverlight_mail_core::keyring::delete_password(&removed_username, &removed_jmap_url) {
            log::warn!("Failed to delete token from keyring: {}", e);
        }

        self.status_message = "Account removed".into();

        // Clean up cached data for removed account
        let mut tasks = vec![follow_up];
        if let Some(cache) = &self.cache {
            let cache = cache.clone();
            tasks.push(cosmic::task::future(async move {
                if let Err(e) = cache.remove_account(removed_id).await {
                    log::warn!("Failed to clean cache for removed account: {}", e);
                }
                Message::Noop
            }));
        }
        cosmic::task::batch(tasks)
    }

    pub(super) fn mailbox_belongs_to_account(&self, account_id: &str, mailbox_id: &str) -> bool {
        self.account_index(account_id)
            .and_then(|idx| self.accounts.get(idx))
            .is_some_and(|a| a.folders.iter().any(|f| f.mailbox_id == mailbox_id))
    }

    fn clear_selected_folder_projection(&mut self) {
        self.messages.clear();
        self.selected_message = None;
        self.messages_offset = 0;
        self.has_more_messages = false;
        self.pending_body = None;
        self.preview_body.clear();
        self.preview_markdown.clear();
        self.preview_attachments.clear();
        self.preview_image_handles.clear();
        self.conversation.clear();
        self.active_conversation_id = None;
        self.collapsed_threads.clear();
        self.recompute_visible();
    }

    /// Keep selected folder anchored to canonical mailbox ID after any folder snapshot apply.
    pub(super) fn revalidate_selected_folder(&mut self) {
        let Some(active_idx) = self.active_account else {
            self.selected_folder = None;
            self.selected_mailbox_id = None;
            self.selected_folder_evicted = false;
            return;
        };
        let Some(active) = self.accounts.get(active_idx) else {
            self.selected_folder = None;
            self.selected_mailbox_id = None;
            self.selected_folder_evicted = true;
            self.clear_selected_folder_projection();
            self.status_message = "Selected folder evicted (account missing)".into();
            self.phase = Phase::Idle;
            return;
        };

        let canonical_id = self.selected_mailbox_id.clone().or_else(|| {
            self.selected_folder
                .and_then(|fi| active.folders.get(fi))
                .map(|f| f.mailbox_id.clone())
        });

        let Some(ref id) = canonical_id else {
            self.selected_folder_evicted = false;
            return;
        };
        let (folder_idx, mailbox_id, evicted) = revalidated_selected_folder_index(
            Some(id),
            self.selected_folder,
            &active.folders,
        );
        self.selected_folder = folder_idx;
        self.selected_mailbox_id = mailbox_id;
        self.selected_folder_evicted = evicted;
        if evicted {
            self.clear_selected_folder_projection();
            self.status_message = "Selected folder no longer exists; selection cleared".into();
            self.phase = Phase::Idle;
        }
    }

    /// Get the client for an explicit account ID.
    pub(super) fn client_for_account(&self, account_id: &str) -> Option<JmapClient> {
        self.account_index(account_id)
            .and_then(|i| self.accounts[i].client.clone())
    }

    /// Get the active account's ID, or empty string.
    pub(super) fn active_account_id(&self) -> String {
        self.active_account
            .and_then(|i| self.accounts.get(i))
            .map(|a| a.config.id.clone())
            .unwrap_or_default()
    }

    /// Get the active account's client.
    pub(super) fn active_client(&self) -> Option<JmapClient> {
        self.active_account
            .and_then(|i| self.accounts.get(i))
            .and_then(|a| a.client.clone())
    }

    /// Find account index by ID.
    pub(super) fn account_index(&self, account_id: &str) -> Option<usize> {
        self.accounts.iter().position(|a| a.config.id == account_id)
    }

    /// Drop a dead client and schedule reconnect with backoff.
    pub(super) fn drop_session_and_schedule_reconnect(
        &mut self,
        account_idx: usize,
        reason: &str,
    ) -> Task<Message> {
        let Some(acct) = self.accounts.get_mut(account_idx) else {
            return Task::none();
        };
        let label = acct.config.label.clone();
        log::warn!("Dropping client for '{}' (reason: {})", label, reason);
        acct.client = None;
        acct.conn_state = super::ConnectionState::Error(format!("Session lost: {}", reason));
        acct.last_error = Some(format!("Session lost: {}", reason));
        let delay = acct.reconnect_backoff();
        let aid = acct.config.id.clone();
        log::info!(
            "Scheduling reconnect for '{}' in {}s (reason: {})",
            label,
            delay.as_secs(),
            reason,
        );
        cosmic::task::future(async move {
            tokio::time::sleep(delay).await;
            Message::ForceReconnect(aid)
        })
    }

    /// Reconcile a folder's unread count from the actual messages in the list.
    /// Corrects sidebar badge drift after flag ops or server-side changes.
    pub(super) fn reconcile_folder_unread_count(&mut self, account_id: &str, mailbox_id: &str) {
        let unread = self
            .messages
            .iter()
            .filter(|m| m.context_mailbox_id == mailbox_id && !m.is_read)
            .count() as u32;
        if let Some(idx) = self.account_index(account_id) {
            if let Some(folder) = self.accounts[idx]
                .folders
                .iter_mut()
                .find(|f| f.mailbox_id == mailbox_id)
            {
                if folder.unread_count != unread {
                    log::debug!(
                        "Reconciling unread count for '{}': {} → {}",
                        folder.name,
                        folder.unread_count,
                        unread,
                    );
                    folder.unread_count = unread;
                }
            }
        }
    }

    /// Refresh the cached compose labels (account labels + from addresses)
    /// so dialog() can borrow them with &self lifetime.
    pub(super) fn refresh_compose_cache(&mut self) {
        self.compose_account_labels = self.accounts.iter().map(|a| a.config.label.clone()).collect();
        self.compose_cached_from = self
            .accounts
            .get(self.compose_account)
            .map(|a| a.config.email_addresses.clone())
            .unwrap_or_default();
    }

    /// Handle account management messages (add/edit/remove/collapse).
    pub(super) fn handle_account_management(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::AccountAdd => {
                self.setup_model = Some(SetupModel::from_config_needs(&ConfigNeedsInput::FullSetup));
                self.setup_password_visible = false;
            }
            Message::RequestDeleteAccount(ref id) => {
                self.confirm_delete_account_id = Some(id.clone());
            }
            Message::CancelDeleteAccount => {
                self.confirm_delete_account_id = None;
            }
            Message::ConfirmDeleteAccount => {
                let Some(id) = self.confirm_delete_account_id.take() else {
                    return Task::none();
                };
                self.setup_model = None;
                return self.delete_account_now(&id);
            }
            Message::AccountEdit(ref id) => {
                if let Some(acct) = self.accounts.iter().find(|a| &a.config.id == id) {
                    use neverlight_mail_core::setup::SetupFields;
                    self.setup_model = Some(SetupModel::for_edit(
                        id.clone(),
                        SetupFields {
                            label: acct.config.label.clone(),
                            jmap_url: acct.config.jmap_url.clone(),
                            username: acct.config.username.clone(),
                            email: acct.config.email_addresses.join(", "),
                        },
                    ));
                    self.setup_password_visible = false;
                }
            }
            Message::ToggleAccountCollapse(idx) => {
                if let Some(acct) = self.accounts.get_mut(idx) {
                    acct.collapsed = !acct.collapsed;
                }
            }
            _ => {}
        }
        Task::none()
    }

    /// Save the current account list to the multi-account config file.
    pub(super) fn save_multi_account_config(&self) -> Result<(), String> {
        use neverlight_mail_core::config::{AuthBackend, FileAccountConfig, MultiAccountFileConfig};

        let accounts: Vec<FileAccountConfig> = self
            .accounts
            .iter()
            .map(|a| FileAccountConfig {
                id: a.config.id.clone(),
                label: a.config.label.clone(),
                jmap_url: a.config.jmap_url.clone(),
                username: a.config.username.clone(),
                auth: AuthBackend::Keyring,
                email_addresses: a.config.email_addresses.clone(),
                capabilities: a.config.capabilities.clone(),
                max_messages_per_mailbox: a.config.max_messages_per_mailbox,
            })
            .collect();

        let config = MultiAccountFileConfig { accounts };
        config.save()
    }
}

#[cfg(test)]
mod tests {
    use super::revalidated_selected_folder_index;
    use neverlight_mail_core::models::Folder;

    fn folder(mailbox_id: &str, name: &str) -> Folder {
        Folder {
            mailbox_id: mailbox_id.to_string(),
            path: name.to_string(),
            name: name.to_string(),
            role: None,
            sort_order: 0,
            unread_count: 0,
            total_count: 0,
        }
    }

    #[test]
    fn revalidation_keeps_selection_when_mailbox_still_exists() {
        let folders = vec![folder("M11", "INBOX"), folder("M22", "Archive")];
        let (idx, id, evicted) = revalidated_selected_folder_index(Some("M22"), Some(0), &folders);
        assert_eq!(idx, Some(1));
        assert_eq!(id.as_deref(), Some("M22"));
        assert!(!evicted);
    }

    #[test]
    fn revalidation_evicts_selection_when_mailbox_missing() {
        let folders = vec![folder("M11", "INBOX"), folder("M33", "Sent")];
        let (idx, id, evicted) = revalidated_selected_folder_index(Some("M22"), Some(0), &folders);
        assert_eq!(idx, None);
        assert_eq!(id, None);
        assert!(evicted);
    }

    #[test]
    fn revalidation_derives_id_from_index_when_id_not_set() {
        let folders = vec![folder("M11", "INBOX"), folder("M22", "Archive")];
        let (idx, id, evicted) = revalidated_selected_folder_index(None, Some(1), &folders);
        assert_eq!(idx, Some(1));
        assert_eq!(id.as_deref(), Some("M22"));
        assert!(!evicted);
    }
}
