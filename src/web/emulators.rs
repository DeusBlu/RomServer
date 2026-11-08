//! Emulators page: the global "emulators I use" list and custom emulators.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, anyhow, bail};
use axum::extract::{Path, State};
use axum::response::Redirect;
use axum_extra::extract::Form;
use maud::{Markup, html};
use serde::Deserialize;

use super::AppState;
use super::common::{Page, blocking, layout, settings_or_notice};
use crate::catalog::Catalog;
use crate::emulators::{EmulatorInfo, Kind};
use crate::settings::{CUSTOM_PREFIX, CustomEmulator, Settings, Uses, is_folder_name};

/// system → emulators offered for it: the catalog plus custom ones, sorted by name.
pub(super) type Offered = BTreeMap<String, Vec<EmulatorInfo>>;

pub(super) fn offered(db: &Catalog, settings: &Settings) -> anyhow::Result<Offered> {
    let mut by_system = db.emulators_by_system()?;
    for (id, custom) in &settings.custom_emulators {
        for system in &custom.systems {
            by_system
                .entry(system.clone())
                .or_default()
                .push(EmulatorInfo {
                    id: id.clone(),
                    name: custom.name.clone(),
                    kind: Kind::Custom,
                    corename: None,
                });
        }
    }
    for list in by_system.values_mut() {
        list.sort_by_key(|e| e.name.to_lowercase());
    }
    Ok(by_system)
}

/// Every emulator name the UI might need, keyed by id.
pub(super) fn names(offered: &Offered) -> HashMap<&str, &EmulatorInfo> {
    offered
        .values()
        .flatten()
        .map(|e| (e.id.as_str(), e))
        .collect()
}

/// Turns `system/emulator` checkbox values into a `Uses` map, accepting only pairs on `allowed`.
pub(super) fn parse_picks(
    picks: &[String],
    allowed: impl Fn(&str, &str) -> bool,
) -> anyhow::Result<Uses> {
    let mut uses = Uses::new();
    for pick in picks {
        let (system, id) = pick.split_once('/').context("malformed choice")?;
        if !allowed(system, id) {
            bail!("{id} isn't an option for {system}");
        }
        let list = uses.entry(system.to_string()).or_default();
        if !list.iter().any(|e| e == id) {
            list.push(id.to_string());
        }
    }
    Ok(uses)
}

pub(super) fn kind_tag(kind: Kind) -> Markup {
    let class = match kind {
        Kind::RetroArch => "tag ra",
        Kind::Standalone => "tag sa",
        Kind::Custom => "tag custom",
    };
    html! { span class=(class) { (kind.label()) } }
}

pub(super) async fn emulators_page(State(state): State<AppState>) -> Page {
    let settings = match settings_or_notice(&state, "Emulators") {
        Ok(s) => s,
        Err(page) => return Ok(page),
    };
    let (offered, system_names, existing_folders, settings) = {
        let st = state.clone();
        blocking(move || {
            let db = st.db(&settings)?;
            let existing = existing_save_folders(&settings);
            Ok((
                offered(&db, &settings)?,
                db.system_names()?,
                existing,
                settings,
            ))
        })
        .await?
    };
    let names = names(&offered);
    // (system, emulator) pairs: an emulator used for several systems keeps each one's saves apart.
    let in_use: Vec<(&str, &str)> = settings
        .uses
        .iter()
        .flat_map(|(system, ids)| ids.iter().map(move |id| (system.as_str(), id.as_str())))
        .collect();
    let none: Vec<EmulatorInfo> = Vec::new();
    Ok(layout(
        "Emulators",
        html! {
            p.muted {
                "Expand a system and tick the emulators you use for it. Ticking one emulator under "
                "several systems means you use it for all of them. Each machine then ticks the ones it has. "
                "The list comes from ES-DE's system definitions and libretro's core info."
            }
            form method="post" action="/emulators" {
                @for system in settings.systems.keys() {
                    @let emus = offered.get(system).unwrap_or(&none);
                    @let chosen = settings.uses.get(system).map_or(0, Vec::len);
                    details open[chosen > 0] {
                        summary {
                            (system) " "
                            span.muted { (system_names.get(system).map_or("", String::as_str)) }
                            @if chosen > 0 { " " span.tag.count { (chosen) " in use" } }
                        }
                        @if emus.is_empty() {
                            p.muted { "No known emulators for this system. Add a custom one below." }
                        }
                        @for e in emus {
                            label.check {
                                input type="checkbox" name="pick" value={(system) "/" (e.id)}
                                    checked[settings.uses.get(system).is_some_and(|u| u.contains(&e.id))];
                                span { (e.name) }
                                (kind_tag(e.kind))
                                @if let Some(core) = &e.corename {
                                    span.muted.small { "saves to RetroArch saves/" (core) }
                                }
                            }
                        }
                    }
                }
                div.savebar { button { "Save choices" } }
            }

            h2 { "Save folders" }
            p.muted {
                "Saves live in one folder per system under "
                code { (settings.saves_root().display()) } ", shared by every emulator you use for "
                "it: where their formats match they see the same save (Snes9x and bsnes both write "
                "raw SRAM), where they don't each keeps its own file beside the others. Leave a box "
                "empty to use the system's folder (shown greyed), or name another folder. An "
                "emulator used for several systems keeps each system's saves apart (for RetroArch "
                "that needs a machine to sort saves by content folder). Save states are never "
                "shared between emulators: each keeps its own, for every system it runs."
            }
            @if in_use.is_empty() {
                p.muted { "Tick some emulators above first." }
            } @else {
                form method="post" action="/emulators/save-folders" {
                    datalist #save-folders { @for f in &existing_folders { option value=(f); } }
                    table.grid {
                        tr { th { "System" } th { "Emulator" } th { "Save folder" } }
                        @for (system, id) in &in_use {
                            tr {
                                td { (system) }
                                td {
                                    @match names.get(id) {
                                        Some(e) => { (e.name) " " (kind_tag(e.kind)) }
                                        None => { (id) }
                                    }
                                }
                                td {
                                    input name={"folder:" (system) "/" (id)} list="save-folders"
                                        placeholder=(Settings::default_save_folder(system))
                                        value=[settings.save_folders.get(*system).and_then(|f| f.get(*id))];
                                }
                            }
                        }
                    }
                    button { "Save folders" }
                }
            }

            h2 { "Custom emulators" }
            p.muted { "For emulators the catalog doesn't know. They show up under their systems above." }
            @if !settings.custom_emulators.is_empty() {
                table.grid {
                    tr { th { "Name" } th { "Systems" } th {} }
                    @for (id, emu) in &settings.custom_emulators {
                        tr {
                            td { strong { (emu.name) } }
                            td { (emu.systems.join(", ")) }
                            td {
                                form method="post" action={"/emulators/custom/" (id) "/delete"}
                                    onsubmit={"return confirm('Remove " (emu.name) " everywhere it is used?')"} {
                                    button.link { "remove" }
                                }
                            }
                        }
                    }
                }
            }
            form.stack method="post" action="/emulators/custom" {
                label { "Name " input name="name" required maxlength="60" placeholder="My Emulator"; }
                fieldset {
                    legend { "Systems it runs" }
                    @for sys in settings.systems.keys() {
                        label.check { input type="checkbox" name="systems" value=(sys); (sys) }
                    }
                }
                button { "Add custom emulator" }
            }
        },
    ))
}

#[derive(Deserialize)]
pub(super) struct PicksForm {
    #[serde(default)]
    pick: Vec<String>,
}

pub(super) async fn emulators_save(
    State(state): State<AppState>,
    Form(form): Form<PicksForm>,
) -> Page<Redirect> {
    let mut settings = state.settings()?;
    let st = state.clone();
    let settings = blocking(move || {
        let offered = offered(&st.db(&settings)?, &settings)?;
        settings.uses = parse_picks(&form.pick, |system, id| {
            settings.systems.contains_key(system)
                && offered
                    .get(system)
                    .is_some_and(|l| l.iter().any(|e| e.id == id))
        })?;
        settings.prune_uses();
        Ok(settings)
    })
    .await?;
    settings.save(&state.config)?;
    Ok(Redirect::to("/emulators"))
}

/// Folders already under the saves root, offered as suggestions.
fn existing_save_folders(settings: &Settings) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(settings.saves_root())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    out.sort_by_key(|n| n.to_lowercase());
    out
}

/// Fields are `folder:<system>/<emulator id>` → folder name; empty clears the assignment.
pub(super) async fn save_folders_save(
    State(state): State<AppState>,
    Form(fields): Form<Vec<(String, String)>>,
) -> Page<Redirect> {
    let mut settings = state.settings()?;
    for (key, value) in fields {
        let Some((system, id)) = key
            .strip_prefix("folder:")
            .and_then(|pair| pair.split_once('/'))
        else {
            continue;
        };
        if !settings
            .uses
            .get(system)
            .is_some_and(|ids| ids.iter().any(|e| e == id))
        {
            return Err(anyhow!("{id} isn't an emulator in use for {system}").into());
        }
        let folder = value.trim();
        if folder.is_empty() || folder == Settings::default_save_folder(system) {
            if let Some(folders) = settings.save_folders.get_mut(system) {
                folders.remove(id);
                if folders.is_empty() {
                    settings.save_folders.remove(system);
                }
            }
        } else if !is_folder_name(folder) {
            return Err(anyhow!("{folder:?} isn't a plain folder name").into());
        } else {
            settings
                .save_folders
                .entry(system.to_string())
                .or_default()
                .insert(id.to_string(), folder.to_string());
        }
    }
    settings.save(&state.config)?;
    Ok(Redirect::to("/emulators"))
}

#[derive(Deserialize)]
pub(super) struct CustomEmulatorForm {
    name: String,
    #[serde(default)]
    systems: Vec<String>,
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

pub(super) async fn custom_emulator_add(
    State(state): State<AppState>,
    Form(form): Form<CustomEmulatorForm>,
) -> Page<Redirect> {
    let mut settings = state.settings()?;
    let name = form.name.trim().to_string();
    let id = format!("{CUSTOM_PREFIX}{}", slug(&name));
    if name.is_empty() || id == CUSTOM_PREFIX {
        return Err(anyhow!("give the emulator a name with at least one letter or digit").into());
    }
    if settings.custom_emulators.contains_key(&id) {
        return Err(anyhow!("a custom emulator called {name} already exists").into());
    }
    if form.systems.is_empty() {
        return Err(anyhow!("tick at least one system").into());
    }
    settings.custom_emulators.insert(
        id,
        CustomEmulator {
            name,
            systems: form.systems,
        },
    );
    settings.save(&state.config)?;
    Ok(Redirect::to("/emulators"))
}

pub(super) async fn custom_emulator_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Page<Redirect> {
    let mut settings = state.settings()?;
    settings
        .custom_emulators
        .remove(&id)
        .context("no such custom emulator")?;
    for ids in settings.uses.values_mut() {
        ids.retain(|e| *e != id);
    }
    settings.prune_uses();
    settings.save(&state.config)?;
    Ok(Redirect::to("/emulators"))
}
