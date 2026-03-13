use cosmic::app::Task;
use cosmic::widget;
use cosmic::Element;

use neverlight_mail_core::config::{
    AccountCapabilities, AccountConfig, AuthBackend, AuthMethod, FileAccountConfig,
    MultiAccountFileConfig, new_account_id, DEFAULT_JMAP_SESSION_URL,
};
use neverlight_mail_oauth::{AppInfo, OAuthRedirectHandler};
use neverlight_mail_core::setup::{self, FieldId, SetupInput, SetupRequest};

use super::{AccountState, AppModel, ConnectionState, Message, OAuthSetupPhase, OAuthTokenResult};

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
                return self.handle_setup_submit();
            }

            Message::SetupCancel => {
                self.setup_model = None;
                self.oauth_phase = OAuthSetupPhase::Inactive;
                self.oauth_error = None;
                if self.accounts.is_empty() {
                    self.status_message = "Not connected — no cached data".into();
                } else {
                    let total_folders: usize = self.accounts.iter().map(|a| a.folders.len()).sum();
                    self.status_message = format!("{} folders (offline)", total_folders);
                }
            }

            // OAuth: single-shot flow (discover → register → browser → exchange)
            Message::SetupOAuthStart => {
                return self.handle_oauth_start();
            }
            Message::SetupOAuthTokensReceived(result) => {
                return self.handle_oauth_tokens_received(result);
            }

            _ => {}
        }
        Task::none()
    }

    fn handle_setup_submit(&mut self) -> Task<Message> {
        let is_token_only = matches!(
            self.setup().request,
            SetupRequest::TokenOnly { .. }
        );

        if is_token_only {
            if self.setup().token.is_empty() {
                self.setup_mut().error = Some("API token is required".into());
                return Task::none();
            }
        } else if let Some(err) = self.setup().validate() {
            self.setup_mut().error = Some(err);
            return Task::none();
        }

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

        let account_id = match &self.setup().request {
            SetupRequest::Edit { account_id }
            | SetupRequest::TokenOnly { account_id, .. }
            | SetupRequest::Reauth { account_id, .. } => account_id.clone(),
            SetupRequest::Full => new_account_id(),
        };

        let token_backend = if is_token_only || !token.is_empty() {
            setup::store_token(&username, &jmap_url, &token)
        } else {
            MultiAccountFileConfig::load()
                .ok()
                .flatten()
                .and_then(|m| m.accounts.iter().find(|a| a.id == account_id).map(|a| a.auth.clone()))
                .unwrap_or_else(|| setup::store_token(&username, &jmap_url, &token))
        };

        let fac = FileAccountConfig {
            id: account_id.clone(),
            label: label.clone(),
            jmap_url: jmap_url.clone(),
            username: username.clone(),
            auth: token_backend,
            email_addresses: email_addresses.clone(),
            capabilities: AccountCapabilities::default(),
            max_messages_per_mailbox: None,
        };

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

        let account_config = AccountConfig {
            id: account_id.clone(),
            label: label.clone(),
            jmap_url,
            username,
            auth: AuthMethod::AppPassword { token },
            email_addresses,
            capabilities: AccountCapabilities::default(),
            max_messages_per_mailbox: None,
        };

        self.finalize_setup(account_id, label, account_config)
    }

    /// Start full OAuth flow: discover → register → browser auth → token exchange.
    fn handle_oauth_start(&mut self) -> Task<Message> {
        let jmap_url = self.setup().jmap_url.trim().to_string();

        if jmap_url.is_empty() || !jmap_url.starts_with("https://") {
            self.setup_mut().error = Some("JMAP URL required for OAuth discovery".into());
            return Task::none();
        }

        self.oauth_phase = OAuthSetupPhase::Discovering;
        self.oauth_error = None;
        self.setup_mut().error = None;

        cosmic::task::future(async move {
            let result: Result<OAuthTokenResult, String> = async {
                // Bind redirect listener first (OS-assigned port)
                let redirect =
                    neverlight_mail_oauth::LocalServerRedirect::bind("Neverlight Mail").await
                        .map_err(|e| e.to_string())?;

                let app_info = AppInfo {
                    client_name: "Neverlight Mail".into(),
                    client_uri: "https://github.com/jstelzer/neverlight-mail".into(),
                    software_id: "com.neverlight.email".into(),
                    software_version: env!("CARGO_PKG_VERSION").into(),
                    redirect_uri: redirect.redirect_uri(),
                };

                // Discover + register
                let flow = neverlight_mail_oauth::OAuthFlow::discover_and_register(
                    &jmap_url,
                    &app_info,
                    "urn:ietf:params:oauth:scope:mail",
                )
                .await
                .map_err(|e| e.to_string())?;

                // Open browser and wait for authorization
                let token_set = flow.authorize(&redirect).await.map_err(|e| e.to_string())?;

                Ok(OAuthTokenResult {
                    issuer: flow.issuer().to_string(),
                    client_id: flow.client_id().to_string(),
                    token_endpoint: flow.token_endpoint().to_string(),
                    resource: flow.resource().to_string(),
                    access_token: token_set.access_token,
                    refresh_token: token_set.refresh_token,
                })
            }
            .await;

            Message::SetupOAuthTokensReceived(result)
        })
    }

    /// Tokens received — save config and connect.
    fn handle_oauth_tokens_received(
        &mut self,
        result: Result<OAuthTokenResult, String>,
    ) -> Task<Message> {
        let tokens = match result {
            Ok(t) => t,
            Err(e) => {
                log::error!("OAuth flow failed: {}", e);
                self.oauth_phase = OAuthSetupPhase::Inactive;
                self.oauth_error = Some(e.clone());
                if let Some(m) = self.setup_model.as_mut() {
                    m.error = Some(format!("OAuth failed: {e}"));
                }
                return Task::none();
            }
        };

        let jmap_url = self.setup().jmap_url.trim().to_string();
        let username = self.setup().username.trim().to_string();
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

        let account_id = match &self.setup().request {
            SetupRequest::Edit { account_id }
            | SetupRequest::TokenOnly { account_id, .. }
            | SetupRequest::Reauth { account_id, .. } => account_id.clone(),
            SetupRequest::Full => new_account_id(),
        };

        // Store refresh token in keyring
        let refresh_token_plaintext =
            match neverlight_mail_core::keyring::set_oauth_refresh(&account_id, &tokens.refresh_token) {
                Ok(()) => None,
                Err(e) => {
                    log::warn!("Keyring unavailable for OAuth ({}), using plaintext", e);
                    Some(tokens.refresh_token.clone())
                }
            };

        // For reauth, preserve existing config fields (capabilities, emails, etc.)
        let mut multi = MultiAccountFileConfig::load()
            .ok()
            .flatten()
            .unwrap_or(MultiAccountFileConfig { accounts: Vec::new() });

        let existing = multi.accounts.iter().find(|a| a.id == account_id);
        let (caps, max_msgs, emails_resolved) = if let Some(ex) = existing {
            (
                ex.capabilities.clone(),
                ex.max_messages_per_mailbox,
                if email_addresses.is_empty() { ex.email_addresses.clone() } else { email_addresses.clone() },
            )
        } else {
            (AccountCapabilities::default(), None, email_addresses.clone())
        };

        let fac = FileAccountConfig {
            id: account_id.clone(),
            label: label.clone(),
            jmap_url: jmap_url.clone(),
            username: username.clone(),
            auth: AuthBackend::OAuth {
                issuer: tokens.issuer.clone(),
                client_id: tokens.client_id.clone(),
                resource: tokens.resource.clone(),
                token_endpoint: tokens.token_endpoint.clone(),
                refresh_token_plaintext,
            },
            email_addresses: emails_resolved,
            capabilities: caps,
            max_messages_per_mailbox: max_msgs,
        };

        if let Some(pos) = multi.accounts.iter().position(|a| a.id == account_id) {
            multi.accounts[pos] = fac;
        } else {
            multi.accounts.push(fac);
        }
        if let Err(e) = multi.save() {
            log::error!("Failed to save OAuth config: {}", e);
            if let Some(m) = self.setup_model.as_mut() {
                m.error = Some(format!("Failed to save config: {e}"));
            }
            return Task::none();
        }

        let account_config = AccountConfig {
            id: account_id.clone(),
            label: label.clone(),
            jmap_url,
            username,
            auth: AuthMethod::OAuth {
                issuer: tokens.issuer,
                client_id: tokens.client_id,
                token_endpoint: tokens.token_endpoint,
                refresh_token: tokens.refresh_token,
                access_token: Some(tokens.access_token),
                resource: tokens.resource,
            },
            email_addresses,
            capabilities: AccountCapabilities::default(),
            max_messages_per_mailbox: None,
        };

        self.oauth_phase = OAuthSetupPhase::Inactive;
        self.oauth_error = None;
        self.finalize_setup(account_id, label, account_config)
    }

    /// Shared setup finalization: update AccountState, close dialog, start connecting.
    fn finalize_setup(
        &mut self,
        account_id: String,
        label: String,
        account_config: AccountConfig,
    ) -> Task<Message> {
        let connect_config = account_config.clone();

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

        super::connect_account(connect_config, account_id)
    }

    pub(super) fn setup_dialog(&self) -> Element<'_, Message> {
        let model = self.setup();
        let mut controls = widget::column().spacing(12);

        let title = model.title();
        let is_token_only = matches!(model.request, SetupRequest::TokenOnly { .. });
        let is_reauth = model.is_reauth();

        if is_reauth {
            // Reauth: show read-only account info + prominent OAuth button
            controls = controls.push(
                widget::text::body(format!("Account: {}", model.label)),
            );
            controls = controls.push(
                widget::text::body(format!("Server: {}", model.jmap_url)),
            );
            controls = controls.push(
                widget::text::body(format!("Username: {}", model.username)),
            );
            controls = controls.push(widget::text::caption(
                "Your authorization has expired. Sign in again to reconnect.",
            ));

            let oauth_label = match self.oauth_phase {
                OAuthSetupPhase::Inactive => "Re-authorize with browser",
                OAuthSetupPhase::Discovering => "Signing in...",
            };
            let oauth_enabled = self.oauth_phase == OAuthSetupPhase::Inactive;
            let mut oauth_btn = widget::button::suggested(oauth_label);
            if oauth_enabled {
                oauth_btn = oauth_btn.on_press(Message::SetupOAuthStart);
            }
            controls = controls.push(oauth_btn);
        } else if !is_token_only {
            controls = controls.push(
                widget::text_input("Account name (e.g. Work)", &model.label)
                    .label("Label")
                    .on_input(Message::SetupLabelChanged),
            );

            controls = controls
                .push(
                    widget::text_input(DEFAULT_JMAP_SESSION_URL, &model.jmap_url)
                        .label("JMAP Session URL")
                        .on_input(Message::SetupJmapUrlChanged),
                )
                .push(
                    widget::text_input("you@example.com", &model.username)
                        .label("Username")
                        .on_input(Message::SetupUsernameChanged),
                );

            // OAuth sign-in button
            let oauth_label = match self.oauth_phase {
                OAuthSetupPhase::Inactive => "Sign in with browser",
                OAuthSetupPhase::Discovering => "Signing in...",
            };
            let oauth_enabled = self.oauth_phase == OAuthSetupPhase::Inactive;
            let mut oauth_btn = widget::button::standard(oauth_label);
            if oauth_enabled {
                oauth_btn = oauth_btn.on_press(Message::SetupOAuthStart);
            }
            controls = controls.push(oauth_btn);

            // Divider text
            controls = controls.push(
                widget::text::caption("— or enter an app password manually —"),
            );
        }

        if !is_reauth {
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
        }

        if !is_token_only && !is_reauth {
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

        let error_text = self
            .oauth_error
            .as_deref()
            .or(model.error.as_deref());

        let mut dialog = widget::dialog()
            .title(title)
            .control(controls);

        if !is_reauth {
            dialog = dialog.primary_action(
                widget::button::suggested("Connect").on_press(Message::SetupSubmit),
            );
        }
        dialog = dialog.secondary_action(
            widget::button::standard("Cancel").on_press(Message::SetupCancel),
        );

        if let Some(err) = error_text {
            dialog = dialog.body(err);
        }

        dialog.into()
    }
}
