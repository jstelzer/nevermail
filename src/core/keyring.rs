const SERVICE: &str = "nevermail";

fn key_id(username: &str, server: &str) -> String {
    format!("{username}@{server}")
}

pub fn get_password(username: &str, server: &str) -> Result<String, String> {
    let key = key_id(username, server);
    log::debug!("keyring GET: service={SERVICE:?} key={key:?}");
    let entry = keyring::Entry::new(SERVICE, &key).map_err(|e| {
        log::error!("keyring Entry::new failed for key={key:?}: {e}");
        format!("keyring error: {e}")
    })?;
    entry.get_password().map_err(|e| {
        log::warn!("keyring get_password failed for key={key:?}: {e}");
        format!("keyring get: {e}")
    })
}

pub fn set_password(username: &str, server: &str, password: &str) -> Result<(), String> {
    let key = key_id(username, server);
    log::debug!("keyring SET: service={SERVICE:?} key={key:?}");
    let entry = keyring::Entry::new(SERVICE, &key).map_err(|e| {
        log::error!("keyring Entry::new failed for key={key:?}: {e}");
        format!("keyring error: {e}")
    })?;
    entry.set_password(password).map_err(|e| {
        log::error!("keyring set_password failed for key={key:?}: {e}");
        format!("keyring set: {e}")
    })
}
