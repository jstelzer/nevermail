use cosmic::app::Task;
use cosmic::dialog::file_chooser;
use cosmic::widget::text_editor;

use super::{AppModel, Message};
use crate::core::models::{AttachmentData, DraggedFiles};
use crate::core::smtp::{self, OutgoingEmail};
use crate::ui::compose_dialog::ComposeMode;

/// Guess MIME type from file extension.
fn mime_from_ext(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("gz" | "gzip") => "application/gzip",
        Some("tar") => "application/x-tar",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("txt") => "text/plain",
        Some("html" | "htm") => "text/html",
        Some("css") => "text/css",
        Some("csv") => "text/csv",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        Some("doc") => "application/msword",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xls") => "application/vnd.ms-excel",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("odt") => "application/vnd.oasis.opendocument.text",
        Some("ods") => "application/vnd.oasis.opendocument.spreadsheet",
        Some("mp3") => "audio/mpeg",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        _ => "application/octet-stream",
    }
}

impl AppModel {
    pub(super) fn handle_compose(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ComposeNew => {
                if self.show_setup_dialog || self.show_compose_dialog {
                    return Task::none();
                }
                self.compose_mode = ComposeMode::New;
                self.compose_account = self.active_account.unwrap_or(0);
                self.compose_from = 0;
                self.compose_to.clear();
                self.compose_subject.clear();
                self.compose_body = text_editor::Content::new();
                self.compose_in_reply_to = None;
                self.compose_references = None;
                self.compose_attachments.clear();
                self.compose_error = None;
                self.is_sending = false;
                self.show_compose_dialog = true;
                self.refresh_compose_cache();
            }

            Message::ComposeReply => {
                if self.show_setup_dialog || self.show_compose_dialog {
                    return Task::none();
                }
                if let Some(index) = self.selected_message {
                    if let Some(msg) = self.messages.get(index) {
                        self.compose_mode = ComposeMode::Reply;
                        // Auto-select owning account
                        self.compose_account = self.account_for_mailbox(msg.mailbox_hash)
                            .unwrap_or(self.active_account.unwrap_or(0));
                        self.compose_to = msg.from.clone();

                        let subj = &msg.subject;
                        self.compose_subject = if subj.starts_with("Re: ") {
                            subj.clone()
                        } else {
                            format!("Re: {subj}")
                        };

                        let quoted = quote_body(&self.preview_body, &msg.from, &msg.date);
                        self.compose_body =
                            text_editor::Content::with_text(&format!("\n\n{quoted}"));

                        self.compose_in_reply_to = Some(msg.message_id.clone());
                        self.compose_references = Some(build_references(
                            msg.in_reply_to.as_deref(),
                            &msg.message_id,
                        ));
                        self.compose_attachments.clear();
                        self.compose_error = None;
                        self.is_sending = false;
                        self.show_compose_dialog = true;
                        self.refresh_compose_cache();
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
                        // Auto-select owning account
                        self.compose_account = self.account_for_mailbox(msg.mailbox_hash)
                            .unwrap_or(self.active_account.unwrap_or(0));
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
                        self.compose_body =
                            text_editor::Content::with_text(&format!("\n\n{fwd}"));

                        self.compose_in_reply_to = None;
                        self.compose_references = None;
                        self.compose_attachments = self.preview_attachments.clone();
                        self.compose_error = None;
                        self.is_sending = false;
                        self.show_compose_dialog = true;
                        self.refresh_compose_cache();
                    }
                }
            }

            Message::ComposeAccountChanged(i) => {
                self.compose_account = i;
                self.compose_from = 0; // Reset from index when account changes
                self.refresh_compose_cache();
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

            Message::ComposeAttach => {
                return cosmic::task::future(async move {
                    let dialog = file_chooser::open::Dialog::new().title("Attach files");
                    match dialog.open_files().await {
                        Ok(response) => {
                            let mut attachments = Vec::new();
                            for url in response.urls() {
                                let path = match url.to_file_path() {
                                    Ok(p) => p,
                                    Err(_) => continue,
                                };
                                let data = match tokio::fs::read(&path).await {
                                    Ok(d) => d,
                                    Err(e) => {
                                        return Message::ComposeAttachLoaded(Err(format!(
                                            "Failed to read {}: {e}",
                                            path.display()
                                        )));
                                    }
                                };
                                let filename = path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "attachment".into());
                                let mime_type = mime_from_ext(&path).to_owned();
                                attachments.push(AttachmentData {
                                    filename,
                                    mime_type,
                                    data,
                                });
                            }
                            Message::ComposeAttachLoaded(Ok(attachments))
                        }
                        Err(file_chooser::Error::Cancelled) => Message::Noop,
                        Err(e) => {
                            Message::ComposeAttachLoaded(Err(format!("File picker error: {e}")))
                        }
                    }
                });
            }

            Message::ComposeAttachLoaded(Ok(files)) => {
                self.compose_attachments.extend(files);
            }
            Message::ComposeAttachLoaded(Err(e)) => {
                self.compose_error = Some(e);
            }

            Message::ComposeRemoveAttachment(i) => {
                if i < self.compose_attachments.len() {
                    self.compose_attachments.remove(i);
                }
            }

            Message::ComposeDragEnter => {
                self.compose_drag_hover = true;
            }
            Message::ComposeDragLeave => {
                self.compose_drag_hover = false;
            }
            Message::ComposeFileTransfer(key) => {
                self.compose_drag_hover = false;
                return cosmic::task::future(async move {
                    let result = async {
                        let ft = ashpd::documents::FileTransfer::new().await?;
                        ft.retrieve_files(&key).await
                    }
                    .await;
                    Message::ComposeFileTransferResolved(
                        result.map_err(|e| format!("Portal file transfer failed: {e}")),
                    )
                });
            }
            Message::ComposeFileTransferResolved(Ok(paths)) => {
                return cosmic::task::future(async move {
                    read_paths_as_attachments(paths).await
                });
            }
            Message::ComposeFileTransferResolved(Err(e)) => {
                self.compose_error = Some(e);
            }
            Message::ComposeFilesDropped(DraggedFiles(uri_list)) => {
                self.compose_drag_hover = false;
                let paths = parse_uri_list(&uri_list);
                if paths.is_empty() {
                    return Task::none();
                }
                return cosmic::task::future(async move {
                    read_paths_as_attachments(paths).await
                });
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

                let Some(acct) = self.accounts.get(self.compose_account) else {
                    self.compose_error = Some("No account selected".into());
                    return Task::none();
                };

                let from_addrs = &acct.config.email_addresses;
                let from_addr = from_addrs
                    .get(self.compose_from)
                    .cloned()
                    .unwrap_or_else(|| {
                        from_addrs.first().cloned().unwrap_or_default()
                    });
                if from_addr.is_empty() {
                    self.compose_error = Some(
                        "No email address configured. Re-run setup to add one.".into(),
                    );
                    return Task::none();
                }

                self.is_sending = true;
                self.compose_error = None;

                let smtp_config = acct.config.smtp.clone();
                let email = OutgoingEmail {
                    from: from_addr,
                    to: self.compose_to.clone(),
                    subject: self.compose_subject.clone(),
                    body: body_text,
                    in_reply_to: self.compose_in_reply_to.clone(),
                    references: self.compose_references.clone(),
                    attachments: self.compose_attachments.clone(),
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
                self.compose_attachments.clear();
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

/// Parse a text/uri-list string into local file paths.
/// Skips blank lines, comments (lines starting with #), and non-file:// URIs.
fn parse_uri_list(uri_list: &str) -> Vec<String> {
    uri_list
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let url = url::Url::parse(line).ok()?;
            let path = url.to_file_path().ok()?;
            Some(path.to_string_lossy().into_owned())
        })
        .collect()
}

/// Read a list of file paths into AttachmentData. Shared by portal and uri-list codepaths.
async fn read_paths_as_attachments(paths: Vec<String>) -> Message {
    let mut attachments = Vec::new();
    for p in &paths {
        let path = std::path::Path::new(p);
        let data = match tokio::fs::read(path).await {
            Ok(d) => d,
            Err(e) => {
                return Message::ComposeAttachLoaded(Err(format!(
                    "Failed to read {}: {e}",
                    path.display()
                )));
            }
        };
        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".into());
        let mime_type = mime_from_ext(path).to_owned();
        attachments.push(AttachmentData {
            filename,
            mime_type,
            data,
        });
    }
    Message::ComposeAttachLoaded(Ok(attachments))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_uri_list_basic() {
        let input = "file:///home/user/doc.pdf\nfile:///tmp/photo.jpg\n";
        let paths = parse_uri_list(input);
        assert_eq!(paths, vec!["/home/user/doc.pdf", "/tmp/photo.jpg"]);
    }

    #[test]
    fn parse_uri_list_skips_comments_and_blanks() {
        let input = "# this is a comment\n\nfile:///home/user/doc.pdf\n\n# another comment\n";
        let paths = parse_uri_list(input);
        assert_eq!(paths, vec!["/home/user/doc.pdf"]);
    }

    #[test]
    fn parse_uri_list_skips_non_file_uris() {
        let input = "https://example.com/file.pdf\nfile:///home/user/doc.pdf\n";
        let paths = parse_uri_list(input);
        assert_eq!(paths, vec!["/home/user/doc.pdf"]);
    }

    #[test]
    fn parse_uri_list_handles_spaces_in_paths() {
        let input = "file:///home/user/my%20documents/report.pdf\n";
        let paths = parse_uri_list(input);
        assert_eq!(paths, vec!["/home/user/my documents/report.pdf"]);
    }

    #[test]
    fn parse_uri_list_empty_input() {
        assert!(parse_uri_list("").is_empty());
        assert!(parse_uri_list("   \n  \n").is_empty());
    }

    #[test]
    fn parse_uri_list_trims_whitespace() {
        let input = "  file:///home/user/doc.pdf  \n";
        let paths = parse_uri_list(input);
        assert_eq!(paths, vec!["/home/user/doc.pdf"]);
    }

    #[test]
    fn mime_from_ext_common_types() {
        assert_eq!(mime_from_ext(std::path::Path::new("file.pdf")), "application/pdf");
        assert_eq!(mime_from_ext(std::path::Path::new("photo.jpg")), "image/jpeg");
        assert_eq!(mime_from_ext(std::path::Path::new("photo.JPEG")), "image/jpeg");
        assert_eq!(mime_from_ext(std::path::Path::new("doc.txt")), "text/plain");
        assert_eq!(mime_from_ext(std::path::Path::new("unknown.xyz")), "application/octet-stream");
        assert_eq!(mime_from_ext(std::path::Path::new("noext")), "application/octet-stream");
    }
}
