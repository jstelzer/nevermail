use cosmic::app::Task;
use futures::SinkExt;
use neverlight_mail_core::client::JmapClient;
use neverlight_mail_core::push::{self, EventSourceConfig};

use super::{AppModel, ConnectionState, Message};

/// Returns a stream that listens for JMAP EventSource (SSE) push notifications
/// and maps state changes into app messages.
pub(super) fn push_watch_stream(
    client: JmapClient,
    account_id: String,
) -> impl futures::Stream<Item = Message> {
    cosmic::iced_futures::stream::channel(50, move |mut output| async move {
        let aid = account_id.clone();
        let mut sender = output.clone();
        let result = push::listen(
            &client,
            &EventSourceConfig::default(),
            move |_state_change| {
                // Fire-and-forget send; the channel buffers up to 50
                let _ = sender.try_send(Message::PushStateChanged(aid.clone()));
            },
        )
        .await;

        match result {
            Ok(()) => {
                let _ = output.send(Message::PushEnded(account_id.clone())).await;
            }
            Err(e) => {
                let _ = output
                    .send(Message::PushError(account_id.clone(), e))
                    .await;
            }
        }
    })
}

impl AppModel {
    pub(super) fn handle_watch(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::PushStateChanged(ref account_id) => {
                log::debug!("Push state change for account {}", account_id);
                return self.dispatch(Message::Refresh);
            }

            Message::PushError(ref account_id, ref error) => {
                log::warn!("Push error for account {}: {}", account_id, error);
                if let Some(idx) = self.account_index(account_id) {
                    self.accounts[idx].last_error = Some(error.clone());
                    let aid = account_id.clone();
                    return cosmic::task::future(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        Message::ForceReconnect(aid)
                    });
                }
            }

            Message::PushEnded(ref account_id) => {
                log::info!("Push stream ended for account {}", account_id);
                if let Some(idx) = self.account_index(account_id) {
                    self.accounts[idx].conn_state =
                        ConnectionState::Error("Push stream ended".into());
                    self.accounts[idx].last_error = Some("Push stream ended".into());
                    let aid = account_id.clone();
                    return cosmic::task::future(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        Message::ForceReconnect(aid)
                    });
                }
            }

            _ => {}
        }
        Task::none()
    }
}
