//! Background backfill subscription: walks mailbox history in batches.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cosmic::app::Task;
use futures::SinkExt;
use neverlight_mail_core::backfill;
use neverlight_mail_core::client::JmapClient;
use neverlight_mail_core::config::AccountId;
use neverlight_mail_core::store::CacheHandle;

use super::{AppModel, Message};

/// Returns a stream that round-robins over incomplete mailboxes,
/// fetching one batch per mailbox, sleeping between batches.
pub(super) fn backfill_stream(
    client: JmapClient,
    cache: CacheHandle,
    account_id: AccountId,
    folder_mailbox_ids: Vec<String>,
    max_messages: Option<u32>,
    pause: Arc<AtomicBool>,
) -> impl futures::Stream<Item = Message> {
    cosmic::iced_futures::stream::channel(10, move |mut output| async move {
        let page_size = neverlight_mail_core::email::DEFAULT_PAGE_SIZE;
        let aid = account_id.clone();

        loop {
            // Wait while paused (head sync in progress)
            while pause.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }

            // Get incomplete mailboxes from cache
            let incomplete = match cache.list_backfill_progress(aid.clone()).await {
                Ok(list) => list,
                Err(e) => {
                    log::warn!("backfill: failed to list progress: {}", e);
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    continue;
                }
            };

            // Build work list: incomplete mailboxes + mailboxes with no progress yet
            let has_progress: std::collections::HashSet<String> =
                incomplete.iter().map(|p| p.mailbox_id.clone()).collect();

            let mut work: Vec<String> = incomplete.into_iter().map(|p| p.mailbox_id).collect();

            // Add mailboxes that have never started backfill
            for mid in &folder_mailbox_ids {
                if !has_progress.contains(mid) {
                    // Check if already completed
                    let progress = cache
                        .get_backfill_progress(aid.clone(), mid.clone())
                        .await
                        .ok()
                        .flatten();
                    if progress.is_none() {
                        work.push(mid.clone());
                    }
                }
            }

            if work.is_empty() {
                // All mailboxes completed
                let _ = output
                    .send(Message::BackfillComplete(aid.clone()))
                    .await;
                return;
            }

            // Process one batch per mailbox
            for mailbox_id in &work {
                // Check pause between each mailbox
                if pause.load(Ordering::Relaxed) {
                    break;
                }

                match backfill::backfill_batch(
                    &client,
                    &cache,
                    &aid,
                    mailbox_id,
                    page_size,
                    max_messages,
                )
                .await
                {
                    Ok(result) => {
                        let _ = output
                            .send(Message::BackfillProgress {
                                account_id: aid.clone(),
                                mailbox_id: result.mailbox_id,
                                position: result.position,
                                total: result.total,
                                completed: result.completed,
                            })
                            .await;
                    }
                    Err(e) => {
                        log::warn!(
                            "backfill: batch failed for mailbox {}: {}",
                            mailbox_id,
                            e
                        );
                    }
                }

                // Throttle between mailboxes
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    })
}

impl AppModel {
    pub(super) fn handle_backfill(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::BackfillProgress {
                ref account_id,
                ref mailbox_id,
                position,
                total,
                completed,
            } => {
                if let Some(idx) = self.account_index(account_id) {
                    if completed {
                        self.accounts[idx].backfill_progress.remove(mailbox_id);
                    } else {
                        self.accounts[idx]
                            .backfill_progress
                            .insert(mailbox_id.clone(), (position, total));
                    }
                }
            }
            Message::BackfillComplete(ref account_id) => {
                log::info!("Backfill complete for account {}", account_id);
                if let Some(idx) = self.account_index(account_id) {
                    self.accounts[idx].backfill_active = false;
                    self.accounts[idx].backfill_progress.clear();
                }
            }
            Message::BackfillTrigger {
                ref account_id,
                ref mailbox_id,
            } => {
                if let Some(cache) = &self.cache {
                    let cache = cache.clone();
                    let aid = account_id.clone();
                    let mid = mailbox_id.clone();
                    if let Some(idx) = self.account_index(account_id) {
                        self.accounts[idx].backfill_active = true;
                    }
                    return cosmic::task::future(async move {
                        let _ = cache.reset_backfill_progress(aid, mid).await;
                        Message::Noop
                    });
                }
            }
            _ => {}
        }
        Task::none()
    }
}
