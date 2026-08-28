//! Local per-project Claude Code usage from transcript JSONL files.
//!
//! Claude's usage API exposes aggregate plan utilization only, so this module
//! estimates each project's relative share from local assistant events. The
//! rolling 5-hour and 7-day windows are an approximation of Anthropic's
//! server-side windows and are not an absolute mapping to the plan percentage.

mod scan;
mod types;

use std::io::Write;

pub use scan::{ScanResult, scan};
pub use types::{ProjectUsage, ProjectsReport, UsageWindow, Windows};

/// Render the project report as two compact terminal tables.
pub fn print_pretty(w: &mut impl Write, report: &ProjectsReport) -> std::io::Result<()> {
    print_window(w, "Session 5h", &report.windows.session)?;
    writeln!(w)?;
    print_window(w, "Week 7d", &report.windows.weekly)
}

fn print_window(w: &mut impl Write, title: &str, window: &UsageWindow) -> std::io::Result<()> {
    writeln!(w, "{title}")?;
    writeln!(
        w,
        "{:<32} {:>7} {:>12} {:>10} {:>10} {:>10} {:>10}  profiles",
        "label", "share%", "weighted", "in", "out", "cache_w", "cache_r"
    )?;
    for project in &window.projects {
        writeln!(
            w,
            "{:<32} {:>6.1}% {:>12} {:>10} {:>10} {:>10} {:>10}  {}",
            project.label,
            project.share_pct,
            format_weighted(project.weighted_tokens),
            project.input_tokens,
            project.output_tokens,
            project.cache_creation_tokens,
            project.cache_read_tokens,
            project.profiles.join(",")
        )?;
    }
    Ok(())
}

fn format_weighted(value: f64) -> String {
    if value.fract().abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        format!("{value:.2}").trim_end_matches('0').to_string()
    }
}
