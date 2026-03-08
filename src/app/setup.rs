use cosmic::app::Task;
use cosmic::widget;
use cosmic::Element;

use neverlight_mail_core::config::{
    AccountCapabilities, AccountConfig, FileAccountConfig, MultiAccountFileConfig, new_account_id,
};
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
            Message::SetupLabelChanged(v) => {
                self.setup_mut().update(SetupInput::SetField(FieldId::Label, v));
            }
            Message::SetupJmapUrlChanged(v) => {
                self.setup_mut().update(SetupInput::SetField(FieldId::JmapUrl, v));
            }
            Message::SetupUsernameChanged(v) => {
                self.setup_mut().update(SetupInput::SetField(FieldId::Username, v));
            }
            Message::SetupTokenChanged(v) => {
                self.setup_mut().update(SetupInput::SetField(FieldId::Token, v));
            }
            Message::SetupPasswordVisibilityToggled => {
                self.setup_password_visible = !self.setup_password_visible;
            }
            Message::SetupEmailAddressesChanged(v) => {
                self.setup_mut().update(SetupInput::SetField(FieldId::Email, v));
            }

            Message::SetupSubmit => {
                // Validate via SetupModel
                if let Some(err) = self.setup().validate() {
                    self.setup_mut().error = Some(err);
                    return Task::none();
                }

                let is_token_only = matches!(
                    self.setup().request,
                    SetupRequest::TokenOnly { .. }
                );

                // Extract validated values
                let jmap_url = self.setup().jmap_url.trim().to_string();
                let username = self.setup().username.trim().to_string();
                let token = self.setup().token.clone();
                let label = if self.setup().label.trim().is_empty() {
                    username.clone()
                } else {
                    self.setup().label.trim().to_string()
                };

                let email_addresses: Vec<String> = self.setup().email
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();

                // Determine account ID from request
                let account_id = match &self.setup().request {
                    SetupRequest::Edit { account_id } => account_id.clone(),
                    SetupRequest::TokenOnly { account_id, .. } => account_id.clone(),
                    SetupRequest::Full => new_account_id(),
                };

                // Store token in keyring
                let token_backend = if is_token_only || !token.is_empty() {
                    setup::store_token(&username, &jmap_url, &token)
                } else {
                    // Edit mode: preserve existing token
                    MultiAccountFileConfig::load()
                        .ok()
                        .flatten()
                        .and_then(|m| m.accounts.iter().find(|a| a.id == account_id).map(|a| a.auth_token.clone()))
                        .unwrap_or_else(|| setup::store_token(&username, &jmap_url, &token))
                };

                // Build file account config
                let fac = FileAccountConfig {
                    id: account_id.clone(),
                    label: label.clone(),
                    jmap_url: jmap_url.clone(),
                    username: username.clone(),
                    auth_token: token_backend,
                    email_addresses: email_addresses.clone(),
                    capabilities: AccountCapabilities::default(),
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
                let account_config = AccountConfig {
                    id: account_id.clone(),
                    label: label.clone(),
                    jmap_url,
                    username,
                    token,
                    email_addresses,
                    capabilities: AccountCapabilities::default(),
                };

                let connect_config = account_config.clone();

                // Update or add AccountState
                if let Some(idx) = self.account_index(&account_id) {
                    self.accounts[idx].config = account_config;
                    self.accounts[idx].conn_state = ConnectionState::Connecting;
                    self.accounts[idx].client = None;
                } else {
                    let mut acct = AccountState::new(account_config);
                    acct.conn_state = ConnectionState::Connecting;
                    self.accounts.push(acct);
                }

                self.setup_model = None;
                self.status_message = format!("{}: Connecting...", label);

                let aid = account_id.clone();
                return super::connect_account(connect_config, aid);
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
        let is_token_only = matches!(model.request, SetupRequest::TokenOnly { .. });

        if !is_token_only {
            controls = controls.push(
                widget::text_input("Account name (e.g. Work)", &model.label)
                    .label("Label")
                    .on_input(Message::SetupLabelChanged),
            );

            controls = controls
                .push(
                    widget::text_input("https://api.fastmail.com/jmap/session", &model.jmap_url)
                        .label("JMAP Session URL")
                        .on_input(Message::SetupJmapUrlChanged),
                )
                .push(
                    widget::text_input("you@example.com", &model.username)
                        .label("Username")
                        .on_input(Message::SetupUsernameChanged),
                );
        }

        controls = controls.push(
            widget::text_input::secure_input(
                "API token / app password",
                &model.token,
                Some(Message::SetupPasswordVisibilityToggled),
                !self.setup_password_visible,
            )
            .label("Token")
            .on_input(Message::SetupTokenChanged),
        );

        if !is_token_only {
            controls = controls.push(
                widget::text_input("you@example.com, alias@example.com", &model.email)
                    .label("Email addresses (comma-separated)")
                    .on_input(Message::SetupEmailAddressesChanged),
            );

            if let SetupRequest::Edit { account_id } = &model.request {
                controls = controls.push(
                    widget::button::destructive("Delete Account")
                        .on_press(Message::RequestDeleteAccount(account_id.clone())),
                );
            }
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
