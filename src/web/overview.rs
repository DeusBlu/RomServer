//! Overview page.

use std::collections::BTreeSet;

use axum::extract::State;
use maud::html;

use super::AppState;
use super::common::{Page, blocking, layout, settings_or_notice};
use crate::catalog::Catalog;

pub(super) async fn dashboard(State(state): State<AppState>) -> Page {
    let settings = match settings_or_notice(&state, "Overview") {
        Ok(s) => s,
        Err(page) => return Ok(page),
    };
    let owned: Vec<(String, usize)> = blocking(move || {
        let catalog = Catalog::open(&settings.database)?;
        settings
            .nodes
            .keys()
            .map(|n| Ok((n.clone(), catalog.owned(n)?.len())))
            .collect::<anyhow::Result<_>>()
    })
    .await?;
    let settings = state.settings()?;
    Ok(layout(
        "Overview",
        html! {
            div.cards {
                div.card { div.big { (settings.systems.len()) } div.muted { "systems" } }
                div.card {
                    div.big { (settings.uses.values().flatten().collect::<BTreeSet<_>>().len()) }
                    div.muted { "emulators in use" }
                }
                div.card { div.big { (settings.nodes.len()) } div.muted { "machines" } }
            }
            h2 { "Paths" }
            table.grid {
                tr { th { "Library (source, read-mostly)" } td { code { (settings.library_root.display()) } } }
                tr { th { "Views" } td { code { (settings.views_root.display()) } } }
                tr { th { "Database" } td { code { (settings.database.display()) } } }
                tr { th { "Settings file" } td { code { (state.config.display()) } } }
            }
            h2 { "Machines" }
            @if owned.is_empty() {
                p.muted { "No machines yet. " a href="/machines" { "Add one" } "." }
            } @else {
                table.grid {
                    tr { th { "Machine" } th { "Platform" } th { "Links in view" } }
                    @for (name, count) in &owned {
                        tr {
                            td { a href={"/machines/" (name)} { (name) } }
                            td { (super::machines::platform_label(&settings.nodes[name].platform)) }
                            td { (count) }
                        }
                    }
                }
            }
        },
    ))
}
