use cosmic::app::Task;
use cosmic::widget;
use cosmic::Element;

use super::{AppModel, Message};
use crate::config::{Config, FileConfig, PasswordBackend};
use crate::core::imap::ImapSession;

impl AppModel {
    pub(super) fn handle_setup(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SetupServerChanged(v) => {
                self.setup_server = v;
            }
            Message::SetupPortChanged(v) => {
                self.setup_port = v;
            }
            Message::SetupUsernameChanged(v) => {
                self.setup_username = v;
            }
            Message::SetupPasswordChanged(v) => {
                self.setup_password = v;
            }
            Message::SetupStarttlsToggled(v) => {
                self.setup_starttls = v;
            }
            Message::SetupPasswordVisibilityToggled => {
                self.setup_password_visible = !self.setup_password_visible;
            }
            Message::SetupEmailAddressesChanged(v) => {
                self.setup_email_addresses = v;
            }

            Message::SetupSubmit => {
                // Validate
                if self.setup_server.trim().is_empty()
                    || self.setup_username.trim().is_empty()
                    || self.setup_password.is_empty()
                {
                    self.setup_error = Some("All fields are required".into());
                    return Task::none();
                }
                let port: u16 = match self.setup_port.trim().parse() {
                    Ok(p) => p,
                    Err(_) => {
                        self.setup_error = Some("Port must be a number (e.g. 993)".into());
                        return Task::none();
                    }
                };

                let email_addresses: Vec<String> = self
                    .setup_email_addresses
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !self.password_only_mode && email_addresses.is_empty() {
                    self.setup_error =
                        Some("At least one email address is required for sending".into());
                    return Task::none();
                }

                let server = self.setup_server.trim().to_string();
                let username = self.setup_username.trim().to_string();
                let password = self.setup_password.clone();
                let starttls = self.setup_starttls;

                // Try keyring first; fall back to plaintext on failure
                let password_backend =
                    match crate::core::keyring::set_password(&username, &server, &password) {
                        Ok(()) => {
                            log::info!("Password stored in keyring");
                            PasswordBackend::Keyring
                        }
                        Err(e) => {
                            log::warn!("Keyring unavailable ({}), using plaintext", e);
                            PasswordBackend::Plaintext {
                                value: password.clone(),
                            }
                        }
                    };

                // Save config file
                let fc = FileConfig {
                    server: server.clone(),
                    port,
                    username: username.clone(),
                    starttls,
                    password: password_backend,
                    email_addresses: email_addresses.clone(),
                };
                if let Err(e) = fc.save() {
                    log::error!("Failed to save config: {}", e);
                    self.setup_error = Some(format!("Failed to save config: {e}"));
                    return Task::none();
                }

                // Build runtime config and connect
                let config = Config {
                    imap_server: server,
                    imap_port: port,
                    username,
                    password,
                    use_starttls: starttls,
                    email_addresses,
                };

                self.config = Some(config.clone());
                self.show_setup_dialog = false;
                self.setup_password.clear();
                self.setup_error = None;
                self.is_syncing = true;
                self.status_message = "Connecting...".into();

                return cosmic::task::future(async move {
                    Message::Connected(ImapSession::connect(config).await)
                });
            }

            Message::SetupCancel => {
                self.show_setup_dialog = false;
                if self.folders.is_empty() {
                    self.status_message = "Not connected — no cached data".into();
                } else {
                    self.status_message =
                        format!("{} folders (offline)", self.folders.len());
                }
            }

            _ => {}
        }
        Task::none()
    }

    pub(super) fn setup_dialog(&self) -> Element<'_, Message> {
        let mut controls = widget::column().spacing(12);

        if !self.password_only_mode {
            controls = controls
                .push(
                    widget::text_input("mail.example.com", &self.setup_server)
                        .label("IMAP Server")
                        .on_input(Message::SetupServerChanged),
                )
                .push(
                    widget::text_input("993", &self.setup_port)
                        .label("Port")
                        .on_input(Message::SetupPortChanged),
                )
                .push(
                    widget::text_input("you@example.com", &self.setup_username)
                        .label("Username")
                        .on_input(Message::SetupUsernameChanged),
                );
        }

        controls = controls.push(
            widget::text_input::secure_input(
                "Password",
                &self.setup_password,
                Some(Message::SetupPasswordVisibilityToggled),
                !self.setup_password_visible,
            )
            .label("Password")
            .on_input(Message::SetupPasswordChanged),
        );

        if !self.password_only_mode {
            controls = controls
                .push(
                    widget::text_input("you@example.com, alias@example.com", &self.setup_email_addresses)
                        .label("Email addresses (comma-separated)")
                        .on_input(Message::SetupEmailAddressesChanged),
                )
                .push(
                    widget::settings::item::builder("Use STARTTLS")
                        .toggler(self.setup_starttls, Message::SetupStarttlsToggled),
                );
        }

        let mut dialog = widget::dialog()
            .title(if self.password_only_mode {
                "Enter Password"
            } else {
                "Account Setup"
            })
            .control(controls)
            .primary_action(
                widget::button::suggested("Connect").on_press(Message::SetupSubmit),
            )
            .secondary_action(
                widget::button::standard("Cancel").on_press(Message::SetupCancel),
            );

        if let Some(ref err) = self.setup_error {
            dialog = dialog.body(err.as_str());
        }

        dialog.into()
    }
}
