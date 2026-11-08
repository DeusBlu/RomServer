//! Library page: which folders hold which system, scan, check and fix cue sheets.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use anyhow::{Context, anyhow, bail};
use axum::extract::{Path, State};
use axum::response::Redirect;
use axum_extra::extract::Form;
use maud::{Markup, html};
use serde::Deserialize;

use super::AppState;
use super::common::{Page, blocking, busy_guard, layout, settings_or_notice, spinner};
use crate::cuefix::{self, Verdict};
use crate::platforms::MEDIA_TYPES;
use crate::settings::{Settings, System, is_plain_id, is_plain_relative};
use crate::{media, ops};

/// Folders directly under the library that aren't a system yet and aren't RomServer's own
/// (saves, states, media). Not every folder is a system (a share can hold anything), so the
/// user picks.
fn unmapped_folders(settings: &Settings) -> anyhow::Result<Vec<String>> {
    let mapped: BTreeSet<PathBuf> = settings
        .systems
        .values()
        .map(|s| s.folder.clone())
        .collect();
    let own = [
        settings.saves_root(),
        settings.states_root(),
        settings.media_root(),
    ];
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&settings.library_root)
        .with_context(|| format!("read {}", settings.library_root.display()))?
    {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.')
            || !entry.file_type()?.is_dir()
            || mapped.contains(&PathBuf::from(&name))
            || own.contains(&entry.path())
        {
            continue;
        }
        out.push(name);
    }
    out.sort_by_key(|n| n.to_lowercase());
    Ok(out)
}

pub(super) async fn library_page(State(state): State<AppState>) -> Page {
    let settings = match settings_or_notice(&state, "Library") {
        Ok(s) => s,
        Err(page) => return Ok(page),
    };
    let (system_names, folders, settings) = {
        let st = state.clone();
        blocking(move || {
            let names = st.db(&settings)?.system_names()?;
            // A missing library is shown as a notice, not an error page.
            let folders = unmapped_folders(&settings).map_err(|e| format!("{e:#}"));
            Ok((names, folders, settings))
        })
        .await?
    };
    let mut choices: Vec<(&String, &String)> = system_names
        .iter()
        .filter(|(id, _)| !settings.systems.contains_key(*id))
        .collect();
    choices.sort_by_key(|(_, name)| name.to_lowercase());
    Ok(layout(
        "Library",
        html! {
            p.muted {
                "Source folders under " code { (settings.library_root.display()) }
                ". Views never write here; only the tools on this page do, and each one previews first."
            }
            @if settings.systems.is_empty() {
                p.muted { "No systems yet. Add a folder below." }
            } @else {
                table.grid {
                    tr { th { "System" } th { "Folder" } th {} }
                    @for (id, sys) in &settings.systems {
                        tr {
                            td {
                                strong { (id) } " "
                                span.muted { (system_names.get(id).map_or("", String::as_str)) }
                            }
                            td { code { (sys.folder.display()) } }
                            td {
                                form method="post" action={"/library/systems/" (id) "/delete"}
                                    onsubmit={"return confirm('Stop using " (id) "? Its emulator choices are dropped, and machine views lose it the next time they are applied. No files are deleted.')"} {
                                    button.link { "remove" }
                                }
                            }
                        }
                    }
                }
            }
            h2 { "Add a system folder" }
            @match &folders {
                Err(e) => { div.notice.error { "Can't list the library: " (e) } }
                Ok(folders) if folders.is_empty() => {
                    p.muted { "Every folder in the library is already a system." }
                }
                Ok(folders) => {
                    form.stack method="post" action="/library/systems" {
                        label { "Folder "
                            select name="folder" required {
                                @for f in folders { option value=(f) { (f) } }
                            }
                        }
                        label { "System "
                            select name="system" required {
                                @for (id, name) in &choices {
                                    option value=(id) { (name) " (" (id) ")" }
                                }
                            }
                        }
                        p.muted.small {
                            "Systems use ES-DE's short names, which every view is built from. "
                            "Folders that aren't systems (backups, archives) can simply be left out."
                        }
                        button { "Add system" }
                    }
                }
            }
            h2 { "Scraped media" }
            p.muted {
                "Kept in RomServer's own layout, one folder per game: "
                code { (settings.media_root().display()) "/<system>/<game>/box-front.png" }
                ". Every machine gets it in its own frontend's layout and names."
            }
            p.muted {
                strong { "Existing media: " }
                "media from before RomServer, in the media folder (ES-DE's " code { "covers/" } ", …) "
                "or in a system's ROM folder (" code { "images/" } ", " code { "media/" } ", an old "
                code { "Images/" } "). What RomServer recognises is imported; for the rest you choose "
                "to name it (game and kind) or discard it and re-scrape later."
            }
            @if settings.systems.is_empty() {
                p.muted.small { "Add a system first." }
            } @else {
                div.row {
                    @for system in settings.systems.keys() {
                        form hx-post="/library/media/plan" hx-target="#media" hx-indicator="#media-spin" {
                            input type="hidden" name="system" value=(system);
                            button.secondary { (system) }
                        }
                    }
                    (spinner("media-spin"))
                }
                div #media {}
            }

            h2 { "Scan" }
            p.muted { "Read-only: counts games, multi-disc sets and saves, and lists problems." }
            button hx-post="/library/scan" hx-target="#scan" hx-indicator="#scan-spin" { "Scan library" }
            (spinner("scan-spin"))
            div #scan {}
            h2 { "Cue sheets" }
            p.muted {
                "Checks every cue sheet's FILE references. Only fixes when exactly one file fits; "
                "the original is kept in the undo folder."
            }
            button hx-post="/library/cues" hx-target="#cues" hx-indicator="#cues-spin" { "Check cue sheets" }
            (spinner("cues-spin"))
            div #cues {}
        },
    ))
}

pub(super) async fn library_scan(State(state): State<AppState>) -> Page {
    let settings = state.settings()?;
    let summaries = blocking(move || {
        settings
            .systems
            .keys()
            .map(|s| ops::summarise_system(&settings, s))
            .collect::<anyhow::Result<Vec<_>>>()
    })
    .await?;
    Ok(html! {
        table.grid {
            tr { th { "System" } th { "Games" } th { "Multi-disc" } th { "Saves" } th { "Ignored" } th { "Issues" } }
            @for s in &summaries {
                tr {
                    td { (s.system) }
                    td { (s.games) }
                    td { (s.multi_disc) }
                    td { (s.saves) }
                    td { (s.ignored) }
                    td class=[(!s.issues.is_empty()).then_some("warn")] { (s.issues.len()) }
                }
            }
        }
        @for s in summaries.iter().filter(|s| !s.issues.is_empty()) {
            details {
                summary { (s.system) ": " (s.issues.len()) " issues" }
                ul { @for i in &s.issues { li { (i) } } }
            }
        }
    })
}

pub(super) async fn cues_check(State(state): State<AppState>) -> Page {
    let settings = state.settings()?;
    let reports = blocking(move || ops::check_cues(&settings, None)).await?;
    let fixable = reports.iter().filter(|r| r.fixable()).count();
    Ok(html! {
        @if reports.is_empty() {
            div.notice.ok { "Every cue sheet's references resolve." }
        } @else {
            table.grid {
                tr { th { "Cue sheet" } th { "References" } th { "Proposed" } }
                @for r in &reports {
                    @for b in &r.bad {
                        tr {
                            td { code { (r.cue.display()) } }
                            td { code { (b.reference) } }
                            @match &b.verdict {
                                Verdict::Fix(f) => { td.ok { "→ " code { (f) } } }
                                Verdict::Unresolved(why) => { td.warn { "needs a human: " (why) } }
                            }
                        }
                    }
                }
            }
            @if fixable > 0 {
                form hx-post="/library/cues/fix" hx-target="#cues" hx-indicator="#cues-spin"
                    hx-confirm={"Rewrite " (fixable) " cue sheet(s) in the library?"} {
                    input type="hidden" name="expected" value=(fixable);
                    button.danger { "Fix " (fixable) " cue sheet(s)" }
                }
            }
        }
    })
}

#[derive(Deserialize)]
pub(super) struct AddSystemForm {
    system: String,
    folder: String,
}

pub(super) async fn system_add(
    State(state): State<AppState>,
    Form(form): Form<AddSystemForm>,
) -> Page<Redirect> {
    let mut settings = state.settings()?;
    let folder = PathBuf::from(form.folder.trim());
    if !is_plain_id(&form.system) || !is_plain_relative(&folder) {
        return Err(anyhow!("pick a system and a folder").into());
    }
    if settings.systems.contains_key(&form.system) {
        return Err(anyhow!("{} is already set up", form.system).into());
    }
    if let Some((other, _)) = settings.systems.iter().find(|(_, s)| s.folder == folder) {
        return Err(anyhow!("{} is already the folder for {other}", folder.display()).into());
    }
    if !settings.library_root.join(&folder).is_dir() {
        return Err(anyhow!("{} isn't a folder in the library", folder.display()).into());
    }
    let st = state.clone();
    let known: HashMap<String, String> = {
        let s = state.settings()?;
        blocking(move || st.db(&s)?.system_names()).await?
    };
    if !known.contains_key(&form.system) {
        return Err(anyhow!("{} isn't a known system", form.system).into());
    }
    settings.systems.insert(form.system, System { folder });
    settings.save(&state.config)?;
    Ok(Redirect::to("/library"))
}

pub(super) async fn system_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Page<Redirect> {
    let mut settings = state.settings()?;
    if !settings.systems.contains_key(&id) {
        return Err(anyhow!("no system {id}").into());
    }
    settings.remove_system(&id);
    settings.save(&state.config)?;
    Ok(Redirect::to("/library"))
}

#[derive(Deserialize)]
pub(super) struct MediaForm {
    system: String,
}

fn kind_select(name: &str, current: Option<&str>, empty_label: &str) -> Markup {
    html! {
        select name=(name) {
            option value="" selected[current.is_none()] { (empty_label) }
            @for t in MEDIA_TYPES {
                option value=(t) selected[current == Some(*t)] { (t) }
            }
        }
    }
}

fn folder_of(item: &media::Existing) -> String {
    item.from
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

pub(super) async fn media_plan(State(state): State<AppState>, Form(form): Form<MediaForm>) -> Page {
    let settings = state.settings()?;
    let system = form.system.clone();
    let plan = blocking(move || {
        if !settings.systems.contains_key(&system) {
            bail!("no system {system}");
        }
        media::plan_existing(&settings, &system)
    })
    .await?;
    let mut by_folder: BTreeMap<String, Vec<(usize, &media::Existing)>> = BTreeMap::new();
    for (i, item) in plan.needs_input.iter().enumerate() {
        by_folder
            .entry(folder_of(item))
            .or_default()
            .push((i, item));
    }
    let auto_preview: Vec<String> = plan
        .auto
        .iter()
        .take(300)
        .map(|e| {
            let disc = e
                .disc
                .map(|n| format!("/{}", crate::project::disc_folder(n)))
                .unwrap_or_default();
            format!(
                "{}  →  {}{disc}/{}",
                e.from.display(),
                e.game.as_deref().unwrap_or("?"),
                e.kind.as_deref().unwrap_or("?")
            )
        })
        .collect();
    let duplicate_list: Vec<String> = plan
        .duplicates
        .iter()
        .map(|e| format!("{}  ({})", e.from.display(), e.reason))
        .collect();
    Ok(html! {
        h3 { "Existing media: " (form.system) }
        div.cards {
            div.card { div.big { (plan.in_place) } div.muted { "already in place" } }
            div.card { div.big { (plan.auto.len()) } div.muted { "ready to import" } }
            div.card { div.big { (plan.duplicates.len()) } div.muted { "duplicates" } }
            div.card.warn[!plan.needs_input.is_empty()] {
                div.big { (plan.needs_input.len()) } div.muted { "need your input" }
            }
        }
        p.muted.small {
            strong { "Already in place: " } "found in RomServer's layout, nothing to do. "
            strong { "Ready to import: " } "recognised, will be renamed into place. "
            strong { "Duplicates: " } "identical copies of art the game already has. "
            strong { "Need your input: " } "couldn't be placed on its own."
        }
        @if !duplicate_list.is_empty() {
            details { summary { "Duplicates (" (duplicate_list.len()) ")" }
                p.muted.small {
                    "Byte-identical copies of art the game already has (a disc set scraped once per "
                    "disc). They go to the undo folder."
                }
                ul.paths { @for d in duplicate_list.iter().take(300) { li { (d) } } }
            }
        }
        @if plan.auto.is_empty() && plan.needs_input.is_empty() && plan.duplicates.is_empty() {
            div.notice.ok {
                "All good: " (plan.in_place) " media files found for " (form.system)
                ", all already in RomServer's layout. Nothing to import."
            }
        } @else {
            @if !auto_preview.is_empty() {
                details { summary { "Ready to import (" (plan.auto.len()) ")" }
                    ul.paths {
                        @for p in &auto_preview { li { (p) } }
                        @if plan.auto.len() > auto_preview.len() {
                            li.muted { "… and " (plan.auto.len() - auto_preview.len()) " more" }
                        }
                    }
                }
            }
            form hx-post="/library/media/import" hx-target="#media" hx-indicator="#media-spin"
                hx-confirm={"Import " (form.system) "'s existing media?"} {
                input type="hidden" name="system" value=(form.system);
                input type="hidden" name="expected" value=(plan.auto.len());
                input type="hidden" name="expected-duplicates" value=(plan.duplicates.len());
                datalist #media-games { @for g in &plan.games { option value=(g); } }
                @for (folder, items) in &by_folder {
                    details open {
                        summary { code { (folder) } " (" (items.len()) ")" }
                        p.small.muted { "For every file below left on \u{201c}as the folder\u{201d}:" }
                        div.row {
                            select name={"folder-action:" (folder)} {
                                option value="leave" { "Leave them" }
                                option value="import" { "Import them" }
                                option value="discard" { "Discard them" }
                            }
                            (kind_select(&format!("folder-kind:{folder}"), None, "kind: use each file's"))
                        }
                        table.grid {
                            tr { th { "File" } th { "Why" } th { "Do" } th { "Game" } th { "Kind" } }
                            @for (i, item) in items {
                                tr {
                                    td { code.small { (file_name_of(item)) } }
                                    td.muted.small { (item.reason) }
                                    td {
                                        input type="hidden" name={"from:" (i)} value=(item.from.display());
                                        select name={"action:" (i)} {
                                            option value="folder" { "as the folder" }
                                            option value="import" { "rename and re-import" }
                                            option value="discard" { "discard" }
                                            option value="leave" { "leave" }
                                        }
                                    }
                                    td { input name={"game:" (i)} list="media-games" size="28"
                                        value=[item.game.as_deref()] placeholder="game"; }
                                    td { (kind_select(&format!("kind:{i}"), item.kind.as_deref(), "kind?")) }
                                }
                            }
                        }
                    }
                }
                p.muted.small {
                    "Imports are renames inside the library (nothing copied, nothing overwritten). "
                    "Discarded files go to the undo folder, so you can re-scrape instead. An undo log is "
                    "written first, and every machine's view gets the new media afterwards."
                }
                button { "Import" }
            }
        }
    })
}

fn file_name_of(item: &media::Existing) -> String {
    item.from
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub(super) async fn media_import(
    State(state): State<AppState>,
    Form(fields): Form<Vec<(String, String)>>,
) -> Page {
    let _guard = busy_guard(&state)?;
    let settings = state.settings()?;
    let fields: HashMap<String, String> = fields.into_iter().collect();
    let system = fields.get("system").cloned().context("missing system")?;
    let expected: usize = fields
        .get("expected")
        .and_then(|v| v.parse().ok())
        .context("missing expected count")?;
    let expected_duplicates: usize = fields
        .get("expected-duplicates")
        .and_then(|v| v.parse().ok())
        .context("missing expected duplicate count")?;
    let sys = system.clone();
    let (imported, discarded, log, projected) = blocking(move || {
        if !settings.systems.contains_key(&sys) {
            bail!("no system {sys}");
        }
        let plan = media::plan_existing(&settings, &sys)?;
        if plan.auto.len() != expected || plan.duplicates.len() != expected_duplicates {
            bail!("the media changed since the preview; look again");
        }
        let mut decided = Vec::new();
        for i in 0.. {
            let Some(from) = fields.get(&format!("from:{i}")) else {
                break;
            };
            // Only files the plan still reports: never anything the form makes up.
            let item = plan
                .needs_input
                .iter()
                .find(|e| e.from.display().to_string() == *from)
                .with_context(|| format!("{from} changed since the preview; look again"))?;
            let folder = folder_of(item);
            let get = |key: String| {
                fields
                    .get(&key)
                    .map(|v| v.trim().to_string())
                    .unwrap_or_default()
            };
            let mut action = get(format!("action:{i}"));
            if action == "folder" {
                action = get(format!("folder-action:{folder}"));
            }
            let decision = match action.as_str() {
                "import" => {
                    let mut kind = get(format!("kind:{i}"));
                    if kind.is_empty() {
                        kind = get(format!("folder-kind:{folder}"));
                    }
                    media::Decision::Import {
                        game: get(format!("game:{i}")),
                        kind,
                    }
                }
                "discard" => media::Decision::Discard,
                _ => media::Decision::Leave,
            };
            decided.push((item.clone(), decision));
        }
        let (imported, discarded, log) =
            media::apply_existing(&settings, &sys, &plan.auto, &plan.duplicates, &decided)?;
        let mut catalog = crate::catalog::Catalog::open(&settings.database)?;
        let projected: Vec<(String, String)> = settings
            .nodes
            .keys()
            .map(|node| {
                let result = match ops::apply_additions(&settings, &mut catalog, node) {
                    Ok((added, relinked)) => format!("{added} added, {relinked} relinked"),
                    Err(e) => format!("not updated: {e:#}"),
                };
                (node.clone(), result)
            })
            .collect();
        Ok((imported, discarded, log, projected))
    })
    .await?;
    Ok(html! {
        div.notice.ok {
            (system) ": imported " (imported) ", discarded " (discarded) ". Undo log: " code { (log.display()) }
        }
        @if !projected.is_empty() {
            table.grid {
                tr { th { "Machine" } th { "View" } }
                @for (node, result) in &projected { tr { td { (node) } td { (result) } } }
            }
        }
        p.muted.small { "Files you left are still there; look again any time." }
    })
}

#[derive(Deserialize)]
pub(super) struct CueFixForm {
    expected: usize,
}

pub(super) async fn cues_fix(State(state): State<AppState>, Form(form): Form<CueFixForm>) -> Page {
    let _guard = busy_guard(&state)?;
    let settings = state.settings()?;
    let (fixed, log) = blocking(move || {
        let reports = ops::check_cues(&settings, None)?;
        let fixable = reports.iter().filter(|r| r.fixable()).count();
        if fixable != form.expected {
            bail!("the library changed since the check ({fixable} fixable now); check again");
        }
        Ok((fixable, cuefix::apply(&reports, &settings.undo_dir)?))
    })
    .await?;
    Ok(html! {
        div.notice.ok {
            "Fixed " (fixed) " cue sheet(s). Undo log: " code { (log.display()) }
        }
    })
}
