//! Login (same scheme as Lantern; see auth.rs).

use std::net::SocketAddr;

use anyhow::anyhow;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::Form;
use maud::{Markup, html};
use serde::Deserialize;

use super::AppState;
use super::common::{AppError, shell};
use crate::auth;

fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(auth::token_from_cookies)
}

pub(super) async fn require_auth(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if session_token(req.headers()).is_some_and(|t| state.auth.validate_session(t)) {
        return next.run(req).await;
    }
    let to = if state.auth.is_password_set() {
        "/login"
    } else {
        "/setup"
    };
    if req.headers().contains_key("hx-request") {
        // htmx follows this with a full page load instead of swapping a login form into a div.
        return (StatusCode::UNAUTHORIZED, [("HX-Redirect", to)]).into_response();
    }
    Redirect::to(to).into_response()
}

fn logged_in(token: String) -> Response {
    (
        [(header::SET_COOKIE, auth::session_cookie(&token))],
        Redirect::to("/"),
    )
        .into_response()
}

fn login_form(error: Option<&str>) -> Markup {
    shell(
        "Log in",
        false,
        html! {
            @if let Some(e) = error { div.notice.error { (e) } }
            form.stack method="post" action="/login" {
                label { "Password " input type="password" name="password" required autofocus; }
                button { "Log in" }
            }
        },
    )
}

pub(super) async fn login_page(State(state): State<AppState>) -> Response {
    if !state.auth.is_password_set() {
        return Redirect::to("/setup").into_response();
    }
    login_form(None).into_response()
}

#[derive(Deserialize)]
pub(super) struct LoginForm {
    password: String,
}

pub(super) async fn login(State(state): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    let auth = state.auth.clone();
    let ok = tokio::task::spawn_blocking(move || auth.verify_password(&form.password))
        .await
        .unwrap_or(false);
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            login_form(Some("Wrong password.")),
        )
            .into_response();
    }
    match state.auth.create_session() {
        Ok(token) => logged_in(token),
        Err(e) => AppError(e).into_response(),
    }
}

fn setup_form(error: Option<&str>) -> Markup {
    shell(
        "Set a password",
        false,
        html! {
            p.muted {
                "RomServer has no password yet. Set one now; it protects every page. "
                "To reset it later, delete " code { "password.lock" } " and restart."
            }
            @if let Some(e) = error { div.notice.error { (e) } }
            form.stack method="post" action="/setup" {
                label { "Password " input type="password" name="password" required autofocus
                    minlength=(auth::MIN_PASSWORD_LEN); }
                label { "Again " input type="password" name="confirm" required; }
                button { "Set password" }
            }
        },
    )
}

pub(super) async fn setup_page(State(state): State<AppState>) -> Response {
    if state.auth.is_password_set() {
        return Redirect::to("/login").into_response();
    }
    setup_form(None).into_response()
}

#[derive(Deserialize)]
pub(super) struct SetupForm {
    password: String,
    confirm: String,
}

pub(super) async fn setup(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Form(form): Form<SetupForm>,
) -> Response {
    if !auth::is_lan(peer.ip()) {
        println!(
            "[auth] setup attempt blocked from non-LAN address {}",
            peer.ip()
        );
        return (
            StatusCode::FORBIDDEN,
            setup_form(Some(
                "Password setup is only allowed from the local network.",
            )),
        )
            .into_response();
    }
    if form.password != form.confirm {
        return (
            StatusCode::BAD_REQUEST,
            setup_form(Some("The passwords don't match.")),
        )
            .into_response();
    }
    let auth = state.auth.clone();
    let result = tokio::task::spawn_blocking(move || {
        auth.set_password(&form.password)?;
        auth.create_session()
    })
    .await
    .map_err(|e| anyhow!("{e}"))
    .and_then(|r| r);
    match result {
        Ok(token) => logged_in(token),
        Err(e) => (StatusCode::BAD_REQUEST, setup_form(Some(&format!("{e:#}")))).into_response(),
    }
}

pub(super) async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = session_token(&headers) {
        state.auth.destroy_session(token);
    }
    (
        [(header::SET_COOKIE, auth::clear_cookie())],
        Redirect::to("/login"),
    )
        .into_response()
}
