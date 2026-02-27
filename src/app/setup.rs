use cosmic::app::Task;
use cosmic::widget;
use cosmic::Element;

use neverlight_mail_core::config::{
    AccountConfig, FileAccountConfig, MultiAccountFileConfig, SmtpConfig,
    new_account_id,
};
use neverlight_mail_core::imap::ImapSession;
use neverlight_mail_core::setup::{self, FieldId, SetupInput, SetupRequest};

use super::{AccountState, AppModel, ConnectionState, Message};

impl AppModel {
    /// Access the setup model, panicking if absent. Only call when you've
    /// already checked `self.setup_model.is_some()`.
    fn setup(&self) -> &setup::SetupModel {
        self.setup_model.as_ref().expect("setup_model is None")
    }
    fn setup_mut(&mut self) -> &mut setup::SetupModel {
        self.setup_model.as_mut().expect("setup_model is None")
    }

    pub(super) fn handle_setup(&mut self, message: Message) -> Task<Message> {
        match message {
            // Core IMAP fields → SetupModel
            Message::SetupLabelChanged(v) => {
                self.setup_mut().update(SetupInput::SetField(FieldId::Label, v));
            }
            Message::SetupServerChanged(v) => {
                self.setup_mut().update(SetupInput::SetField(FieldId::Server, v));
            }
            Message::SetupPortChanged(v) => {
                self.setup_mut().update(SetupInput::SetField(FieldId::Port, v));
            }
            Message::SetupUsernameChanged(v) => {
                self.setup_mut().update(SetupInput::SetField(FieldId::Username, v));
            }
            Message::SetupPasswordChanged(v) => {
                self.setup_mut().update(SetupInput::SetField(FieldId::Password, v));
            }
            Message::SetupStarttlsToggled(v) => {
                self.setup_mut().update(SetupInput::SetToggle(FieldId::Starttls, v));
            }
            Message::SetupPasswordVisibilityToggled => {
                self.setup_password_visible = !self.setup_password_visible;
            }
            Message::SetupEmailAddressesChanged(v) => {
                self.setup_mut().update(SetupInput::SetField(FieldId::Email, v));
            }
            Message::SetupSmtpServerChanged(v) => {
                self.setup_mut().update(SetupInput::SetField(FieldId::SmtpServer, v));
            }
            Message::SetupSmtpPortChanged(v) => {
                self.setup_mut().update(SetupInput::SetField(FieldId::SmtpPort, v));
            }
            Message::SetupSmtpUsernameChanged(v) => {
                self.setup_mut().update(SetupInput::SetField(FieldId::SmtpUsername, v));
            }
            Message::SetupSmtpPasswordChanged(v) => {
                self.setup_mut().update(SetupInput::SetField(FieldId::SmtpPassword, v));
            }
            Message::SetupSmtpStarttlsToggled(v) => {
                self.setup_mut().update(SetupInput::SetToggle(FieldId::SmtpStarttls, v));
            }

            Message::SetupSubmit => {
                // Validate core fields via SetupModel
                if let Some(err) = self.setup().validate() {
                    self.setup_mut().error = Some(err);
                    return Task::none();
                }

                let is_password_only = matches!(
                    self.setup().request,
                    SetupRequest::PasswordOnly { .. }
                );

                // Extract validated values from SetupModel
                let server = self.setup().server.trim().to_string();
                let username = self.setup().username.trim().to_string();
                let password = self.setup().password.clone();
                let starttls = self.setup().starttls;
                let port: u16 = self.setup().port.trim().parse().unwrap(); // validated above
                let label = if self.setup().label.trim().is_empty() {
                    username.clone()
                } else {
                    self.setup().label.trim().to_string()
                };

                // Parse comma-separated email addresses from core model
                let email_addresses: Vec<String> = self.setup().email
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();

                // Determine account ID from request
                let account_id = match &self.setup().request {
                    SetupRequest::Edit { account_id } => account_id.clone(),
                    SetupRequest::PasswordOnly { account_id, .. } => account_id.clone(),
                    SetupRequest::Full => new_account_id(),
                };

                // Build SMTP overrides from core model fields
                let smtp_pw = if is_password_only {
                    None
                } else if self.setup().smtp_password.is_empty() {
                    // Edit mode: preserve existing SMTP password
                    if let SetupRequest::Edit { account_id: ref aid } = self.setup().request {
                        MultiAccountFileConfig::load()
                            .ok()
                            .flatten()
                            .and_then(|m| m.accounts.iter().find(|a| a.id == *aid).map(|a| a.smtp.password.clone()))
                            .flatten()
                    } else {
                        None
                    }
                } else {
                    setup::store_smtp_password(&account_id, &self.setup().smtp_password)
                };
                let smtp_overrides = self.setup().build_smtp_overrides(smtp_pw);

                // Store IMAP password via shared helper
                let password_backend = if is_password_only || !password.is_empty() {
                    setup::store_password(&username, &server, &password)
                } else {
                    // Edit mode: preserve existing password (handled by core's try_submit,
                    // but COSMIC does its own persist, so look it up)
                    MultiAccountFileConfig::load()
                        .ok()
                        .flatten()
                        .and_then(|m| m.accounts.iter().find(|a| a.id == account_id).map(|a| a.password.clone()))
                        .unwrap_or_else(|| setup::store_password(&username, &server, &password))
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
                    self.setup_mut().error = Some(format!("Failed to save config: {e}"));
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

                self.setup_model = None;
                self.status_message = format!("{}: Connecting...", label);

                let aid = account_id.clone();
                return cosmic::task::future(async move {
                    let result = ImapSession::connect(imap_config).await;
                    Message::AccountConnected { account_id: aid, result }
                });
            }

            Message::SetupCancel => {
                self.setup_model = None;
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
        let model = self.setup();
        let mut controls = widget::column().spacing(12);

        let title = model.title();
        let is_password_only = matches!(model.request, SetupRequest::PasswordOnly { .. });

        if !is_password_only {
            controls = controls.push(
                widget::text_input("Account name (e.g. Work)", &model.label)
                    .label("Label")
                    .on_input(Message::SetupLabelChanged),
            );

            controls = controls
                .push(
                    widget::text_input("mail.example.com", &model.server)
                        .label("IMAP Server")
                        .on_input(Message::SetupServerChanged),
                )
                .push(
                    widget::text_input("993", &model.port)
                        .label("Port")
                        .on_input(Message::SetupPortChanged),
                )
                .push(
                    widget::text_input("you@example.com", &model.username)
                        .label("Username")
                        .on_input(Message::SetupUsernameChanged),
                );
        }

        controls = controls.push(
            widget::text_input::secure_input(
                "Password",
                &model.password,
                Some(Message::SetupPasswordVisibilityToggled),
                !self.setup_password_visible,
            )
            .label("Password")
            .on_input(Message::SetupPasswordChanged),
        );

        if !is_password_only {
            controls = controls
                .push(
                    widget::text_input("you@example.com, alias@example.com", &model.email)
                        .label("Email addresses (comma-separated)")
                        .on_input(Message::SetupEmailAddressesChanged),
                )
                .push(
                    widget::settings::item::builder("Use STARTTLS")
                        .toggler(model.starttls, Message::SetupStarttlsToggled),
                );

            // SMTP overrides section
            controls = controls
                .push(widget::text::body("SMTP Settings (optional — defaults to IMAP)"))
                .push(
                    widget::text_input("smtp.example.com", &model.smtp_server)
                        .label("SMTP Server")
                        .on_input(Message::SetupSmtpServerChanged),
                )
                .push(
                    widget::text_input("587", &model.smtp_port)
                        .label("SMTP Port")
                        .on_input(Message::SetupSmtpPortChanged),
                )
                .push(
                    widget::text_input("smtp username", &model.smtp_username)
                        .label("SMTP Username")
                        .on_input(Message::SetupSmtpUsernameChanged),
                )
                .push(
                    widget::text_input::secure_input(
                        "SMTP password (blank = use IMAP password)",
                        &model.smtp_password,
                        None::<Message>,
                        true,
                    )
                    .label("SMTP Password")
                    .on_input(Message::SetupSmtpPasswordChanged),
                )
                .push(
                    widget::settings::item::builder("SMTP STARTTLS")
                        .toggler(model.smtp_starttls, Message::SetupSmtpStarttlsToggled),
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

        if let Some(ref err) = model.error {
            dialog = dialog.body(err.as_str());
        }

        dialog.into()
    }
}
