#![doc(
    html_logo_url = "https://raw.githubusercontent.com/s00d/tauri-plugin-keyring-store/main/assets/docs-logo.png",
    html_favicon_url = "https://raw.githubusercontent.com/s00d/tauri-plugin-keyring-store/main/assets/docs-logo.png"
)]

//! OS keychain / credential-manager storage with a Stronghold-shaped API (sessions, vault
//! locations, optional SLIP10/BIP39 procedures). See the crate README for IPC and guest-js.
//!
//! ## Rust usage
//!
//! ```rust,no_run
//! use tauri::Manager;
//! use tauri_plugin_keyring_store::KeyringExt;
//!
//! fn example<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
//!   let _svc = app.keyring().store.service();
//! }
//! ```
//!
//! ## Examples (naming helpers)
//!
//! ```rust
//! use tauri_plugin_keyring_store::{join_prefix, split_prefixed};
//!
//! # fn main() -> Result<(), tauri_plugin_keyring_store::Error> {
//! let account = join_prefix("billing", "stripe_sk")?;
//! assert_eq!(split_prefixed(&account)?, ("billing".into(), "stripe_sk".into()));
//! # Ok(())
//! # }
//! ```

mod backend;
mod commands;
mod error;
mod models;
pub mod naming;
mod plugin;
mod store;

mod backup_crypto;

#[cfg(feature = "crypto")]
mod crypto;

pub use error::{Error, Result};
pub use models::*;
pub use naming::{join_prefix, split_prefixed, PREFIX_SEPARATOR};
pub use plugin::KeyringPlugin;
#[cfg(any(target_os = "ios", target_os = "macos"))]
pub use store::WriteAccessibility;
pub use store::{KeyringAvailability, KeyringStore, SessionRegistry};

use tauri::plugin::{Builder as PluginBuilder, TauriPlugin};
use tauri::{Manager, Runtime};

/// Access managed [`KeyringPlugin`] state from [`tauri::App`], [`tauri::AppHandle`], or [`tauri::WebviewWindow`].
///
/// Registered by [`init`] or [`Builder::build`].
pub trait KeyringExt<R: Runtime> {
    /// Returns the shared plugin state (store + session registry).
    fn keyring(&self) -> &KeyringPlugin;
}

impl<R: Runtime, T: Manager<R>> KeyringExt<R> for T {
    fn keyring(&self) -> &KeyringPlugin {
        self.state::<KeyringPlugin>().inner()
    }
}

/// Configure [`KeyringPlugin`] before mounting (service name defaults to the app identifier).
///
/// # Example
///
/// ```rust,no_run
/// use tauri_plugin_keyring_store::Builder;
///
/// fn plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
///     Builder::new()
///         .service("com.example.myapp.keyring")
///         .build()
/// }
/// ```
#[derive(Default)]
pub struct Builder {
    service: Option<String>,
    #[cfg(target_os = "ios")]
    ios_write_accessibility: Option<WriteAccessibility>,
}

impl Builder {
    /// Same as [`Default::default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the keychain/credential-manager **service** string (Stronghold has no direct equivalent).
    pub fn service(mut self, service: impl Into<String>) -> Self {
        self.service = Some(service.into());
        self
    }

    /// iOS Data Protection policy for new credential writes (default: [`WriteAccessibility::AfterFirstUnlock`]).
    #[cfg(target_os = "ios")]
    pub fn ios_write_accessibility(mut self, policy: WriteAccessibility) -> Self {
        self.ios_write_accessibility = Some(policy);
        self
    }

    /// Build the plugin for registration via [`tauri::Builder::plugin`].
    pub fn build<R: Runtime>(self) -> TauriPlugin<R> {
        let service_hint = self.service;
        #[cfg(target_os = "ios")]
        let ios_write_accessibility = self.ios_write_accessibility;
        PluginBuilder::new("keyring-store")
            .setup(move |app, _api| {
                let service = service_hint.unwrap_or_else(|| app.config().identifier.clone());
                #[cfg(target_os = "ios")]
                {
                    let mut store = KeyringStore::new(service);
                    if let Some(policy) = ios_write_accessibility {
                        store = store.with_write_accessibility(policy);
                    }
                    app.manage(KeyringPlugin::from_store(store));
                }
                #[cfg(not(target_os = "ios"))]
                app.manage(KeyringPlugin::new(service));
                Ok(())
            })
            .invoke_handler(tauri::generate_handler![
                commands::get_passwords,
                commands::set_passwords,
                commands::delete_passwords,
                commands::password_exists,
                commands::export_passwords_plain,
                commands::import_passwords_plain,
                commands::export_passwords_encrypted,
                commands::import_passwords_encrypted,
                commands::ping,
                commands::initialize,
                commands::destroy,
                commands::save,
                commands::create_client,
                commands::load_client,
                commands::get_store_record,
                commands::save_store_record,
                commands::remove_store_record,
                commands::save_secret,
                commands::remove_secret,
                commands::execute_procedure,
            ])
            .build()
    }
}

/// Registers the plugin with default options (`service` = bundle identifier).
///
/// Shorthand for `Builder::default().build()`. Use [`Builder`] to set a custom service name.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::default().build()
}
