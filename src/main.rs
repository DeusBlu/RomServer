mod auth;
mod catalog;
mod cuefix;
mod emulators;
mod ops;
mod project;
mod scan;
mod settings;
mod sync;
#[cfg(test)]
mod tests;
mod web;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::catalog::Catalog;
use crate::ops::Target;
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
    /// Run the web UI.
    Serve {
        #[arg(long, env = "ROMSERVER_LISTEN", default_value = "0.0.0.0:8080")]
        listen: String,
        /// Login password hash (same format as Lantern's). Delete it to reset the password.
        #[arg(
            long,
            env = "ROMSERVER_PASSWORD_FILE",
            default_value = "/config/data/password.lock"
        )]
        password_file: PathBuf,
    },
    /// Scan library systems and report games, saves and problems. Read-only.
    Scan {
        /// Limit to one system (short name, e.g. psx).
        #[arg(long)]
        system: Option<String>,
        /// List every game.
        #[arg(long)]
        verbose: bool,
    },
    /// Show what `apply` would change in a machine's view. Read-only.
    Plan {
        #[arg(long)]
        node: String,
        #[arg(long)]
        verbose: bool,
    },
    /// Build or update a machine's view.
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
    if let Command::Serve {
        listen,
        password_file,
    } = &cli.command
    {
        // The UI loads settings per request, so it can start (and help fix) a broken file.
        return tokio::runtime::Runtime::new()?.block_on(web::serve(
            cli.config.clone(),
            listen,
            password_file.clone(),
        ));
    }
    let settings = Settings::load(&cli.config)?;
    match cli.command {
        Command::Serve { .. } => unreachable!(),
        Command::Scan { system, verbose } => cmd_scan(&settings, system.as_deref(), verbose),
        Command::Cues { system, fix, yes } => cmd_cues(&settings, system.as_deref(), fix, yes),
        Command::Plan { node, verbose } => {
            let catalog = Catalog::open(&settings.database)?;
            let np = ops::plan_node(
                &settings,
                &catalog,
                &node,
                Target::Settings { allow_empty: false },
            )?;
            print_plan(&np, verbose);
            Ok(())
        }
        Command::Apply {
            node,
            yes,
            allow_empty,
        } => {
            let mut catalog = Catalog::open(&settings.database)?;
            let np = ops::plan_node(&settings, &catalog, &node, Target::Settings { allow_empty })?;
            print_plan(&np, false);
            if np.plan.is_noop() {
                println!("Nothing to do.");
                return Ok(());
            }
            if !yes && !confirm()? {
                println!("Aborted.");
                return Ok(());
            }
            sync::apply(&mut catalog, &node, &np.view_root, &np.plan)?;
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
        let s = ops::summarise_system(settings, system)?;
        println!(
            "{system} ({}): {} games ({} multi-disc), {} saves, {} ignored, {} issues",
            s.root.display(),
            s.games,
            s.multi_disc,
            s.saves,
            s.ignored,
            s.issues.len()
        );
        for issue in &s.issues {
            println!("  ! {issue}");
        }
        if verbose {
            for g in scan::scan_system(&s.root)?.games {
                println!("  {}", g.name());
            }
        }
    }
    Ok(())
}

fn cmd_cues(settings: &Settings, only: Option<&str>, fix: bool, yes: bool) -> Result<()> {
    let reports = ops::check_cues(settings, only)?;
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

fn print_plan(np: &ops::NodePlan, verbose: bool) {
    for w in &np.warnings {
        eprintln!("warning: {w}");
    }
    let plan = &np.plan;
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
    let paths = |links: &[project::Link]| -> Vec<String> {
        links.iter().map(|l| l.dst.display().to_string()).collect()
    };
    show("+", paths(&plan.create));
    show("~", paths(&plan.replace));
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
