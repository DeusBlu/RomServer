//! Long operations as background jobs. Starting one answers at once with a progress bar that
//! polls every second until the job's result replaces it. The job holds the one-operation lock
//! for as long as it runs, so a second click (or a second browser) can't start another.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use axum::extract::{Path, State};
use maud::{Markup, PreEscaped, html};

use super::AppState;
use super::common::{AppError, Page};
use crate::progress::Progress;

/// Finished jobs kept for late polls; older ones are dropped.
const KEEP_FINISHED: usize = 8;

struct Job {
    title: String,
    progress: Arc<Progress>,
    /// The result's markup, once finished.
    outcome: Option<String>,
}

#[derive(Default)]
pub(super) struct Jobs {
    next_id: u64,
    by_id: BTreeMap<u64, Job>,
}

pub(super) type SharedJobs = Arc<Mutex<Jobs>>;

fn lock(jobs: &SharedJobs) -> std::sync::MutexGuard<'_, Jobs> {
    jobs.lock().unwrap_or_else(|e| e.into_inner())
}

/// Starts `work` in the background and returns the progress bar that follows it. Refuses while
/// another file-changing operation runs.
pub(super) fn start(
    state: &AppState,
    title: impl Into<String>,
    work: impl FnOnce(&Progress) -> anyhow::Result<Markup> + Send + 'static,
) -> Result<Markup, AppError> {
    let guard = state.busy.clone().try_lock_owned().map_err(|_| {
        AppError(anyhow!(
            "another operation is running; try again when it finishes"
        ))
    })?;
    let progress = Arc::new(Progress::default());
    let title = title.into();
    let id = {
        let mut jobs = lock(&state.jobs);
        jobs.next_id += 1;
        let id = jobs.next_id;
        jobs.by_id.insert(
            id,
            Job {
                title: title.clone(),
                progress: progress.clone(),
                outcome: None,
            },
        );
        id
    };
    let first = running(id, &title, &progress);
    let jobs = state.jobs.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let outcome = match work(&progress) {
            Ok(markup) => markup.into_string(),
            Err(e) => html! {
                div.notice.error { strong { (title) " failed: " } (format!("{e:#}")) }
            }
            .into_string(),
        };
        let mut jobs = lock(&jobs);
        if let Some(job) = jobs.by_id.get_mut(&id) {
            job.outcome = Some(outcome);
        }
        // Forget the oldest finished jobs.
        let finished: Vec<u64> = jobs
            .by_id
            .iter()
            .filter(|(_, j)| j.outcome.is_some())
            .map(|(id, _)| *id)
            .collect();
        for old in finished.iter().take(finished.len().saturating_sub(KEEP_FINISHED)) {
            jobs.by_id.remove(old);
        }
    });
    Ok(first)
}

fn running(id: u64, title: &str, progress: &Progress) -> Markup {
    let now = progress.snapshot();
    html! {
        div.job hx-get={"/jobs/" (id)} hx-trigger="every 1s" hx-swap="outerHTML" {
            p { strong { (title) } " · " (if now.stage.is_empty() { "starting" } else { &now.stage }) }
            @if now.total > 0 {
                progress value=(now.done) max=(now.total) {}
                span.muted.small { " " (now.done) " of " (now.total) }
            } @else {
                progress {}
            }
            p.muted.small { "You can leave this page; the job keeps going and other changes wait for it." }
        }
    }
}

/// Polled by the progress bar: the bar again while running, the result once finished.
pub(super) async fn poll(State(state): State<AppState>, Path(id): Path<u64>) -> Page {
    let jobs = lock(&state.jobs);
    let job = jobs
        .by_id
        .get(&id)
        .ok_or_else(|| anyhow!("that job is no longer known; reload the page"))?;
    Ok(match &job.outcome {
        Some(done) => PreEscaped(done.clone()),
        None => running(id, &job.title, &job.progress),
    })
}
