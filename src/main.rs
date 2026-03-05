use std::cmp::Reverse;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeDelta};
use clap::Parser;
use colored::Colorize;
use humansize::{format_size, BINARY};

// ─── CLI Arguments ───────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "copilot-clean", about = "Clean up old sessions, logs, and versions from ~/.copilot")]
struct Cli {
    /// Number of days after which sessions and logs are considered old
    #[arg(long, default_value_t = 7)]
    days: u64,

    /// Number of latest versions to keep per platform in pkg/
    #[arg(long, default_value_t = 2)]
    keep_versions: usize,

    /// Show what would be removed without actually removing anything
    #[arg(long)]
    dry_run: bool,

    /// Path to the .copilot directory (defaults to ~/.copilot)
    #[arg(long)]
    copilot_dir: Option<PathBuf>,
}

// ─── Cleanup Item ────────────────────────────────────────────────────────────

#[derive(Debug)]
struct CleanupItem {
    path: PathBuf,
    category: &'static str,
    reason: String,
    size: u64,
    modified: DateTime<Local>,
}

// ─── Y/N/A Confirmation ─────────────────────────────────────────────────────

enum Confirm {
    Yes,
    No,
    All,
}

fn ask_confirm(prompt: &str) -> Confirm {
    loop {
        print!("{} {} ", prompt, "[Y/N/A]".dimmed());
        io::stdout().flush().ok();
        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            return Confirm::No;
        }
        match input.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Confirm::Yes,
            "n" | "no" => return Confirm::No,
            "a" | "all" => return Confirm::All,
            _ => println!("{}", "  Please enter Y (yes), N (no), or A (all).".yellow()),
        }
    }
}

// ─── Scanner ─────────────────────────────────────────────────────────────────

fn dir_size(path: &Path) -> u64 {
    if path.is_file() {
        return path.metadata().map(|m| m.len()).unwrap_or(0);
    }
    fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| {
            let p = e.path();
            if p.is_dir() { dir_size(&p) } else { p.metadata().map(|m| m.len()).unwrap_or(0) }
        })
        .sum()
}

fn modified_time(path: &Path) -> Result<DateTime<Local>> {
    let meta = path.metadata().context("Failed to read metadata")?;
    let modified: DateTime<Local> = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH).into();
    Ok(modified)
}

fn scan_sessions(copilot_dir: &Path, max_age: TimeDelta) -> Result<Vec<CleanupItem>> {
    let session_dir = copilot_dir.join("session-state");
    if !session_dir.exists() {
        return Ok(vec![]);
    }
    let cutoff = Local::now() - max_age;
    let mut items = Vec::new();

    for entry in fs::read_dir(&session_dir).context("Failed to read session-state/")? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let modified = modified_time(&path).unwrap_or(DateTime::UNIX_EPOCH.into());
        if modified < cutoff {
            let size = dir_size(&path);
            items.push(CleanupItem {
                path,
                category: "Session",
                reason: format!("Last modified {}", modified.format("%Y-%m-%d %H:%M")),
                size,
                modified,
            });
        }
    }
    items.sort_by_key(|i| i.modified);
    Ok(items)
}

fn scan_logs(copilot_dir: &Path, max_age: TimeDelta) -> Result<Vec<CleanupItem>> {
    let log_dir = copilot_dir.join("logs");
    if !log_dir.exists() {
        return Ok(vec![]);
    }
    let cutoff = Local::now() - max_age;
    let mut items = Vec::new();

    for entry in fs::read_dir(&log_dir).context("Failed to read logs/")? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let modified = modified_time(&path).unwrap_or(DateTime::UNIX_EPOCH.into());
        if modified < cutoff {
            let size = path.metadata().map(|m| m.len()).unwrap_or(0);
            items.push(CleanupItem {
                path,
                category: "Log",
                reason: format!("Last modified {}", modified.format("%Y-%m-%d %H:%M")),
                size,
                modified,
            });
        }
    }
    items.sort_by_key(|i| i.modified);
    Ok(items)
}

/// Parse a version string like "0.0.421" or "0.0.421-1" into a sortable tuple.
fn parse_version(name: &str) -> (u64, u64, u64, String) {
    let (base, pre) = match name.split_once('-') {
        Some((b, p)) => (b, p.to_string()),
        None => (name, String::new()),
    };
    let parts: Vec<u64> = base.split('.').filter_map(|s| s.parse().ok()).collect();
    (
        parts.first().copied().unwrap_or(0),
        parts.get(1).copied().unwrap_or(0),
        parts.get(2).copied().unwrap_or(0),
        pre,
    )
}

fn scan_old_versions(copilot_dir: &Path, keep: usize) -> Result<Vec<CleanupItem>> {
    let pkg_dir = copilot_dir.join("pkg");
    if !pkg_dir.exists() {
        return Ok(vec![]);
    }
    let mut items = Vec::new();

    // Iterate platform subdirectories (e.g. universal, win32-x64)
    for platform_entry in fs::read_dir(&pkg_dir).context("Failed to read pkg/")? {
        let platform_entry = platform_entry?;
        let platform_path = platform_entry.path();
        if !platform_path.is_dir() {
            continue;
        }
        // Skip tmp directory
        if platform_path.file_name().is_some_and(|n| n == "tmp") {
            continue;
        }

        let mut versions: Vec<(PathBuf, String)> = Vec::new();
        for ver_entry in fs::read_dir(&platform_path)? {
            let ver_entry = ver_entry?;
            let ver_path = ver_entry.path();
            if ver_path.is_dir() {
                let name = ver_entry.file_name().to_string_lossy().to_string();
                versions.push((ver_path, name));
            }
        }

        // Sort by version descending (newest first)
        versions.sort_by(|a, b| {
            let va = parse_version(&a.1);
            let vb = parse_version(&b.1);
            vb.cmp(&va)
        });

        // Keep the newest `keep` versions, mark the rest for removal
        let platform_name = platform_path.file_name().unwrap_or_default().to_string_lossy();
        for (path, name) in versions.into_iter().skip(keep) {
            let modified = modified_time(&path).unwrap_or(DateTime::UNIX_EPOCH.into());
            let size = dir_size(&path);
            items.push(CleanupItem {
                path,
                category: "Old version",
                reason: format!("{platform_name}/{name} (keeping latest {keep})"),
                size,
                modified,
            });
        }
    }
    items.sort_by_key(|i| Reverse(i.size));
    Ok(items)
}

// ─── Removal ─────────────────────────────────────────────────────────────────

fn remove_to_trash(path: &Path) -> Result<()> {
    trash::delete(path).with_context(|| format!("Failed to move to recycle bin: {}", path.display()))
}

// ─── Main ────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = Cli::parse();
    let copilot_dir = cli.copilot_dir.unwrap_or_else(|| {
        dirs::home_dir()
            .expect("Could not determine home directory")
            .join(".copilot")
    });

    if !copilot_dir.exists() {
        println!("{}", "~/.copilot directory not found. Nothing to clean.".yellow());
        return Ok(());
    }

    println!(
        "{} {}",
        "Scanning".bold().cyan(),
        copilot_dir.display().to_string().dimmed()
    );

    let max_age = TimeDelta::days(cli.days as i64);

    // Gather all cleanup candidates
    let sessions = scan_sessions(&copilot_dir, max_age)?;
    let logs = scan_logs(&copilot_dir, max_age)?;
    let versions = scan_old_versions(&copilot_dir, cli.keep_versions)?;

    let total_items = sessions.len() + logs.len() + versions.len();
    if total_items == 0 {
        println!("{}", "✓ Nothing to clean up!".green().bold());
        return Ok(());
    }

    // Print summary
    let total_size: u64 = sessions.iter().chain(&logs).chain(&versions).map(|i| i.size).sum();
    println!();
    println!(
        "Found {} item(s) to clean ({}):",
        total_items.to_string().bold(),
        format_size(total_size, BINARY).yellow()
    );
    if !sessions.is_empty() {
        println!("  {} old session(s)", sessions.len().to_string().bold());
    }
    if !logs.is_empty() {
        println!("  {} old log file(s)", logs.len().to_string().bold());
    }
    if !versions.is_empty() {
        println!("  {} old version(s)", versions.len().to_string().bold());
    }
    println!();

    if cli.dry_run {
        println!("{}", "── Dry run ─ no files will be removed ──".yellow().bold());
        println!();
    }

    let all_items: Vec<CleanupItem> = sessions.into_iter().chain(logs).chain(versions).collect();
    let mut auto_yes = false;
    let mut removed_count = 0u64;
    let mut removed_size = 0u64;

    for item in &all_items {
        let label = match item.category {
            "Session" => item.category.blue(),
            "Log" => item.category.magenta(),
            "Old version" => item.category.red(),
            _ => item.category.normal(),
        };
        let display_name = item
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        println!(
            "  [{}] {} ({})",
            label,
            display_name.bold(),
            format_size(item.size, BINARY).dimmed()
        );
        println!("        {}", item.reason.dimmed());

        if cli.dry_run {
            removed_count += 1;
            removed_size += item.size;
            continue;
        }

        let should_remove = if auto_yes {
            true
        } else {
            match ask_confirm("  Remove?") {
                Confirm::Yes => true,
                Confirm::No => false,
                Confirm::All => {
                    auto_yes = true;
                    true
                }
            }
        };

        if should_remove {
            match remove_to_trash(&item.path) {
                Ok(()) => {
                    println!("        {}", "→ Moved to recycle bin".green());
                    removed_count += 1;
                    removed_size += item.size;
                }
                Err(e) => {
                    println!("        {} {e}", "✗ Error:".red().bold());
                }
            }
        } else {
            println!("        {}", "→ Skipped".dimmed());
        }
    }

    println!();
    if cli.dry_run {
        println!(
            "{} Would remove {} item(s) ({})",
            "Dry run:".yellow().bold(),
            removed_count.to_string().bold(),
            format_size(removed_size, BINARY).yellow()
        );
    } else {
        println!(
            "{} Removed {} item(s) ({}) to recycle bin",
            "Done!".green().bold(),
            removed_count.to_string().bold(),
            format_size(removed_size, BINARY).yellow()
        );
    }

    Ok(())
}
