use std::collections::HashMap;
use std::sync::Arc;

use cosmic::app::Task;

use neverlight_mail_core::config::ConfigNeedsInput;
use neverlight_mail_core::imap::ImapSession;
use neverlight_mail_core::models::Folder;
use neverlight_mail_core::setup::SetupModel;

use super::{AppModel, Message, Phase};

fn revalidated_selected_folder_index(
    selected_mailbox_hash: Option<u64>,
    selected_folder_index: Option<usize>,
    folders: &[Folder],
) -> (Option<usize>, Option<u64>, bool) {
    let canonical_hash = selected_mailbox_hash
        .or_else(|| selected_folder_index.and_then(|idx| folders.get(idx).map(|f| f.mailbox_hash)));
    let Some(hash) = canonical_hash else {
        return (selected_folder_index, None, false);
    };
    if let Some(folder_idx) = folders.iter().position(|f| f.mailbox_hash == hash) {
        return (Some(folder_idx), Some(hash), false);
    }
    (None, None, true)
}

impl AppModel {
    pub(super) fn mailbox_belongs_to_account(&self, account_id: &str, mailbox_hash: u64) -> bool {
        self.account_index(account_id)
            .and_then(|idx| self.accounts.get(idx))
            .is_some_and(|a| a.folders.iter().any(|f| f.mailbox_hash == mailbox_hash))
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
        self.collapsed_threads.clear();
        self.recompute_visible();
    }

    /// Keep selected folder anchored to canonical mailbox hash after any folder snapshot apply.
    pub(super) fn revalidate_selected_folder(&mut self) {
        let Some(active_idx) = self.active_account else {
            self.selected_folder = None;
            self.selected_mailbox_hash = None;
            self.selected_folder_evicted = false;
            return;
        };
        let Some(active) = self.accounts.get(active_idx) else {
            self.selected_folder = None;
            self.selected_mailbox_hash = None;
            self.selected_folder_evicted = true;
            self.clear_selected_folder_projection();
            self.status_message = "Selected folder evicted (account missing)".into();
            self.phase = Phase::Idle;
            return;
        };

        let canonical_hash = self.selected_mailbox_hash.or_else(|| {
            self.selected_folder
                .and_then(|fi| active.folders.get(fi))
                .map(|f| f.mailbox_hash)
        });

        let Some(hash) = canonical_hash else {
            self.selected_folder_evicted = false;
            return;
        };
        let (folder_idx, mailbox_hash, evicted) = revalidated_selected_folder_index(
            Some(hash),
            self.selected_folder,
            &active.folders,
        );
        self.selected_folder = folder_idx;
        self.selected_mailbox_hash = mailbox_hash;
        self.selected_folder_evicted = evicted;
        if evicted {
            self.clear_selected_folder_projection();
            self.status_message = "Selected folder no longer exists; selection cleared".into();
            self.phase = Phase::Idle;
        }
    }

    /// Find the account index that owns a given mailbox_hash.
    pub(super) fn account_for_mailbox(&self, mailbox_hash: u64) -> Option<usize> {
        let mut matches = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.folders.iter().any(|f| f.mailbox_hash == mailbox_hash))
            .map(|(i, _)| i);
        let first = matches.next();
        if first.is_some() && matches.next().is_some() {
            log::error!(
                "Ambiguous mailbox ownership for mailbox_hash {} across accounts",
                mailbox_hash
            );
            return None;
        }
        first
    }

    /// Find account index by explicit account hint and mailbox hash.
    pub(super) fn account_for_account_mailbox(
        &self,
        account_id: &str,
        mailbox_hash: u64,
    ) -> Option<usize> {
        let idx = self.account_index(account_id)?;
        self.accounts
            .get(idx)
            .filter(|a| a.folders.iter().any(|f| f.mailbox_hash == mailbox_hash))
            .map(|_| idx)
    }

    /// Get the session for an explicit account + mailbox ownership pair.
    pub(super) fn session_for_account_mailbox(
        &self,
        account_id: &str,
        mailbox_hash: u64,
    ) -> Option<Arc<ImapSession>> {
        self.account_for_account_mailbox(account_id, mailbox_hash)
            .and_then(|i| self.accounts[i].session.clone())
    }

    /// Get the folder_map for an explicit account + mailbox ownership pair.
    pub(super) fn folder_map_for_account_mailbox(
        &self,
        account_id: &str,
        mailbox_hash: u64,
    ) -> Option<&HashMap<String, u64>> {
        self.account_for_account_mailbox(account_id, mailbox_hash)
            .map(|i| &self.accounts[i].folder_map)
    }

    /// Get the active account's ID, or empty string.
    pub(super) fn active_account_id(&self) -> String {
        self.active_account
            .and_then(|i| self.accounts.get(i))
            .map(|a| a.config.id.clone())
            .unwrap_or_default()
    }

    /// Get the active account's session.
    pub(super) fn active_session(&self) -> Option<Arc<ImapSession>> {
        self.active_account
            .and_then(|i| self.accounts.get(i))
            .and_then(|a| a.session.clone())
    }

    /// Find account index by ID.
    pub(super) fn account_index(&self, account_id: &str) -> Option<usize> {
        self.accounts.iter().position(|a| a.config.id == account_id)
    }

    /// Drop a dead session and schedule reconnect with backoff.
    pub(super) fn drop_session_and_schedule_reconnect(
        &mut self,
        account_idx: usize,
        reason: &str,
    ) -> Task<Message> {
        let Some(acct) = self.accounts.get_mut(account_idx) else {
            return Task::none();
        };
        let label = acct.config.label.clone();
        log::warn!("Dropping session for '{}' (reason: {})", label, reason);
        acct.session = None;
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
    pub(super) fn reconcile_folder_unread_count(&mut self, account_id: &str, mailbox_hash: u64) {
        let unread = self
            .messages
            .iter()
            .filter(|m| m.mailbox_hash == mailbox_hash && !m.is_read)
            .count() as u32;
        if let Some(idx) = self.account_index(account_id) {
            if let Some(folder) = self.accounts[idx]
                .folders
                .iter_mut()
                .find(|f| f.mailbox_hash == mailbox_hash)
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
            Message::AccountEdit(ref id) => {
                if let Some(acct) = self.accounts.iter().find(|a| &a.config.id == id) {
                    use neverlight_mail_core::setup::SetupFields;
                    self.setup_model = Some(SetupModel::for_edit(
                        id.clone(),
                        SetupFields {
                            label: acct.config.label.clone(),
                            server: acct.config.imap_server.clone(),
                            port: acct.config.imap_port.to_string(),
                            username: acct.config.username.clone(),
                            email: acct.config.email_addresses.join(", "),
                            starttls: acct.config.use_starttls,
                            smtp_server: acct.config.smtp_overrides.server.clone().unwrap_or_default(),
                            smtp_port: acct.config.smtp_overrides.port.map(|p| p.to_string()).unwrap_or_else(|| "587".into()),
                            smtp_username: acct.config.smtp_overrides.username.clone().unwrap_or_default(),
                            smtp_starttls: acct.config.smtp_overrides.use_starttls.unwrap_or(true),
                        },
                    ));
                    self.setup_password_visible = false;
                }
            }
            Message::AccountRemove(ref id) => {
                if let Some(idx) = self.account_index(id) {
                    let removed_id = self.accounts[idx].config.id.clone();
                    let removed_username = self.accounts[idx].config.username.clone();
                    let removed_server = self.accounts[idx].config.imap_server.clone();
                    self.accounts.remove(idx);
                    // Adjust active_account
                    if let Some(active) = self.active_account {
                        if active == idx {
                            self.active_account = None;
                            self.selected_folder = None;
                            self.selected_mailbox_hash = None;
                            self.selected_folder_evicted = false;
                            self.clear_selected_folder_projection();
                        } else if active > idx {
                            self.active_account = Some(active - 1);
                            self.revalidate_selected_folder();
                        }
                    }
                    // Save updated config
                    let _ = self.save_multi_account_config();

                    // Clean up keyring passwords
                    if let Err(e) = neverlight_mail_core::keyring::delete_password(&removed_username, &removed_server) {
                        log::warn!("Failed to delete IMAP password from keyring: {}", e);
                    }
                    if let Err(e) = neverlight_mail_core::keyring::delete_smtp_password(&removed_id) {
                        log::debug!("No SMTP password to delete from keyring: {}", e);
                    }

                    self.status_message = "Account removed".into();

                    // Clean up cached data for removed account
                    if let Some(cache) = &self.cache {
                        let cache = cache.clone();
                        return cosmic::task::future(async move {
                            if let Err(e) = cache.remove_account(removed_id).await {
                                log::warn!("Failed to clean cache for removed account: {}", e);
                            }
                            Message::Noop
                        });
                    }
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
        use neverlight_mail_core::config::{FileAccountConfig, MultiAccountFileConfig, PasswordBackend};

        let accounts: Vec<FileAccountConfig> = self
            .accounts
            .iter()
            .map(|a| FileAccountConfig {
                id: a.config.id.clone(),
                label: a.config.label.clone(),
                server: a.config.imap_server.clone(),
                port: a.config.imap_port,
                username: a.config.username.clone(),
                starttls: a.config.use_starttls,
                password: PasswordBackend::Keyring,
                email_addresses: a.config.email_addresses.clone(),
                smtp: a.config.smtp_overrides.clone(),
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

    fn folder(mailbox_hash: u64, name: &str) -> Folder {
        Folder {
            mailbox_hash,
            path: name.to_string(),
            name: name.to_string(),
            unread_count: 0,
            total_count: 0,
        }
    }

    #[test]
    fn revalidation_keeps_selection_when_mailbox_still_exists() {
        let folders = vec![folder(11, "INBOX"), folder(22, "Archive")];
        let (idx, hash, evicted) = revalidated_selected_folder_index(Some(22), Some(0), &folders);
        assert_eq!(idx, Some(1));
        assert_eq!(hash, Some(22));
        assert!(!evicted);
    }

    #[test]
    fn revalidation_evicts_selection_when_mailbox_missing() {
        let folders = vec![folder(11, "INBOX"), folder(33, "Sent")];
        let (idx, hash, evicted) = revalidated_selected_folder_index(Some(22), Some(0), &folders);
        assert_eq!(idx, None);
        assert_eq!(hash, None);
        assert!(evicted);
    }

    #[test]
    fn revalidation_derives_hash_from_index_when_hash_not_set() {
        let folders = vec![folder(11, "INBOX"), folder(22, "Archive")];
        let (idx, hash, evicted) = revalidated_selected_folder_index(None, Some(1), &folders);
        assert_eq!(idx, Some(1));
        assert_eq!(hash, Some(22));
        assert!(!evicted);
    }
}
