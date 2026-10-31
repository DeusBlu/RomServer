//! Machines pages: add and configure machines, preview and apply their views.

use anyhow::{Context, anyhow, bail};
use axum::extract::{Path, State};
use axum::response::Redirect;
use axum_extra::extract::Form;
use maud::{Markup, html};
use serde::Deserialize;

use super::AppState;
use super::common::{Page, blocking, busy_guard, layout, settings_or_notice, spinner};
use super::emulators::{kind_tag, names, offered, parse_picks};
use crate::catalog::Catalog;
use crate::ops::{self, NodePlan, Target};
use crate::settings::{Frontend, MultiDisc, Node, Uses, is_plain_id};
use crate::sync;

fn frontend_select(current: Frontend) -> Markup {
    html! {
        select name="frontend" {
            @for f in Frontend::ALL {
                option value=(f.id()) selected[*f == current] { (f.label()) }
            }
        }
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
                ", built from its frontend and the emulators it has."
            }
            table.grid {
                tr { th { "Machine" } th { "Frontend" } th { "Multi-disc" } th { "Emulators" } }
                @for (name, node) in &settings.nodes {
                    tr {
                        td { a href={"/machines/" (name)} { strong { (name) } } }
                        td { (node.frontend.label()) }
                        td { (node.multi_disc.label()) }
                        td { (node.uses.values().map(Vec::len).sum::<usize>()) }
                    }
                }
            }
            h2 { "Add a machine" }
            form.stack method="post" action="/machines" {
                label { "Name " input name="name" required pattern="[A-Za-z0-9_-]+" placeholder="steam-machine"; }
                label { "Frontend " (frontend_select(Frontend::EsDe)) }
                label { "Multi-disc games " (multi_disc_select(MultiDisc::default())) }
                button { "Add, then pick emulators" }
            }
        },
    ))
}

#[derive(Deserialize)]
pub(super) struct NewMachineForm {
    name: String,
    frontend: String,
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
    let node = Node {
        frontend: Frontend::from_id(&form.frontend).context("unknown frontend")?,
        multi_disc: MultiDisc::from_id(&form.multi_disc).context("unknown multi-disc mode")?,
        uses: Uses::new(),
    };
    settings.nodes.insert(name.clone(), node);
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
    let (owned, offered) = {
        let st = state.clone();
        let n = name.clone();
        let s2 = state.settings()?;
        blocking(move || {
            let db = st.db(&s2)?;
            Ok((db.owned(&n)?.len(), offered(&db, &s2)?))
        })
        .await?
    };
    let names = names(&offered);
    let view_root = settings.view_root(&name);
    Ok(layout(
        "Machines",
        html! {
            h2 { (name) }
            form.stack method="post" action={"/machines/" (name)} {
                label { "Frontend " (frontend_select(node.frontend)) }
                label { "Multi-disc games " (multi_disc_select(node.multi_disc)) }
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

#[derive(Deserialize)]
pub(super) struct MachineForm {
    frontend: String,
    multi_disc: String,
    #[serde(default)]
    pick: Vec<String>,
}

pub(super) async fn machine_save(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<MachineForm>,
) -> Page<Redirect> {
    let mut settings = state.settings()?;
    let node = settings.nodes.get_mut(&name).context("no such machine")?;
    node.frontend = Frontend::from_id(&form.frontend).context("unknown frontend")?;
    node.multi_disc = MultiDisc::from_id(&form.multi_disc).context("unknown multi-disc mode")?;
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
