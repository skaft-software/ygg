//! Native Tauri lifecycle and app-owned networking for the experimental Ygg companion.

mod client;
mod core;
mod credentials;
mod profile;
mod proxy;

use std::sync::{Arc, Mutex};

use core::NativeCore;
use credentials::{KeyringCredentials, SharedCredentials};
use profile::ProfileStore;
use proxy::{AssetBundle, ProxyHandle};
use tauri::{Manager, WebviewUrl};

struct RuntimeResources {
    core: Arc<NativeCore>,
    proxy: Mutex<Option<ProxyHandle>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
/// Starts the native companion application and its isolated loopback services.
pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            #[cfg(target_os = "android")]
            credentials::initialize_android_keyring_context()?;
            let app_data = app.path().app_data_dir()?.join("companion");
            let credentials: SharedCredentials = Arc::new(KeyringCredentials::new());
            let core = tauri::async_runtime::block_on(NativeCore::load(
                credentials,
                ProfileStore::open(app_data)?,
            ))?;
            let resolver = app.asset_resolver();
            let assets = AssetBundle::verified(|path| {
                resolver.get(path.to_owned()).map(|asset| asset.bytes)
            })?;
            let proxy = tauri::async_runtime::block_on(ProxyHandle::start(core.clone(), assets))?;
            let launch_url: tauri::Url = proxy.launch_url().parse()?;
            let allowed_port = proxy.address().port();
            let settings_port = proxy.settings_address().port();

            tauri::WebviewWindowBuilder::new(app, "main", WebviewUrl::External(launch_url))
                .on_navigation(move |url| {
                    url.scheme() == "http"
                        && url.host_str() == Some("127.0.0.1")
                        && url
                            .port()
                            .is_some_and(|port| port == allowed_port || port == settings_port)
                        && url.username().is_empty()
                        && url.password().is_none()
                })
                .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
                .build()?;

            app.manage(RuntimeResources {
                core,
                proxy: Mutex::new(Some(proxy)),
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to initialize Ygg Companion");

    app.run(|app_handle, event| match event {
        tauri::RunEvent::Resumed => {
            if let Some(resources) = app_handle.try_state::<RuntimeResources>() {
                let core = resources.core.clone();
                tauri::async_runtime::spawn(async move {
                    core.remote().invalidate_connection().await;
                });
            }
        }
        tauri::RunEvent::Exit => {
            if let Some(resources) = app_handle.try_state::<RuntimeResources>() {
                let proxy = resources
                    .proxy
                    .lock()
                    .ok()
                    .and_then(|mut proxy| proxy.take());
                let core = resources.core.clone();
                tauri::async_runtime::block_on(async move {
                    if let Some(proxy) = proxy {
                        proxy.shutdown().await;
                    }
                    core.remote().close().await;
                });
            }
        }
        _ => {}
    });
}
