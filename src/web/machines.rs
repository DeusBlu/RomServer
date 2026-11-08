//! Machines pages: add and configure machines, preview and apply their views.

use anyhow::{Context, anyhow, bail};
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::Form;
use maud::{Markup, html};
use serde::Deserialize;

use super::AppState;
use super::common::{Page, blocking, busy_guard, layout, settings_or_notice, spinner};
use super::emulators::{kind_tag, names, offered, parse_picks};
use crate::catalog::Catalog;
use crate::mounts::{self, MountKind, MountPlan};
use crate::ops::{self, NodePlan, Target};
use crate::platforms::{self, RetroArchSort};
use crate::settings::{MultiDisc, Node, is_plain_id};
use crate::sync;
use crate::{fixes, media};

fn platform_select(current: &str) -> Markup {
    html! {
        select name="platform" {
            @for (id, p) in &platforms::bundled().platforms {
                option value=(id) selected[id == current] { (p.label) }
            }
        }
    }
}

pub(super) fn platform_label(id: &str) -> &str {
    platforms::bundled()
        .platforms
        .get(id)
        .map_or(id, |p| p.label.as_str())
}

fn sort_select(current: Option<RetroArchSort>, default: RetroArchSort) -> Markup {
    html! {
        select name="retroarch_sort" {
            option value="" selected[current.is_none()] { "Platform default: " (default.label()) }
            @for s in RetroArchSort::ALL {
                option value=(s.id()) selected[current == Some(*s)] { (s.label()) }
            }
        }
    }
}

fn kind_label(kind: MountKind) -> &'static str {
    match kind {
        MountKind::Roms => "ROMs",
        MountKind::Media => "Media",
        MountKind::Saves => "Saves",
        MountKind::States => "States",
    }
}

fn multi_disc_select(current: MultiDisc) -> Markup {
    html! {
        select name="multi_disc" {
            @for m in MultiDisc::ALL {
                option value=(m.id()) selected[*m == current] { (m.label()) }
            }
        }
    }
}

pub(super) async fn machines_page(State(state): State<AppState>) -> Page {
    let settings = match settings_or_notice(&state, "Machines") {
        Ok(s) => s,
        Err(page) => return Ok(page),
    };
    Ok(layout(
        "Machines",
        html! {
            p.muted {
                "Every machine gets its own view under " code { (settings.views_root.display()) }
                ", built from its platform and the emulators it has."
            }
            table.grid {
                tr { th { "Machine" } th { "Platform" } th { "Multi-disc" } th { "Emulators" } }
                @for (name, node) in &settings.nodes {
                    tr {
                        td { a href={"/machines/" (name)} { strong { (name) } } }
                        td { (platform_label(&node.platform)) }
                        td { (node.multi_disc.label()) }
                        td { (node.uses.values().map(Vec::len).sum::<usize>()) }
                    }
                }
            }
            h2 { "Add a machine" }
            form.stack method="post" action="/machines" {
                label { "Name " input name="name" required pattern="[A-Za-z0-9_-]+" placeholder="steam-machine"; }
                label { "Platform " (platform_select("es-de")) }
                label { "Multi-disc games " (multi_disc_select(MultiDisc::default())) }
                button { "Add, then pick emulators" }
            }
        },
    ))
}

#[derive(Deserialize)]
pub(super) struct NewMachineForm {
    name: String,
    platform: String,
    multi_disc: String,
}

pub(super) async fn machine_add(
    State(state): State<AppState>,
    Form(form): Form<NewMachineForm>,
) -> Page<Redirect> {
    let mut settings = state.settings()?;
    let name = form.name.trim().to_string();
    if !is_plain_id(&name) {
        return Err(anyhow!("machine name may only use letters, digits, - and _").into());
    }
    if settings.nodes.contains_key(&name) {
        return Err(anyhow!("{name} already exists").into());
    }
    platforms::bundled().platform(&form.platform)?;
    let multi_disc = MultiDisc::from_id(&form.multi_disc).context("unknown multi-disc mode")?;
    settings
        .nodes
        .insert(name.clone(), Node::new(&form.platform, multi_disc));
    settings.save(&state.config)?;
    Ok(Redirect::to(&format!("/machines/{name}")))
}

pub(super) async fn machine_page(State(state): State<AppState>, Path(name): Path<String>) -> Page {
    let settings = match settings_or_notice(&state, "Machines") {
        Ok(s) => s,
        Err(page) => return Ok(page),
    };
    let node = settings.node(&name)?;
    let systems: Vec<&str> = settings.node_systems(node).into_iter().collect();
    let (owned, offered, mount_plan, missing) = {
        let st = state.clone();
        let n = name.clone();
        let s2 = state.settings()?;
        blocking(move || {
            let db = st.db(&s2)?;
            let offered = offered(&db, &s2)?;
            let plan = mounts::plan_mounts(&s2, &n, &names(&offered))?;
            let missing = mounts::missing_server_folders(&plan);
            Ok((db.owned(&n)?.len(), offered, plan, missing))
        })
        .await?
    };
    let names = names(&offered);
    let view_root = settings.view_root(&name);
    let platform = platforms::bundled().platform(&node.platform)?;
    Ok(layout(
        "Machines",
        html! {
            h2 { (name) }
            form.stack method="post" action={"/machines/" (name)} {
                label { "Platform " (platform_select(&node.platform)) }
                label { "Multi-disc games " (multi_disc_select(node.multi_disc)) }
                label { "Home folder " input name="home" size="30" placeholder=(platform.default_home)
                    value=[node.home.as_deref()]; }
                label { "Base folder " input name="base" size="30" placeholder=(platform.base)
                    value=[node.base.as_deref()]; }
                p.muted.small { "Change the base folder if the platform lives somewhere else (e.g. EmuDeck on an SD card: "
                    code { "/run/media/mmcblk0p1/Emulation" } ")." }
                label { "File owner (uid:gid) "
                    input name="uid" type="number" min="0" style="width:6em" value=(node.uid);
                    input name="gid" type="number" min="0" style="width:6em" value=(node.gid); }
                label { "SMB credentials file " input name="credentials" size="36"
                    placeholder="/home/you/.smbcredentials (empty = guest)"
                    value=[node.credentials.as_deref()]; }
                p.muted.small { "A file on the machine with " code { "username=" } " and " code { "password=" }
                    " lines. RomServer only writes its path into the mount units." }
                label { "RetroArch saves " (sort_select(node.retroarch_sort, platform.retroarch_sort)) }
                label.check { input type="checkbox" name="sync_states" value="1" checked[node.sync_states];
                    "Also share save states (they only load in the same emulator version)" }
                fieldset {
                    legend { "Emulators on this machine" }
                    @if settings.uses.is_empty() {
                        p.muted { "No emulators chosen yet. " a href="/emulators" { "Pick some" } " first." }
                    }
                    @for (system, ids) in &settings.uses {
                        div.group {
                            strong { (system) }
                            @for id in ids {
                                label.check {
                                    input type="checkbox" name="pick" value={(system) "/" (id)}
                                        checked[node.uses.get(system).is_some_and(|u| u.contains(id))];
                                    @match names.get(id.as_str()) {
                                        Some(e) => { span { (e.name) } (kind_tag(e.kind)) }
                                        None => { span { (id) } span.muted { " (not in the catalog any more)" } }
                                    }
                                }
                            }
                        }
                    }
                }
                button { "Save settings" }
            }
            p.muted {
                "Systems in this view: "
                @if systems.is_empty() { "none (tick some emulators)" } @else { (systems.join(", ")) }
            }

            h2 { "View" }
            table.grid {
                tr { th { "Folder" } td { code { (view_root.display()) } } }
                tr { th { "Links RomServer owns" } td { (owned) } }
            }
            p.muted { "Save settings first; the preview always reflects the saved settings." }
            div.row {
                form hx-post={"/machines/" (name) "/plan"} hx-target="#result" hx-indicator="#plan-spin" {
                    input type="hidden" name="target" value="build";
                    button { "Preview changes" }
                }
                form hx-post={"/machines/" (name) "/plan"} hx-target="#result" hx-indicator="#plan-spin" {
                    input type="hidden" name="target" value="clear";
                    button.secondary { "Preview clearing the view" }
                }
                (spinner("plan-spin"))
            }
            div #result {}

            h2 { "New media added" }
            p.muted {
                "Scraped on " (name) "? Its frontend wrote the files into this machine's view. "
                "RomServer copies them into the library's media folder under its own names, turns "
                "this machine's files into projections, and adds the new media to every other "
                "machine's view in its frontend's layout. Re-scraped art replaces what the "
                "library had; the old file is kept in the undo folder."
            }
            form hx-post={"/machines/" (name) "/media/plan"} hx-target="#ingest" hx-indicator="#ingest-spin" {
                button.secondary { "Find new media" }
                (spinner("ingest-spin"))
            }
            div #ingest {}

            (mounts_section(&name, &mount_plan, &missing))

            h2 { "Frontend fixes" }
            div hx-get={"/machines/" (name) "/fixes"} hx-trigger="load" {
                span.muted { "Checking " (name) "'s systems…" }
            }

            h2 { "Remove machine" }
            @if owned > 0 {
                p.muted { "Clear its view first, so no links are left behind." }
            } @else {
                form method="post" action={"/machines/" (name) "/delete"}
                    onsubmit={"return confirm('Remove machine " (name) "?')"} {
                    button.danger { "Remove " (name) }
                }
            }
        },
    ))
}

fn mounts_section(name: &str, plan: &MountPlan, missing: &[std::path::PathBuf]) -> Markup {
    html! {
        h2 { "Mounts" }
        p.muted {
            "What " (name) " mounts from its view on the server: its ROMs and media, and the folders its "
            "emulators keep saves in, filled with the library's saves in the shape this machine expects. "
            "Download the script, copy it to the machine and run it with sudo. It installs automounts and "
            "nothing else: it never moves, copies or deletes a file, and stops without changing anything "
            "if a folder it would mount over has files in it."
        }
        @if plan.mounts.is_empty() {
            p.muted { "Nothing to mount yet. Check the server address and shares under " a href="/settings" { "Settings" } "." }
        } @else {
            table.grid {
                tr { th { "" } th { "On " (name) } th { "From the server" } }
                @for m in &plan.mounts {
                    tr {
                        td { span.tag { (kind_label(m.kind)) } }
                        td { (m.label) br; code.small { (m.local.display()) } }
                        td { code.small { (m.remote.what) } }
                    }
                }
            }
        }
        @if !plan.warnings.is_empty() {
            div.notice.warn {
                strong { "Needs attention" }
                ul { @for w in &plan.warnings { li { (w) } } }
            }
        }
        @if !plan.notes.is_empty() {
            details open {
                summary { "Notes for this platform" }
                ul { @for n in &plan.notes { li { (n) } } }
            }
        }
        @if !missing.is_empty() {
            div.notice.warn {
                p { (missing.len()) " save folder(s) don't exist on the server yet, and mounting a missing folder fails:" }
                ul.paths { @for m in missing { li { (m.display()) } } }
                form method="post" action={"/machines/" (name) "/save-folders"} {
                    button { "Create them" }
                }
            }
        }
        @if !plan.mounts.is_empty() {
            p { a href={"/machines/" (name) "/mounts.sh"} download={"romserver-" (name) ".sh"} {
                "Download romserver-" (name) ".sh" } }
        }
    }
}

#[derive(Deserialize)]
pub(super) struct MachineForm {
    platform: String,
    multi_disc: String,
    #[serde(default)]
    home: String,
    #[serde(default)]
    base: String,
    uid: u32,
    gid: u32,
    #[serde(default)]
    credentials: String,
    #[serde(default)]
    retroarch_sort: String,
    #[serde(default)]
    sync_states: Option<String>,
    #[serde(default)]
    pick: Vec<String>,
}

fn optional(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

pub(super) async fn machine_save(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<MachineForm>,
) -> Page<Redirect> {
    let mut settings = state.settings()?;
    platforms::bundled().platform(&form.platform)?;
    let node = settings.nodes.get_mut(&name).context("no such machine")?;
    node.platform = form.platform;
    node.multi_disc = MultiDisc::from_id(&form.multi_disc).context("unknown multi-disc mode")?;
    node.home = optional(&form.home);
    node.base = optional(&form.base);
    node.uid = form.uid;
    node.gid = form.gid;
    node.credentials = optional(&form.credentials);
    node.retroarch_sort = match form.retroarch_sort.as_str() {
        "" => None,
        id => Some(RetroArchSort::from_id(id).context("unknown RetroArch sort")?),
    };
    node.sync_states = form.sync_states.is_some();
    let global = settings.uses.clone();
    let node = settings.nodes.get_mut(&name).context("no such machine")?;
    node.uses = parse_picks(&form.pick, |system, id| {
        global
            .get(system)
            .is_some_and(|g| g.iter().any(|e| e == id))
    })?;
    settings.save(&state.config)?;
    Ok(Redirect::to(&format!("/machines/{name}")))
}

#[derive(Deserialize)]
pub(super) struct PlanForm {
    /// "build" or "clear"
    target: String,
    #[serde(default)]
    allow_empty: Option<String>,
}

impl PlanForm {
    fn target(&self) -> anyhow::Result<Target> {
        match self.target.as_str() {
            "build" => Ok(Target::Settings {
                allow_empty: self.allow_empty.is_some(),
            }),
            "clear" => Ok(Target::Empty),
            other => bail!("unknown target {other}"),
        }
    }
}

pub(super) async fn machine_plan(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<PlanForm>,
) -> Page {
    let settings = state.settings()?;
    let target = form.target()?;
    let n = name.clone();
    let np = blocking(move || {
        let catalog = Catalog::open(&settings.database)?;
        ops::plan_node(&settings, &catalog, &n, target)
    })
    .await?;
    Ok(render_plan(&name, &form, &np))
}

const LIST_LIMIT: usize = 500;

fn path_list(items: &[String]) -> Markup {
    html! {
        ul.paths {
            @for i in items.iter().take(LIST_LIMIT) { li { (i) } }
            @if items.len() > LIST_LIMIT { li.muted { "… and " (items.len() - LIST_LIMIT) " more" } }
        }
    }
}

fn render_plan(name: &str, form: &PlanForm, np: &NodePlan) -> Markup {
    let p = &np.plan;
    let links = |v: &[crate::project::Link]| -> Vec<String> {
        v.iter().map(|l| l.dst.display().to_string()).collect()
    };
    let removes: Vec<String> = p.remove.iter().map(|r| r.display().to_string()).collect();
    html! {
        div.cards {
            div.card { div.big { (p.create.len()) } div.muted { "to create" } }
            div.card { div.big { (p.replace.len()) } div.muted { "to relink" } }
            div.card { div.big { (p.remove.len()) } div.muted { "to remove" } }
            div.card { div.big { (p.keep.len()) } div.muted { "unchanged" } }
            div.card.warn[!p.conflicts.is_empty()] {
                div.big { (p.conflicts.len()) } div.muted { "left alone" }
            }
        }
        @if !p.conflicts.is_empty() {
            details open {
                summary { "Left alone (" (p.conflicts.len()) ")" }
                (path_list(&p.conflicts))
            }
        }
        @if !np.warnings.is_empty() {
            details {
                summary { "Library warnings (" (np.warnings.len()) ")" }
                (path_list(&np.warnings))
            }
        }
        @if !p.create.is_empty() { details { summary { "Create" } (path_list(&links(&p.create))) } }
        @if !p.replace.is_empty() { details { summary { "Relink" } (path_list(&links(&p.replace))) } }
        @if !p.remove.is_empty() { details { summary { "Remove" } (path_list(&removes)) } }
        @if p.is_noop() {
            div.notice.ok { "The view is up to date." }
        } @else {
            form hx-post={"/machines/" (name) "/apply"} hx-target="#result" hx-indicator="#plan-spin"
                hx-confirm={"Apply these changes to " (name) "'s view?"} {
                input type="hidden" name="target" value=(form.target);
                @if form.allow_empty.is_some() { input type="hidden" name="allow_empty" value="1"; }
                input type="hidden" name="create" value=(p.create.len());
                input type="hidden" name="replace" value=(p.replace.len());
                input type="hidden" name="remove" value=(p.remove.len());
                button.danger { "Apply" }
            }
        }
    }
}

#[derive(Deserialize)]
pub(super) struct ApplyForm {
    target: String,
    #[serde(default)]
    allow_empty: Option<String>,
    create: usize,
    replace: usize,
    remove: usize,
}

pub(super) async fn machine_apply(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<ApplyForm>,
) -> Page {
    let _guard = busy_guard(&state)?;
    let settings = state.settings()?;
    let target = PlanForm {
        target: form.target,
        allow_empty: form.allow_empty,
    }
    .target()?;
    let n = name.clone();
    let expected = (form.create, form.replace, form.remove);
    let np = blocking(move || {
        let mut catalog = Catalog::open(&settings.database)?;
        let np = ops::plan_node(&settings, &catalog, &n, target)?;
        let p = &np.plan;
        if (p.create.len(), p.replace.len(), p.remove.len()) != expected {
            bail!("the library or view changed since the preview; preview again");
        }
        sync::apply(&mut catalog, &n, &np.view_root, &np.plan)?;
        Ok(np)
    })
    .await?;
    let p = &np.plan;
    Ok(html! {
        div.notice.ok {
            "Done: " (p.create.len()) " created, " (p.replace.len()) " relinked, "
            (p.remove.len()) " removed."
            @if !p.conflicts.is_empty() { " " (p.conflicts.len()) " left alone." }
        }
        p { a href={"/machines/" (name)} { "Refresh" } }
    })
}

async fn plan_for(state: &AppState, name: &str) -> Result<MountPlan, super::common::AppError> {
    let st = state.clone();
    let n = name.to_string();
    let settings = state.settings()?;
    blocking(move || {
        let offered = offered(&st.db(&settings)?, &settings)?;
        mounts::plan_mounts(&settings, &n, &names(&offered))
    })
    .await
}

pub(super) async fn mounts_script(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Page<Response> {
    let plan = plan_for(&state, &name).await?;
    let generated = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("at unix time {}", d.as_secs()))
        .unwrap_or_default();
    let body = mounts::script(&plan, &generated);
    Ok((
        [
            (
                header::CONTENT_TYPE,
                "text/x-shellscript; charset=utf-8".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"romserver-{name}.sh\""),
            ),
        ],
        body,
    )
        .into_response())
}

/// Creates the canonical save folders a machine's mounts need. Empty folders only; the
/// library tools' rule (never change existing files) holds.
pub(super) async fn save_folders_create(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Page<Redirect> {
    let _guard = busy_guard(&state)?;
    let plan = plan_for(&state, &name).await?;
    blocking(move || {
        for folder in mounts::missing_server_folders(&plan) {
            crate::ownership::create_dir_all(&folder)
                .with_context(|| format!("create {}", folder.display()))?;
        }
        Ok(())
    })
    .await?;
    Ok(Redirect::to(&format!("/machines/{name}")))
}

pub(super) async fn media_plan(State(state): State<AppState>, Path(name): Path<String>) -> Page {
    let settings = state.settings()?;
    let n = name.clone();
    let (plan, media_root) = blocking(move || {
        let catalog = Catalog::open(&settings.database)?;
        Ok((
            media::plan_ingest(&settings, &catalog, &n)?,
            settings.media_root(),
        ))
    })
    .await?;
    let replacing = plan.items.iter().filter(|i| !i.replaces.is_empty()).count();
    let rows: Vec<String> = plan
        .items
        .iter()
        .take(LIST_LIMIT)
        .map(|i| {
            let target = i.target.strip_prefix(&media_root).unwrap_or(&i.target);
            let mark = if i.replaces.is_empty() {
                ""
            } else {
                "  (replaces)"
            };
            format!("{}  →  {}{mark}", i.view_path.display(), target.display())
        })
        .collect();
    Ok(html! {
        div.cards {
            div.card { div.big { (plan.items.len()) } div.muted { "to import" } }
            div.card { div.big { (replacing) } div.muted { "replace existing" } }
            div.card.warn[!plan.skipped.is_empty()] { div.big { (plan.skipped.len()) } div.muted { "left alone" } }
        }
        @if !plan.skipped.is_empty() {
            details { summary { "Left alone (" (plan.skipped.len()) ")" } (path_list(&plan.skipped)) }
        }
        @if plan.items.is_empty() {
            div.notice.ok { "No new media in " (name) "'s view." }
        } @else {
            details { summary { "Files" } (path_list(&rows)) }
            form hx-post={"/machines/" (name) "/media/import"} hx-target="#ingest" hx-indicator="#ingest-spin"
                hx-confirm={"Import " (plan.items.len()) " media files from " (name) " and add them to every view?"} {
                input type="hidden" name="expected" value=(plan.items.len());
                button { "Import and project " (plan.items.len()) " files" }
            }
        }
    })
}

/// Per machine: (links added, links relinked), or why its view couldn't be updated.
type Projected = Vec<(String, Result<(usize, usize), String>)>;

#[derive(Deserialize)]
pub(super) struct IngestForm {
    expected: usize,
}

pub(super) async fn media_import(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<IngestForm>,
) -> Page {
    let _guard = busy_guard(&state)?;
    let settings = state.settings()?;
    let n = name.clone();
    let (imported, log, projected) = blocking(move || {
        let mut catalog = Catalog::open(&settings.database)?;
        let plan = media::plan_ingest(&settings, &catalog, &n)?;
        if plan.items.len() != form.expected {
            bail!("the view changed since the preview; find new media again");
        }
        let (imported, log) = media::apply_ingest(&settings, &mut catalog, &n, &plan)?;
        // Additions only, so no preview: nothing can be taken away from any view.
        let projected: Projected = settings
            .nodes
            .keys()
            .map(|node| {
                let result = ops::apply_additions(&settings, &mut catalog, node)
                    .map_err(|e| format!("{e:#}"));
                (node.clone(), result)
            })
            .collect();
        Ok((imported, log, projected))
    })
    .await?;
    Ok(html! {
        div.notice.ok {
            "Imported " (imported) " files from " (name) ". Undo log: " code { (log.display()) }
        }
        table.grid {
            tr { th { "Machine" } th { "Added" } th { "Relinked" } }
            @for (node, result) in &projected {
                tr {
                    td { (node) }
                    @match result {
                        Ok((added, relinked)) => { td { (added) } td { (relinked) } }
                        Err(e) => { td.warn colspan="2" { (e) } }
                    }
                }
            }
        }
    })
}

async fn bin_fix(
    state: &AppState,
    name: &str,
) -> Result<fixes::EsdeBinFix, super::common::AppError> {
    let settings = state.settings()?;
    let n = name.to_string();
    blocking(move || fixes::esde_bin_fix(&settings, &n)).await
}

pub(super) async fn fixes_section(State(state): State<AppState>, Path(name): Path<String>) -> Page {
    let fix = bin_fix(&state, &name).await?;
    let path = fix
        .path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    Ok(html! {
        @if !fix.is_needed() {
            p.muted { "Nothing to fix for " (name) "'s frontend." }
        } @else {
            div.notice.warn {
                p { strong { "ES-DE lists cue sheet tracks as games." }
                    " ES-DE's defaults include " code { ".bin" } " for these systems, so every track "
                    "file shows up beside its game:" }
                ul { @for (system, tracks) in &fix.fixed {
                    li { strong { (system) } ": " (tracks) " track files" }
                } }
            }
            @for (system, bins) in &fix.lone_bins {
                details {
                    summary { (system) ": " (bins.len()) " damaged (missing cue sheet), won't show up" }
                    p.muted.small {
                        "A .bin with no cue sheet: a disc rip missing its cue, or an old cartridge dump "
                        "(this system has its own extension now). Neither can play properly, so it's "
                        "left out of every machine's view. Give it a cue sheet (or re-rip it), or "
                        "rename a cartridge to the system's own extension."
                    }
                    (path_list(&bins.iter().map(|b| b.display().to_string()).collect::<Vec<_>>()))
                }
            }
            p {
                strong { "To fix it by hand: " } "in ES-DE's custom systems file ("
                code { (path) } "), give each system above an entry that is ES-DE's own with "
                code { ".bin .BIN" } " taken out of " code { "<extension>" } ". ES-DE uses an entry "
                "there instead of its built-in one. Then restart ES-DE."
            }
            p { strong { "Or let RomServer do it: " }
                "paste that file below and get it back fixed (your own entries stay as they are). "
                "If the file doesn't exist on " (name) ", you have none: use RomServer's default." }
            form.stack hx-post={"/machines/" (name) "/fixes"} hx-target="#fixed-file" {
                textarea name="existing" rows="8" style="width:100%;font-family:ui-monospace,monospace"
                    placeholder={"contents of " (path)} {}
                div.row {
                    button name="choice" value="merge" { "Fix my file" }
                    button.secondary name="choice" value="default" { "I don't have one: use RomServer's default" }
                }
            }
            div #fixed-file {}
        }
    })
}

#[derive(Deserialize)]
pub(super) struct FixForm {
    #[serde(default)]
    existing: String,
    choice: String,
}

pub(super) async fn fixes_apply(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<FixForm>,
) -> Page {
    let fix = bin_fix(&state, &name).await?;
    let existing = match form.choice.as_str() {
        "default" => "",
        _ if form.existing.trim().is_empty() => {
            return Err(anyhow!(
                "paste your custom systems file, or choose RomServer's default if you have none"
            )
            .into());
        }
        _ => form.existing.as_str(),
    };
    let text = fix.apply(existing, &name)?;
    let path = fix
        .path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    Ok(html! {
        p { "Save this as " code { (path) } " on " (name) ", replacing what's there, then restart ES-DE." }
        textarea readonly rows="16" style="width:100%;font-family:ui-monospace,monospace"
            onfocus="this.select()" { (text) }
    })
}

pub(super) async fn machine_delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Page<Redirect> {
    let mut settings = state.settings()?;
    settings.node(&name)?;
    let db = settings.database.clone();
    let n = name.clone();
    let owned = blocking(move || Ok(Catalog::open(&db)?.owned(&n)?.len())).await?;
    if owned > 0 {
        return Err(anyhow!("{name}'s view still has {owned} links; clear it first").into());
    }
    settings.nodes.remove(&name);
    settings.save(&state.config)?;
    Ok(Redirect::to("/machines"))
}
