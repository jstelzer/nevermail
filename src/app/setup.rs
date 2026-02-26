use cosmic::app::Task;
use cosmic::widget;
use cosmic::Element;

use nevermail_core::config::{
    AccountConfig, FileAccountConfig, MultiAccountFileConfig, PasswordBackend, SmtpConfig,
    SmtpOverrides, new_account_id,
};
use nevermail_core::imap::ImapSession;

use super::{AccountState, AppModel, ConnectionState, Message};

impl AppModel {
    pub(super) fn handle_setup(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SetupLabelChanged(v) => {
                self.setup_label = v;
            }
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
            Message::SetupSmtpServerChanged(v) => {
                self.setup_smtp_server = v;
            }
            Message::SetupSmtpPortChanged(v) => {
                self.setup_smtp_port = v;
            }
            Message::SetupSmtpUsernameChanged(v) => {
                self.setup_smtp_username = v;
            }
            Message::SetupSmtpPasswordChanged(v) => {
                self.setup_smtp_password = v;
            }
            Message::SetupSmtpStarttlsToggled(v) => {
                self.setup_smtp_starttls = v;
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
                let label = if self.setup_label.trim().is_empty() {
                    username.clone()
                } else {
                    self.setup_label.trim().to_string()
                };

                // Determine account ID (new or editing)
                let account_id = self
                    .setup_editing_account
                    .clone()
                    .unwrap_or_else(new_account_id);

                // Build SMTP overrides
                // Store SMTP password in keyring if provided
                let smtp_password_backend = if !self.setup_smtp_password.is_empty() {
                    match nevermail_core::keyring::set_smtp_password(&account_id, &self.setup_smtp_password) {
                        Ok(()) => {
                            log::info!("SMTP password stored in keyring");
                            Some(PasswordBackend::Keyring)
                        }
                        Err(e) => {
                            log::warn!("Failed to store SMTP password in keyring: {}", e);
                            Some(PasswordBackend::Plaintext {
                                value: self.setup_smtp_password.clone(),
                            })
                        }
                    }
                } else {
                    None
                };

                let smtp_overrides = SmtpOverrides {
                    server: if self.setup_smtp_server.trim().is_empty() {
                        None
                    } else {
                        Some(self.setup_smtp_server.trim().to_string())
                    },
                    port: self.setup_smtp_port.trim().parse().ok(),
                    username: if self.setup_smtp_username.trim().is_empty() {
                        None
                    } else {
                        Some(self.setup_smtp_username.trim().to_string())
                    },
                    password: smtp_password_backend,
                    use_starttls: Some(self.setup_smtp_starttls),
                };

                // Try keyring first; fall back to plaintext on failure
                let password_backend =
                    match nevermail_core::keyring::set_password(&username, &server, &password) {
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

                // Build file account config
                let fac = FileAccountConfig {
                    id: account_id.clone(),
                    label: label.clone(),
                    server: server.clone(),
                    port,
                    username: username.clone(),
                    starttls,
                    password: password_backend,
                    email_addresses: email_addresses.clone(),
                    smtp: smtp_overrides.clone(),
                };

                // Update or add to multi-account config
                let mut multi = MultiAccountFileConfig::load()
                    .ok()
                    .flatten()
                    .unwrap_or(MultiAccountFileConfig { accounts: Vec::new() });

                if let Some(pos) = multi.accounts.iter().position(|a| a.id == account_id) {
                    multi.accounts[pos] = fac;
                } else {
                    multi.accounts.push(fac);
                }
                if let Err(e) = multi.save() {
                    log::error!("Failed to save config: {}", e);
                    self.setup_error = Some(format!("Failed to save config: {e}"));
                    return Task::none();
                }

                // Build runtime config
                let smtp_config = SmtpConfig::resolve(
                    &server,
                    &username,
                    &password,
                    &smtp_overrides,
                    &account_id,
                );
                let account_config = AccountConfig {
                    id: account_id.clone(),
                    label: label.clone(),
                    imap_server: server.clone(),
                    imap_port: port,
                    username: username.clone(),
                    password: password.clone(),
                    use_starttls: starttls,
                    email_addresses: email_addresses.clone(),
                    smtp: smtp_config,
                    smtp_overrides,
                };

                let imap_config = account_config.to_imap_config();

                // Update or add AccountState
                if let Some(idx) = self.account_index(&account_id) {
                    self.accounts[idx].config = account_config;
                    self.accounts[idx].conn_state = ConnectionState::Connecting;
                    self.accounts[idx].session = None;
                } else {
                    let mut acct = AccountState::new(account_config);
                    acct.conn_state = ConnectionState::Connecting;
                    self.accounts.push(acct);
                }

                self.show_setup_dialog = false;
                self.setup_password.clear();
                self.setup_smtp_password.clear();
                self.setup_error = None;
                self.status_message = format!("{}: Connecting...", label);

                let aid = account_id.clone();
                return cosmic::task::future(async move {
                    let result = ImapSession::connect(imap_config).await;
                    Message::AccountConnected { account_id: aid, result }
                });
            }

            Message::SetupCancel => {
                self.show_setup_dialog = false;
                if self.accounts.is_empty() {
                    self.status_message = "Not connected — no cached data".into();
                } else {
                    let total_folders: usize = self.accounts.iter().map(|a| a.folders.len()).sum();
                    self.status_message = format!("{} folders (offline)", total_folders);
                }
            }

            _ => {}
        }
        Task::none()
    }

    pub(super) fn setup_dialog(&self) -> Element<'_, Message> {
        let mut controls = widget::column().spacing(12);

        let is_edit = self.setup_editing_account.is_some();
        let title = if is_edit { "Edit Account" } else if self.password_only_mode { "Enter Password" } else { "Account Setup" };

        if !self.password_only_mode {
            controls = controls.push(
                widget::text_input("Account name (e.g. Work)", &self.setup_label)
                    .label("Label")
                    .on_input(Message::SetupLabelChanged),
            );

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

            // SMTP overrides section
            controls = controls
                .push(widget::text::body("SMTP Settings (optional — defaults to IMAP)"))
                .push(
                    widget::text_input("smtp.example.com", &self.setup_smtp_server)
                        .label("SMTP Server")
                        .on_input(Message::SetupSmtpServerChanged),
                )
                .push(
                    widget::text_input("587", &self.setup_smtp_port)
                        .label("SMTP Port")
                        .on_input(Message::SetupSmtpPortChanged),
                )
                .push(
                    widget::text_input("smtp username", &self.setup_smtp_username)
                        .label("SMTP Username")
                        .on_input(Message::SetupSmtpUsernameChanged),
                )
                .push(
                    widget::text_input::secure_input(
                        "SMTP password (blank = use IMAP password)",
                        &self.setup_smtp_password,
                        None::<Message>,
                        true,
                    )
                    .label("SMTP Password")
                    .on_input(Message::SetupSmtpPasswordChanged),
                )
                .push(
                    widget::settings::item::builder("SMTP STARTTLS")
                        .toggler(self.setup_smtp_starttls, Message::SetupSmtpStarttlsToggled),
                );
        }

        let mut dialog = widget::dialog()
            .title(title)
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
