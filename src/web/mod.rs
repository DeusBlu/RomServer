//! Web UI: server-rendered HTML with htmx for the interactive parts.
//!
//! Settings are re-read from disk on every request, so the TOML file stays the single source
//! of truth. Anything that changes files (applying a view, fixing cues) is preview → confirm →
//! apply, and the apply refuses if the library changed since the preview.

mod common;
mod emulators;
mod library;
mod login;
mod machines;
mod overview;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use axum::Router;
use axum::middleware;
use axum::routing::{get, post};
use tokio::sync::Mutex;

use crate::auth::Auth;
use crate::catalog::Catalog;
use crate::settings::Settings;

#[derive(Clone)]
struct AppState {
    config: Arc<PathBuf>,
    /// One file-changing operation at a time.
    busy: Arc<Mutex<()>>,
    auth: Arc<Auth>,
    /// Whether this process has loaded the bundled emulator catalog into the database yet.
    catalog_loaded: Arc<AtomicBool>,
}

impl AppState {
    fn settings(&self) -> anyhow::Result<Settings> {
        Settings::load(&self.config)
    }

    /// Opens the database, refreshing the emulator catalog from the bundled copy once per run.
    fn db(&self, settings: &Settings) -> anyhow::Result<Catalog> {
        let mut db = Catalog::open(&settings.database)?;
        if !self.catalog_loaded.load(Ordering::Acquire) {
            db.load_emulator_catalog(&crate::emulators::bundled()?)?;
            self.catalog_loaded.store(true, Ordering::Release);
        }
        Ok(db)
    }
}

pub async fn serve(config: PathBuf, listen: &str, password_file: PathBuf) -> anyhow::Result<()> {
    let state = AppState {
        config: Arc::new(config),
        busy: Arc::new(Mutex::new(())),
        auth: Arc::new(Auth::new(password_file)),
        catalog_loaded: Arc::new(AtomicBool::new(false)),
    };
    if !state.auth.is_password_set() {
        println!(
            "[auth] no password set: RomServer is in setup mode. Open the UI from your LAN \
             and set one now ({}).",
            state.auth.password_file().display()
        );
    }
    let protected = Router::new()
        .route("/", get(overview::dashboard))
        .route("/library", get(library::library_page))
        .route("/library/scan", post(library::library_scan))
        .route("/library/cues", post(library::cues_check))
        .route("/library/cues/fix", post(library::cues_fix))
        .route(
            "/emulators",
            get(emulators::emulators_page).post(emulators::emulators_save),
        )
        .route("/emulators/custom", post(emulators::custom_emulator_add))
        .route(
            "/emulators/custom/{id}/delete",
            post(emulators::custom_emulator_delete),
        )
        .route(
            "/machines",
            get(machines::machines_page).post(machines::machine_add),
        )
        .route(
            "/machines/{name}",
            get(machines::machine_page).post(machines::machine_save),
        )
        .route("/machines/{name}/plan", post(machines::machine_plan))
        .route("/machines/{name}/apply", post(machines::machine_apply))
        .route("/machines/{name}/delete", post(machines::machine_delete))
        .route("/logout", post(login::logout))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            login::require_auth,
        ));
    let app = Router::new()
        .route("/login", get(login::login_page).post(login::login))
        .route("/setup", get(login::setup_page).post(login::setup))
        .route("/assets/htmx.min.js", get(common::htmx_js))
        .route("/assets/style.css", get(common::style_css))
        .merge(protected)
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listen on {listen}"))?;
    println!("RomServer UI on http://{listen}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
