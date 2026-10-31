//! Library page: scan, check and fix cue sheets.

use anyhow::bail;
use axum::extract::State;
use axum_extra::extract::Form;
use maud::html;
use serde::Deserialize;

use super::AppState;
use super::common::{Page, blocking, busy_guard, layout, settings_or_notice, spinner};
use crate::cuefix::{self, Verdict};
use crate::ops;

pub(super) async fn library_page(State(state): State<AppState>) -> Page {
    let settings = match settings_or_notice(&state, "Library") {
        Ok(s) => s,
        Err(page) => return Ok(page),
    };
    Ok(layout(
        "Library",
        html! {
            p.muted {
                "Source folders under " code { (settings.library_root.display()) }
                ". Views never write here; only the tools on this page do, and each one previews first."
            }
            table.grid {
                tr { th { "System" } th { "Folder" } }
                @for (id, sys) in &settings.systems {
                    tr { td { (id) } td { code { (sys.folder.display()) } } }
                }
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
