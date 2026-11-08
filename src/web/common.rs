//! Plumbing shared by every page: errors, background work, the page layout.

use anyhow::anyhow;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{DOCTYPE, Markup, html};

use super::AppState;
use crate::settings::Settings;

pub(super) struct AppError(pub(super) anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = html! {
            div.notice.error {
                strong { "Error: " } (format!("{:#}", self.0))
            }
        };
        (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
    }
}

pub(super) type Page<T = Markup> = Result<T, AppError>;

/// Runs filesystem-heavy work off the async runtime.
pub(super) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, AppError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow!("background task failed: {e}"))?
        .map_err(AppError)
}

pub(super) fn busy_guard(state: &AppState) -> Result<tokio::sync::MutexGuard<'_, ()>, AppError> {
    state.busy.try_lock().map_err(|_| {
        AppError(anyhow!(
            "another operation is running; try again when it finishes"
        ))
    })
}

pub(super) async fn htmx_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../../assets/htmx.min.js"),
    )
}

pub(super) async fn style_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../../assets/style.css"),
    )
}

const NAV: &[(&str, &str)] = &[
    ("/", "Overview"),
    ("/library", "Library"),
    ("/emulators", "Emulators"),
    ("/machines", "Machines"),
    ("/saves", "Saves"),
    ("/settings", "Settings"),
];

pub(super) fn layout(title: &str, body: Markup) -> Markup {
    shell(title, true, body)
}

pub(super) fn shell(title: &str, with_nav: bool, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                // Swap error responses too, so failures show up where the result would have.
                meta name="htmx-config" content=r#"{"responseHandling":[{"code":"204","swap":false},{"code":".*","swap":true}]}"#;
                title { "RomServer · " (title) }
                link rel="stylesheet" href="/assets/style.css";
                script src="/assets/htmx.min.js" {}
            }
            body {
                nav {
                    span.brand { "RomServer" }
                    @if with_nav {
                        @for (href, label) in NAV {
                            a href=(href) class=[(*label == title).then_some("active")] { (label) }
                        }
                        form.push method="post" action="/logout" { button.link { "Log out" } }
                    }
                }
                main {
                    @if with_nav {
                        // The save monitor's banner, filled in after the page loads.
                        div hx-get="/saves/banner" hx-trigger="load" hx-swap="innerHTML" {}
                    }
                    h1 { (title) }
                    (body)
                }
            }
        }
    }
}

/// Full page that shows a settings error instead of failing outright, so a broken file can
/// still be diagnosed from the UI.
pub(super) fn settings_or_notice(state: &AppState, title: &str) -> Result<Settings, Markup> {
    state.settings().map_err(|e| {
        layout(
            title,
            html! {
                div.notice.error {
                    p { strong { "Settings can't be loaded." } }
                    p { (format!("{e:#}")) }
                    p.muted { "File: " code { (state.config.display()) } }
                }
            },
        )
    })
}

pub(super) fn spinner(id: &str) -> Markup {
    html! { span.spinner.htmx-indicator id=(id) { "Working…" } }
}
