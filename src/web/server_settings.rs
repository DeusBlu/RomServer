//! Settings page: library paths, how clients reach the server, and what is shared.

use std::os::unix::fs::MetadataExt;
use std::path::{Path as FsPath, PathBuf};

use anyhow::{anyhow, bail};
use axum::extract::{Path, State};
use axum::response::Redirect;
use axum_extra::extract::Form;
use maud::{Markup, html};
use serde::Deserialize;

use super::AppState;
use super::common::{Page, layout, settings_or_notice};
use crate::settings::{Protocol, Settings, Share};
use crate::shares::{self, Source, UnraidStatus};

/// Device of `path`, or of its nearest existing parent (a views folder may not exist yet).
fn device_of(path: &FsPath) -> Option<u64> {
    path.ancestors()
        .find_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.dev())
}

/// Hard links only work within one filesystem, so every path RomServer links between must
/// share the library's.
fn same_filesystem_warnings(settings: &Settings) -> Vec<String> {
    let library = device_of(&settings.library_root);
    let mut out = Vec::new();
    if library.is_none() {
        out.push(format!(
            "{} can't be read: is the share mounted into the container?",
            settings.library_root.display()
        ));
        return out;
    }
    if device_of(&settings.views_root) != library {
        out.push(format!(
            "{} is on a different filesystem (or container mount) than the library, so views \
             can't be hard-linked. Mount one common parent folder instead.",
            settings.views_root.display()
        ));
    }
    out
}

/// One row of "what clients mount": the server path and its remote, or why it can't be mounted.
fn mount_rows(settings: &Settings) -> Vec<(String, PathBuf, Result<String, String>)> {
    let (shares, _) = shares::effective(settings);
    let remote = |path: &FsPath| {
        shares::remote_for(
            path,
            &shares,
            &settings.network.server,
            settings.network.protocol,
        )
        .map(|r| r.what)
        .map_err(|e| match shares::sharing_advice(settings, &shares, path) {
            Some(advice) => format!("{e:#}. {advice}"),
            None => format!("{e:#}"),
        })
    };
    let mut rows = Vec::new();
    for name in settings.nodes.keys() {
        let path = settings.view_root(name);
        rows.push((format!("{name}'s view"), path.clone(), remote(&path)));
    }
    let saves = settings.saves_root();
    rows.push(("Save folders".to_string(), saves.clone(), remote(&saves)));
    rows
}

fn protocol_select(current: Protocol) -> Markup {
    html! {
        select name="protocol" {
            @for p in Protocol::ALL {
                option value=(p.id()) selected[*p == current] { (p.label()) }
            }
        }
    }
}

fn yes_no(value: bool) -> Markup {
    html! { @if value { span.tag.count { "yes" } } @else { span.muted { "no" } } }
}

pub(super) async fn settings_page(State(state): State<AppState>) -> Page {
    let settings = match settings_or_notice(&state, "Settings") {
        Ok(s) => s,
        Err(page) => return Ok(page),
    };
    let (shares, unraid) = shares::effective(&settings);
    let warnings = same_filesystem_warnings(&settings);
    let rows = mount_rows(&settings);
    let network = &settings.network;
    Ok(layout(
        "Settings",
        html! {
            h2 { "Paths" }
            p.muted {
                "Server paths as the container sees them. Mount the host's " code { "/mnt/user" }
                " at the same path so these match what Unraid shares."
            }
            @for w in &warnings { div.notice.error { (w) } }
            form.stack method="post" action="/settings/paths" {
                label { "Library " input name="library_root" required size="40"
                    value=(settings.library_root.display()); }
                label { "Views " input name="views_root" required size="40"
                    value=(settings.views_root.display()); }
                label { "Save folders " input name="saves_root" size="40"
                    placeholder=(settings.library_root.join("saves").display())
                    value=[settings.saves_root.as_ref().map(|p| p.display().to_string())]; }
                label { "Scraped media " input name="media_root" size="40"
                    placeholder=(settings.library_root.join("media").display())
                    value=[settings.media_root.as_ref().map(|p| p.display().to_string())]; }
                p.muted.small { "Leave these empty to use " code { "saves" } " and " code { "media" }
                    " inside the library. Media is kept frontend-neutral, one folder per game ("
                    code { "<system>/<game>/box-front.png" } "); each machine gets it in its own "
                    "frontend's layout. The Library page converts media scraped by ES-DE." }
                button { "Save paths" }
            }

            h2 { "Network" }
            form.stack method="post" action="/settings/network" {
                label { "Server address " input name="server" size="30" placeholder="192.168.1.10"
                    value=(network.server); }
                label { "Clients mount with " (protocol_select(network.protocol)) }
                label { "Unraid share settings " input name="unraid_share_config" size="30"
                    placeholder="leave empty to set shares by hand"
                    value=[network.unraid_share_config.as_ref().map(|p| p.display().to_string())]; }
                p.muted.small {
                    "Mount Unraid's " code { "/boot/config/shares" } " read-only at this path (e.g. "
                    code { "-v /boot/config/shares:/unraid/shares:ro" } ") and RomServer reads which "
                    "shares are exported over SMB and NFS."
                }
                button { "Save network" }
            }

            h2 { "Shares" }
            @match &unraid {
                UnraidStatus::NotConfigured => {
                    p.muted { "Reading Unraid's share settings is off; only shares set below count." }
                }
                UnraidStatus::NotMounted(path) => {
                    p.muted { "Unraid's share settings aren't mounted at " code { (path.display()) }
                        "; only shares set below count." }
                }
                UnraidStatus::Unreadable { path, error } => {
                    div.notice.error {
                        "Unraid's share settings are mounted at " code { (path.display()) }
                        " but RomServer can't read them (" (error) "). They're readable by root "
                        "only: run the container as root (its default). Until then only shares set "
                        "below count."
                    }
                }
                UnraidStatus::Read { path, shares } => {
                    p.muted { "Read " (shares) " shares from " code { (path.display()) } "." }
                }
            }
            @if shares.is_empty() {
                p.muted { "No shares known yet." }
            } @else {
                table.grid {
                    tr { th { "Share" } th { "Path" } th { "SMB" } th { "NFS" } th { "From" } th {} }
                    @for (name, info) in &shares {
                        tr {
                            td { strong { (name) } }
                            td { code { (info.share.path.display()) } }
                            td { (yes_no(info.share.smb)) }
                            td { (yes_no(info.share.nfs)) }
                            td { @match info.source { Source::Unraid => "Unraid", Source::Manual => "set here" } }
                            td {
                                @if info.source == Source::Manual {
                                    form method="post" action={"/settings/shares/" (name) "/delete"} {
                                        button.link { "remove" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            details {
                summary { "Set a share by hand" }
                p.muted.small {
                    "For servers that aren't Unraid, or to override what Unraid reports. "
                    "A share set here replaces an Unraid share with the same name."
                }
                form.stack method="post" action="/settings/shares" {
                    label { "Share name " input name="name" required placeholder="views"; }
                    label { "Server path " input name="path" required size="40" placeholder="/mnt/user/views"; }
                    label.check { input type="checkbox" name="smb" value="1" checked; "Exported over SMB" }
                    label.check { input type="checkbox" name="nfs" value="1"; "Exported over NFS" }
                    button { "Save share" }
                }
            }

            h2 { "What clients mount" }
            p.muted { "Mount units for each machine are generated from this." }
            table.grid {
                tr { th { "For" } th { "Server path" } th { "Remote" } }
                @for (label, path, remote) in &rows {
                    tr {
                        td { (label) }
                        td { code { (path.display()) } }
                        @match remote {
                            Ok(what) => { td.ok { code { (what) } } }
                            Err(why) => { td.warn { (why) } }
                        }
                    }
                }
            }
        },
    ))
}

fn absolute(value: &str, what: &str) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(value.trim());
    if !path.is_absolute() {
        bail!("{what} must be an absolute path");
    }
    Ok(path)
}

#[derive(Deserialize)]
pub(super) struct PathsForm {
    library_root: String,
    views_root: String,
    #[serde(default)]
    saves_root: String,
    #[serde(default)]
    media_root: String,
}

pub(super) async fn paths_save(
    State(state): State<AppState>,
    Form(form): Form<PathsForm>,
) -> Page<Redirect> {
    let mut settings = state.settings()?;
    let library_root = absolute(&form.library_root, "the library")?;
    let views_root = absolute(&form.views_root, "the views folder")?;
    if views_root.starts_with(&library_root) {
        return Err(
            anyhow!("views must not live inside the library (it is source data only)").into(),
        );
    }
    settings.library_root = library_root;
    settings.views_root = views_root;
    settings.saves_root = match form.saves_root.trim() {
        "" => None,
        other => Some(absolute(other, "the save folder")?),
    };
    settings.media_root = match form.media_root.trim() {
        "" => None,
        other => Some(absolute(other, "the media folder")?),
    };
    settings.save(&state.config)?;
    Ok(Redirect::to("/settings"))
}

#[derive(Deserialize)]
pub(super) struct NetworkForm {
    server: String,
    protocol: String,
    #[serde(default)]
    unraid_share_config: String,
}

pub(super) async fn network_save(
    State(state): State<AppState>,
    Form(form): Form<NetworkForm>,
) -> Page<Redirect> {
    let mut settings = state.settings()?;
    let server = form.server.trim();
    if server.contains(['/', ' ', ':']) && !server.starts_with('[') {
        return Err(
            anyhow!("server address is just a host name or IP, without // or a path").into(),
        );
    }
    settings.network.server = server.to_string();
    settings.network.protocol = Protocol::from_id(&form.protocol)
        .ok_or_else(|| anyhow!("unknown protocol {}", form.protocol))?;
    settings.network.unraid_share_config = match form.unraid_share_config.trim() {
        "" => None,
        other => Some(absolute(other, "Unraid's share settings")?),
    };
    settings.save(&state.config)?;
    Ok(Redirect::to("/settings"))
}

#[derive(Deserialize)]
pub(super) struct ShareForm {
    name: String,
    path: String,
    #[serde(default)]
    smb: Option<String>,
    #[serde(default)]
    nfs: Option<String>,
}

pub(super) async fn share_save(
    State(state): State<AppState>,
    Form(form): Form<ShareForm>,
) -> Page<Redirect> {
    let mut settings = state.settings()?;
    let name = form.name.trim().to_string();
    settings.shares.insert(
        name,
        Share {
            path: absolute(&form.path, "the share path")?,
            smb: form.smb.is_some(),
            nfs: form.nfs.is_some(),
        },
    );
    settings.save(&state.config)?;
    Ok(Redirect::to("/settings"))
}

pub(super) async fn share_delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Page<Redirect> {
    let mut settings = state.settings()?;
    if settings.shares.remove(&name).is_none() {
        return Err(anyhow!("no hand-set share called {name}").into());
    }
    settings.save(&state.config)?;
    Ok(Redirect::to("/settings"))
}
