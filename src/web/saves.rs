//! Saves page: the save monitor's alerts (adopt or dismiss), and the "my save didn't carry
//! over" check. A banner on every page points here while saves are stuck in views.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::anyhow;
use axum::extract::{Query, State};
use axum::response::Redirect;
use axum_extra::extract::Form;
use maud::{Markup, html};
use serde::Deserialize;

use super::AppState;
use super::common::{Page, blocking, busy_guard, layout, settings_or_notice};
use super::emulators::{names, offered};
use crate::catalog::{Alert, AlertKind, Catalog};
use crate::monitor::{self, CheckReport};
use crate::ops;

/// How long machines' save folders must be quiet before a pass runs: an emulator may write a
/// save in several pieces.
const SETTLE: Duration = Duration::from_secs(2);

/// A full pass this often, for anything the watcher misses: writes that bypass `/mnt/user`
/// (an Unraid share set to exclusive access), or an overflowing event queue.
const INTERVAL: Duration = Duration::from_secs(600);

/// Runs the monitor in the background for as long as the server runs. It reacts to changes in
/// machines' save folders (`views/<node>/machine/`): Samba on Unraid writes through the same
/// `/mnt/user` mount the container sees, so the kernel reports a save the moment a machine
/// writes it (checked 2026-11-08: create, write, close and delete over SMB all arrived). A pass
/// waits for any file-changing operation to finish first (an apply creates files before
/// recording them).
pub(super) fn spawn_monitor(state: AppState) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let relevant = match event {
            Ok(e) => !matches!(e.kind, notify::EventKind::Access(_)),
            Err(_) => true, // a dropped event: run a pass to be sure
        };
        if relevant {
            let _ = tx.send(());
        }
    });
    let mut watcher = match watcher {
        Ok(w) => Some(w),
        Err(e) => {
            println!("[monitor] can't watch for changes, checking every few minutes only: {e}");
            None
        }
    };
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(INTERVAL);
        let mut watched: BTreeSet<PathBuf> = BTreeSet::new();
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                Some(()) = rx.recv() => {
                    // Wait until the writes have settled.
                    while let Ok(Some(())) = tokio::time::timeout(SETTLE, rx.recv()).await {}
                }
            }
            let guard = state.busy.clone().lock_owned().await;
            let st = state.clone();
            let result = tokio::task::spawn_blocking(move || {
                let _guard = guard;
                let settings = st.settings()?;
                let mut db = Catalog::open(&settings.database)?;
                monitor::refresh(&settings, &mut db)?;
                Ok::<_, anyhow::Error>(watch_roots(&settings))
            })
            .await;
            match result {
                Ok(Ok(roots)) => {
                    if let Some(w) = watcher.as_mut() {
                        update_watches(w, &mut watched, roots);
                    }
                }
                Ok(Err(e)) => println!("[monitor] {e:#}"),
                Err(e) => println!("[monitor] {e}"),
            }
        }
    });
}

/// The folders the watcher follows: each machine's save and state projections.
fn watch_roots(settings: &crate::settings::Settings) -> BTreeSet<PathBuf> {
    settings
        .nodes
        .keys()
        .map(|n| settings.view_root(n).join(crate::saves::VIEW_MACHINE))
        .filter(|p| p.is_dir())
        .collect()
}

/// Follows new machines' folders and stops following removed ones.
fn update_watches(
    watcher: &mut notify::RecommendedWatcher,
    watched: &mut BTreeSet<PathBuf>,
    roots: BTreeSet<PathBuf>,
) {
    use notify::Watcher as _;
    for gone in watched.difference(&roots) {
        let _ = watcher.unwatch(gone);
    }
    for new in roots.difference(watched) {
        if let Err(e) = watcher.watch(new, notify::RecursiveMode::Recursive) {
            println!("[monitor] can't watch {}: {e}", new.display());
        }
    }
    watched.retain(|p| roots.contains(p));
    watched.extend(roots);
}

fn when(unix: i64) -> String {
    let age = monitor::now() - unix;
    match age {
        ..60 => "just now".into(),
        60..3600 => format!("{} min ago", age / 60),
        3600..86400 => format!("{} h ago", age / 3600),
        _ => format!("{} days ago", age / 86400),
    }
}

/// Loaded by every page (hx-trigger="load"); empty when there is nothing to say.
pub(super) async fn banner(State(state): State<AppState>) -> Page {
    let Ok(settings) = state.settings() else {
        return Ok(html! {});
    };
    let alerts = blocking(move || Catalog::open(&settings.database)?.alerts()).await?;
    let saves = alerts
        .iter()
        .filter(|a| a.kind == AlertKind::Save && !a.dismissed)
        .count();
    let machines: std::collections::BTreeSet<&str> = alerts
        .iter()
        .filter(|a| a.kind == AlertKind::Save && !a.dismissed)
        .map(|a| a.node.as_str())
        .collect();
    Ok(html! {
        @if saves > 0 {
            div.notice.warn {
                strong { (saves) " save file(s) " }
                "were written into the views of " (machines.into_iter().collect::<Vec<_>>().join(", "))
                ", so they won't reach other machines. "
                a href="/saves" { "Review" }
            }
        }
    })
}

fn alert_rows(alerts: &[(Alert, Vec<String>)]) -> Markup {
    html! {
        table.grid {
            tr { th { "Machine" } th { "File" } th { "Seen" } th {} }
            @for (a, folders) in alerts {
                tr {
                    td { (a.node) }
                    td { code.small { (a.path.display()) } }
                    td.muted { (when(a.first_seen)) }
                    td {
                        div.row {
                            @if a.kind == AlertKind::Save {
                                @if folders.is_empty() {
                                    span.muted.small { "no save folder for this system" }
                                } @else {
                                    form method="post" action="/saves/adopt" {
                                        input type="hidden" name="node" value=(a.node);
                                        input type="hidden" name="path" value=(a.path.display());
                                        @if folders.len() == 1 {
                                            input type="hidden" name="folder" value=(folders[0]);
                                            button { "Move to " (folders[0]) }
                                        } @else {
                                            select name="folder" { @for f in folders { option value=(f) { (f) } } }
                                            " " button { "Move" }
                                        }
                                    }
                                }
                            }
                            form method="post" action="/saves/dismiss" {
                                input type="hidden" name="node" value=(a.node);
                                input type="hidden" name="path" value=(a.path.display());
                                button.link { "dismiss" }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Deserialize)]
pub(super) struct CheckQuery {
    #[serde(default)]
    machine: Option<String>,
    #[serde(default)]
    game: Option<String>,
}

pub(super) async fn saves_page(
    State(state): State<AppState>,
    Query(query): Query<CheckQuery>,
) -> Page {
    let settings = match settings_or_notice(&state, "Saves") {
        Ok(s) => s,
        Err(page) => return Ok(page),
    };
    let machine = query
        .machine
        .clone()
        .filter(|m| settings.nodes.contains_key(m))
        .or_else(|| settings.nodes.keys().next().cloned());
    let game = query
        .game
        .clone()
        .map(|g| g.trim().to_string())
        .filter(|g| !g.is_empty());
    let (alerts, entry_names, report) = {
        let st = state.clone();
        let machine = machine.clone();
        let game = game.clone();
        blocking(move || {
            let db = st.db(&settings)?;
            let offered = offered(&db, &settings)?;
            let index = names(&offered);
            let alerts: Vec<(Alert, Vec<String>)> = db
                .alerts()?
                .into_iter()
                .filter(|a| !a.dismissed)
                .map(|a| {
                    let folders = monitor::adopt_candidates(&settings, &a.node, &a.path, &index);
                    (a, folders)
                })
                .collect();
            let entry_names = match &machine {
                Some(m) => ops::node_entry_names(&settings, m).unwrap_or_default(),
                None => Vec::new(),
            };
            let report = match (&machine, &game) {
                (Some(m), Some(g)) => Some(monitor::check(&settings, &db, m, g)?),
                _ => None,
            };
            Ok((alerts, entry_names, report))
        })
        .await?
    };
    let settings = state.settings()?;
    let (saves, others): (Vec<_>, Vec<_>) = alerts
        .into_iter()
        .partition(|(a, _)| a.kind == AlertKind::Save);
    Ok(layout(
        "Saves",
        html! {
            p.muted {
                "RomServer checks every machine's view about once a minute for files it didn't put there. "
                "A save beside a ROM means that machine's RetroArch has "
                strong { "Write Saves to Content Directory" } " on (or no save folder set), so the save "
                "stays in that machine's view. Moving it puts it in the shared save folder; then fix the "
                "setting on the machine so the next save goes there too."
            }
            form method="post" action="/saves/refresh" { button.secondary { "Check views now" } }

            h2 { "Saves written into views" }
            @if saves.is_empty() {
                div.notice.ok { "None. Every save is going where it should." }
            } @else {
                (alert_rows(&saves))
            }

            @if !others.is_empty() {
                details {
                    summary { "Other files in views (" (others.len()) ")" }
                    p.muted.small {
                        "Scraped media, gamelists or files copied in by hand. RomServer never deletes "
                        "them; remove them on the machine, or dismiss them here."
                    }
                    (alert_rows(&others))
                }
            }

            h2 { "My save didn't carry over" }
            @if settings.nodes.is_empty() {
                p.muted { "Add a machine first." }
            } @else {
                form.stack method="get" action="/saves" {
                    label { "Machine that should see the save "
                        select name="machine" {
                            @for name in settings.nodes.keys() {
                                option value=(name) selected[machine.as_deref() == Some(name.as_str())] { (name) }
                            }
                        }
                    }
                    label { "Game "
                        input name="game" list="games" size="40" required value=[game.as_deref()]
                            placeholder="as the frontend lists it";
                    }
                    datalist #games { @for n in &entry_names { option value=(n); } }
                    button { "Look for the save" }
                }
                @if let Some(r) = &report { (render_report(r)) }
            }
        },
    ))
}

fn render_report(r: &CheckReport) -> Markup {
    let other: Vec<&(String, PathBuf)> = r
        .in_save_folders
        .iter()
        .filter(|(f, _)| !r.expected_folders.contains(f))
        .collect();
    html! {
        h3 { "Saves named “" (r.stem) "”" }
        @if r.found_expected() {
            div.notice.ok {
                "It reached the server, in a save folder this machine uses:"
                ul { @for (f, p) in r.in_save_folders.iter().filter(|(f, _)| r.expected_folders.contains(f)) {
                    li { code { (f) "/" (p.display()) } }
                } }
                "If the game still doesn't see it, the machine may have launched it with a different "
                "core, or the emulator only reads saves when the game starts; restart the game."
            }
        }
        @if !r.in_views.is_empty() {
            div.notice.warn {
                "It's in a machine's view, not a shared save folder:"
                ul { @for (n, p) in &r.in_views { li { (n) ": " code { (p.display()) } } } }
                "Move it from the list above, and turn off Write Saves to Content Directory on that machine."
            }
        }
        @if !other.is_empty() {
            div.notice.warn {
                "It's in a save folder this machine doesn't use, so it was saved with a different core or emulator:"
                ul { @for (f, p) in &other { li { code { (f) "/" (p.display()) } } } }
                "Launch the game with the same core everywhere, or give both emulators the same save folder on the Emulators page."
            }
        }
        @if r.in_save_folders.is_empty() && r.in_views.is_empty() {
            div.notice.error {
                p { "Not found anywhere on the server. Most likely first:" }
                ol {
                    li { "RetroArch's " strong { "Sort Saves into Folders by Core Name" }
                         " doesn't match this machine's setting, so the save is in the root of the local saves folder (or in a core folder that isn't mounted)." }
                    li { "The save mount was down when the game saved. Check " code { "systemctl status" } " for the machine's RomServer automounts." }
                    li { "That core isn't ticked for this machine, so its save folder isn't mounted." }
                    li { "The machine's platform is wrong (Flatpak and native emulators keep saves in different places)." }
                    li { "The game was saved under another name (a single disc instead of its .m3u)." }
                }
            }
        }
    }
}

pub(super) async fn refresh_now(State(state): State<AppState>) -> Page<Redirect> {
    let _guard = busy_guard(&state)?;
    let settings = state.settings()?;
    blocking(move || {
        let mut db = Catalog::open(&settings.database)?;
        monitor::refresh(&settings, &mut db)
    })
    .await?;
    Ok(Redirect::to("/saves"))
}

#[derive(Deserialize)]
pub(super) struct AlertForm {
    node: String,
    path: String,
    #[serde(default)]
    folder: Option<String>,
}

pub(super) async fn adopt(
    State(state): State<AppState>,
    Form(form): Form<AlertForm>,
) -> Page<Redirect> {
    let _guard = busy_guard(&state)?;
    let settings = state.settings()?;
    let folder = form
        .folder
        .clone()
        .ok_or_else(|| anyhow!("pick a save folder"))?;
    blocking(move || {
        let db = Catalog::open(&settings.database)?;
        let path = PathBuf::from(&form.path);
        // Only files the monitor reported as saves: never anything RomServer linked.
        let known = db
            .alerts()?
            .into_iter()
            .any(|a| a.node == form.node && a.path == path && a.kind == AlertKind::Save);
        if !known {
            anyhow::bail!("that file isn't a reported save any more; check the views again");
        }
        monitor::adopt(&settings, &form.node, &path, &folder)?;
        db.forget_alert(&form.node, &path)
    })
    .await?;
    Ok(Redirect::to("/saves"))
}

pub(super) async fn dismiss(
    State(state): State<AppState>,
    Form(form): Form<AlertForm>,
) -> Page<Redirect> {
    let settings = state.settings()?;
    blocking(move || {
        Catalog::open(&settings.database)?.dismiss_alert(&form.node, &PathBuf::from(&form.path))
    })
    .await?;
    Ok(Redirect::to("/saves"))
}
