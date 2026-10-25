mod catalog;
mod cuefix;
mod project;
mod scan;
mod settings;
mod sync;
#[cfg(test)]
mod tests;

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use crate::catalog::Catalog;
use crate::project::Link;
use crate::settings::Settings;

#[derive(Parser)]
#[command(
    version,
    about = "Projects one ROM library into per-machine views with hard links"
)]
struct Cli {
    #[arg(
        long,
        env = "ROMSERVER_CONFIG",
        default_value = "/config/settings/romserver.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan library systems and report games, saves and problems. Read-only.
    Scan {
        /// Limit to one system (short name, e.g. psx).
        #[arg(long)]
        system: Option<String>,
        /// List every game.
        #[arg(long)]
        verbose: bool,
    },
    /// Show what `apply` would change in a node's view. Read-only.
    Plan {
        #[arg(long)]
        node: String,
        #[arg(long)]
        verbose: bool,
    },
    /// Build or update a node's view.
    Apply {
        #[arg(long)]
        node: String,
        /// Apply without asking (the plan is still printed).
        #[arg(long)]
        yes: bool,
        /// Allow a system that had games to become empty (normally a sign of an unmounted share).
        #[arg(long)]
        allow_empty: bool,
    },
    /// Library tool: check every cue sheet's FILE references; with --fix, repair the
    /// unambiguous ones (originals kept in the undo folder).
    Cues {
        #[arg(long)]
        system: Option<String>,
        #[arg(long)]
        fix: bool,
        #[arg(long)]
        yes: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let settings = Settings::load(&cli.config)?;
    match cli.command {
        Command::Scan { system, verbose } => cmd_scan(&settings, system.as_deref(), verbose),
        Command::Cues { system, fix, yes } => cmd_cues(&settings, system.as_deref(), fix, yes),
        Command::Plan { node, verbose } => {
            let catalog = Catalog::open(&settings.database)?;
            let (plan, _) = build_plan(&settings, &catalog, &node, false)?;
            print_plan(&plan, verbose);
            Ok(())
        }
        Command::Apply {
            node,
            yes,
            allow_empty,
        } => {
            let mut catalog = Catalog::open(&settings.database)?;
            let (plan, view_root) = build_plan(&settings, &catalog, &node, allow_empty)?;
            print_plan(&plan, false);
            if plan.is_noop() {
                println!("Nothing to do.");
                return Ok(());
            }
            if !yes && !confirm()? {
                println!("Aborted.");
                return Ok(());
            }
            sync::apply(&mut catalog, &node, &view_root, &plan)?;
            println!("Applied.");
            Ok(())
        }
    }
}

fn cmd_scan(settings: &Settings, only: Option<&str>, verbose: bool) -> Result<()> {
    for system in settings
        .systems
        .keys()
        .filter(|s| only.is_none_or(|o| o == s.as_str()))
    {
        let root = settings.system_root(system)?;
        let scan = scan::scan_system(&root)?;
        let multi = scan
            .games
            .iter()
            .filter(|g| matches!(g, scan::Game::MultiDisc { .. }))
            .count();
        println!(
            "{system} ({}): {} games ({multi} multi-disc), {} saves, {} ignored, {} issues",
            root.display(),
            scan.games.len(),
            scan.saves.len(),
            scan.ignored.len(),
            scan.issues.len()
        );
        for issue in &scan.issues {
            println!("  ! {issue}");
        }
        if verbose {
            for g in &scan.games {
                println!("  {}", g.name());
            }
        }
    }
    Ok(())
}

fn cmd_cues(settings: &Settings, only: Option<&str>, fix: bool, yes: bool) -> Result<()> {
    let mut reports = Vec::new();
    for system in settings
        .systems
        .keys()
        .filter(|s| only.is_none_or(|o| o == s.as_str()))
    {
        reports.extend(cuefix::check(&settings.system_root(system)?)?);
    }
    for r in &reports {
        for b in &r.bad {
            match &b.verdict {
                cuefix::Verdict::Fix(f) => {
                    println!("fix  {}: {:?} -> {:?}", r.cue.display(), b.reference, f)
                }
                cuefix::Verdict::Unresolved(why) => {
                    println!("SKIP {}: {:?} ({why})", r.cue.display(), b.reference)
                }
            }
        }
    }
    let fixable = reports.iter().filter(|r| r.fixable()).count();
    println!(
        "{} cue sheets with bad references: {fixable} fixable, {} need a human",
        reports.len(),
        reports.len() - fixable
    );
    if !fix || fixable == 0 {
        return Ok(());
    }
    if !yes && !confirm()? {
        println!("Aborted.");
        return Ok(());
    }
    let log = cuefix::apply(&reports, &settings.undo_dir)?;
    println!("Fixed {fixable}. Undo log: {}", log.display());
    Ok(())
}

fn build_plan(
    settings: &Settings,
    catalog: &Catalog,
    node_name: &str,
    allow_empty: bool,
) -> Result<(sync::Plan, PathBuf)> {
    let node = settings.node(node_name)?;
    let owned = catalog.owned(node_name)?;
    let mut links: Vec<Link> = Vec::new();
    for system in settings.node_systems(node) {
        let root = settings.system_root(system)?;
        let scan = scan::scan_system(&root)?;
        for issue in &scan.issues {
            eprintln!("warning: {system}: {issue}");
        }
        let projection =
            project::project_system(node.frontend, system, &root, &scan, node.multi_disc)?;
        for w in &projection.warnings {
            eprintln!("warning: {system}: {w}");
        }
        let had_entries = owned.keys().any(|p| p.starts_with(system));
        if projection.links.is_empty() && had_entries && !allow_empty {
            bail!(
                "{system}: library folder {} has no games but the view had some. \
                 Is the share mounted? Re-run with --allow-empty if this is intended.",
                root.display()
            );
        }
        links.extend(projection.links);
    }
    let view_root = settings.view_root(node_name);
    Ok((sync::plan(&view_root, links, &owned)?, view_root))
}

fn print_plan(plan: &sync::Plan, verbose: bool) {
    println!(
        "create {}, replace {}, remove {}, unchanged {}, conflicts {}",
        plan.create.len(),
        plan.replace.len(),
        plan.remove.len(),
        plan.keep.len(),
        plan.conflicts.len()
    );
    let limit = if verbose { usize::MAX } else { 10 };
    let show = |label: &str, items: Vec<String>| {
        for item in items.iter().take(limit) {
            println!("  {label} {item}");
        }
        if items.len() > limit {
            println!("  … {} more (use --verbose)", items.len() - limit);
        }
    };
    show(
        "+",
        plan.create
            .iter()
            .map(|l| l.dst.display().to_string())
            .collect(),
    );
    show(
        "~",
        plan.replace
            .iter()
            .map(|l| l.dst.display().to_string())
            .collect(),
    );
    show(
        "-",
        plan.remove
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
    );
    show("!", plan.conflicts.clone());
}

fn confirm() -> Result<bool> {
    use std::io::Write;
    print!("Apply? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}
