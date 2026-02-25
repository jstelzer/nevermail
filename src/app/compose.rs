use cosmic::app::Task;
use cosmic::widget::text_editor;

use super::{AppModel, Message};
use crate::core::smtp::{self, OutgoingEmail};
use crate::config::SmtpConfig;
use crate::ui::compose_dialog::ComposeMode;

impl AppModel {
    pub(super) fn handle_compose(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ComposeNew => {
                if self.show_setup_dialog || self.show_compose_dialog {
                    return Task::none();
                }
                self.compose_mode = ComposeMode::New;
                self.compose_from = 0;
                self.compose_to.clear();
                self.compose_subject.clear();
                self.compose_body = text_editor::Content::new();
                self.compose_in_reply_to = None;
                self.compose_references = None;
                self.compose_error = None;
                self.is_sending = false;
                self.show_compose_dialog = true;
            }

            Message::ComposeReply => {
                if self.show_setup_dialog || self.show_compose_dialog {
                    return Task::none();
                }
                if let Some(index) = self.selected_message {
                    if let Some(msg) = self.messages.get(index) {
                        self.compose_mode = ComposeMode::Reply;
                        self.compose_to = msg.from.clone();

                        let subj = &msg.subject;
                        self.compose_subject = if subj.starts_with("Re: ") {
                            subj.clone()
                        } else {
                            format!("Re: {subj}")
                        };

                        let quoted = quote_body(&self.preview_body, &msg.from, &msg.date);
                        self.compose_body = text_editor::Content::with_text(&format!("\n\n{quoted}"));

                        self.compose_in_reply_to = Some(msg.message_id.clone());
                        self.compose_references = Some(build_references(
                            msg.in_reply_to.as_deref(),
                            &msg.message_id,
                        ));
                        self.compose_error = None;
                        self.is_sending = false;
                        self.show_compose_dialog = true;
                    }
                }
            }

            Message::ComposeForward => {
                if self.show_setup_dialog || self.show_compose_dialog {
                    return Task::none();
                }
                if let Some(index) = self.selected_message {
                    if let Some(msg) = self.messages.get(index) {
                        self.compose_mode = ComposeMode::Forward;
                        self.compose_to.clear();

                        let subj = &msg.subject;
                        self.compose_subject = if subj.starts_with("Fwd: ") {
                            subj.clone()
                        } else {
                            format!("Fwd: {subj}")
                        };

                        let fwd = forward_body(
                            &self.preview_body,
                            &msg.from,
                            &msg.date,
                            &msg.subject,
                        );
                        self.compose_body = text_editor::Content::with_text(&format!("\n\n{fwd}"));

                        self.compose_in_reply_to = None;
                        self.compose_references = None;
                        self.compose_error = None;
                        self.is_sending = false;
                        self.show_compose_dialog = true;
                    }
                }
            }

            Message::ComposeFromChanged(i) => {
                self.compose_from = i;
            }
            Message::ComposeToChanged(v) => {
                self.compose_to = v;
            }
            Message::ComposeSubjectChanged(v) => {
                self.compose_subject = v;
            }
            Message::ComposeBodyAction(action) => {
                self.compose_body.perform(action);
            }

            Message::ComposeSend => {
                if self.compose_to.trim().is_empty() {
                    self.compose_error = Some("Recipient is required".into());
                    return Task::none();
                }

                let body_text = self.compose_body.text();
                if body_text.trim().is_empty() {
                    self.compose_error = Some("Message body is required".into());
                    return Task::none();
                }

                let Some(ref config) = self.config else {
                    self.compose_error = Some("Not configured".into());
                    return Task::none();
                };

                let from_addr = config
                    .email_addresses
                    .get(self.compose_from)
                    .cloned()
                    .unwrap_or_else(|| {
                        config.email_addresses.first().cloned().unwrap_or_default()
                    });
                if from_addr.is_empty() {
                    self.compose_error = Some(
                        "No email address configured. Re-run setup to add one.".into(),
                    );
                    return Task::none();
                }

                self.is_sending = true;
                self.compose_error = None;

                let smtp_config = SmtpConfig::from_imap_config(config);
                let email = OutgoingEmail {
                    from: from_addr,
                    to: self.compose_to.clone(),
                    subject: self.compose_subject.clone(),
                    body: body_text,
                    in_reply_to: self.compose_in_reply_to.clone(),
                    references: self.compose_references.clone(),
                };

                return cosmic::task::future(async move {
                    Message::SendComplete(smtp::send_email(&smtp_config, &email).await)
                });
            }

            Message::ComposeCancel => {
                self.show_compose_dialog = false;
                self.is_sending = false;
            }

            Message::SendComplete(Ok(())) => {
                self.show_compose_dialog = false;
                self.is_sending = false;
                self.compose_to.clear();
                self.compose_subject.clear();
                self.compose_body = text_editor::Content::new();
                self.compose_in_reply_to = None;
                self.compose_references = None;
                self.compose_error = None;
                self.status_message = "Message sent".into();
            }

            Message::SendComplete(Err(e)) => {
                self.is_sending = false;
                self.compose_error = Some(format!("Send failed: {e}"));
            }

            _ => {}
        }
        Task::none()
    }
}

fn quote_body(body: &str, from: &str, date: &str) -> String {
    let mut out = format!("On {date}, {from} wrote:\n");
    for line in body.lines() {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn forward_body(body: &str, from: &str, date: &str, subject: &str) -> String {
    let mut out = String::from("---------- Forwarded message ----------\n");
    out.push_str(&format!("From: {from}\n"));
    out.push_str(&format!("Date: {date}\n"));
    out.push_str(&format!("Subject: {subject}\n\n"));
    out.push_str(body);
    out
}

fn build_references(in_reply_to: Option<&str>, message_id: &str) -> String {
    match in_reply_to {
        Some(irt) => format!("{irt} {message_id}"),
        None => message_id.to_string(),
    }
}
