//! Gemini Code Assist bar text and all-model quota tooltip.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::countdown;
use crate::format::{placeholders, substitute, updated_at_hm};
use crate::pacing;
use crate::pango::{self, color_span, escape, severity_color, severity_for};
use crate::theme::Theme;
use crate::tooltip::{Line as TooltipLine, render_bordered};
use crate::usage::{GeminiBucket, GeminiSnapshot, gemini_severity};
use crate::vendor::{RenderOpts, VendorOutcome};
use crate::waybar::{Class, WaybarOutput};

use super::fetch::FetchOutcome;

pub const DEFAULT_FORMAT: &str = "{gemini_pro_pct}% · {gemini_pro_reset}";

pub fn pro_bucket(snapshot: &GeminiSnapshot) -> Option<&GeminiBucket> {
    snapshot
        .buckets
        .iter()
        .filter(|bucket| bucket.model.starts_with("gemini-") && bucket.model.contains("pro"))
        .max_by_key(|bucket| bucket.window.utilization_pct)
}

pub fn build_placeholders(
    snapshot: &GeminiSnapshot,
    now: DateTime<Utc>,
) -> HashMap<&'static str, String> {
    let pro = pro_bucket(snapshot);
    let pct = pro.map_or(0, |bucket| bucket.window.utilization_pct);
    let reset = pro
        .map(|bucket| countdown::format(bucket.window.resets_at, now))
        .unwrap_or_else(|| "—".into());
    placeholders(vec![
        ("icon", "󰊭".into()),
        ("vendor_short", "gem".into()),
        ("plan", snapshot.tier.clone()),
        ("session_pct", pct.to_string()),
        ("session_reset", reset.clone()),
        ("weekly_pct", pct.to_string()),
        ("weekly_reset", reset.clone()),
        ("gemini_tier", snapshot.tier.clone()),
        (
            "gemini_pro_model",
            pro.map(|bucket| bucket.model.clone())
                .unwrap_or_else(|| "—".into()),
        ),
        ("gemini_pro_pct", pct.to_string()),
        ("gemini_pro_reset", reset),
    ])
}

pub fn render(
    outcome: &VendorOutcome,
    snapshot: &GeminiSnapshot,
    theme: &Theme,
    opts: &RenderOpts,
    now: DateTime<Utc>,
) -> WaybarOutput {
    let severity = gemini_severity(snapshot);
    let format = opts.format.clone().unwrap_or_else(|| DEFAULT_FORMAT.into());
    let values = build_placeholders(snapshot, now);
    let mut safe_values = values.clone();
    for key in ["plan", "gemini_tier", "gemini_pro_model"] {
        if let Some(value) = safe_values.get_mut(key) {
            *value = escape(value);
        }
    }

    let mut text = substitute(&format, &safe_values);
    if outcome.stale {
        text.push_str(" ⏸");
    }
    let icon = opts
        .icon
        .as_deref()
        .filter(|icon| !icon.is_empty())
        .map(|icon| format!("{icon} "))
        .unwrap_or_default();
    let text = color_span(severity_color(severity, theme), &format!("{icon}{text}"));
    let tooltip = opts
        .tooltip_format
        .as_deref()
        .map(|format| substitute(format, &safe_values))
        .unwrap_or_else(|| render_tooltip(outcome, snapshot, theme, opts, now));

    WaybarOutput {
        text,
        tooltip,
        class: Class::from(severity),
    }
}

fn render_tooltip(
    outcome: &VendorOutcome,
    snapshot: &GeminiSnapshot,
    theme: &Theme,
    opts: &RenderOpts,
    now: DateTime<Utc>,
) -> String {
    let mut lines = vec![
        TooltipLine::Center(format!(
            "<span font_weight='bold' foreground='{blue}'>Gemini Code Assist</span>",
            blue = theme.blue
        )),
        TooltipLine::Sep,
        TooltipLine::Body("".into()),
        TooltipLine::Body(format!(
            " <span foreground='{fg}'>  󰣖  Tier</span>  <span font_weight='bold'>{tier}</span>",
            fg = theme.fg,
            tier = escape(&snapshot.tier)
        )),
    ];

    for bucket in &snapshot.buckets {
        lines.push(TooltipLine::Body("".into()));
        push_bucket(&mut lines, bucket, theme, opts, now);
    }
    if snapshot.buckets.is_empty() {
        lines.push(TooltipLine::Body(format!(
            " <span foreground='{dim}'>  No REQUESTS quota buckets reported.</span>",
            dim = theme.dim
        )));
    }

    if let Some((code, message)) = outcome.last_error.as_ref() {
        let label = if *code == 0 {
            "Gemini error".into()
        } else {
            format!("HTTP {code}")
        };
        lines.push(TooltipLine::Body("".into()));
        lines.push(TooltipLine::Sep);
        lines.push(TooltipLine::Body(format!(
            " <span foreground='{red}'>  󰅚  {label}</span>",
            red = theme.red
        )));
        lines.push(TooltipLine::Body(format!(
            " <span foreground='{dim}'>     {message}</span>",
            dim = theme.dim,
            message = escape(message)
        )));
    }

    lines.push(TooltipLine::Body("".into()));
    lines.push(TooltipLine::Sep);
    lines.push(TooltipLine::Body(format!(
        " <span foreground='{dim}'>  󰅐  Updated {updated}</span>",
        dim = theme.dim,
        updated = updated_at_hm(now, outcome.cache_age)
    )));
    render_bordered(&lines, theme)
}

fn push_bucket(
    lines: &mut Vec<TooltipLine>,
    bucket: &GeminiBucket,
    theme: &Theme,
    opts: &RenderOpts,
    now: DateTime<Utc>,
) {
    let pct = bucket.window.utilization_pct;
    let color = severity_color(severity_for(pct), theme);
    let pace = pacing::calc(
        pct,
        bucket.window.resets_at,
        now,
        bucket.window.window_duration,
        opts.pace_tolerance,
    );
    let marker = opts.tooltip_pace_pts.then_some(pace.elapsed_pct);
    let bar = pango::progress_bar(pct, color, theme, marker);
    let pace_glyph = if opts.tooltip_pace_pts {
        pace.point_pace.glyph()
    } else {
        pace.ratio_pace.glyph()
    };
    lines.push(TooltipLine::Body(format!(
        " <span foreground='{fg}'>  󰛨  {model}</span>",
        fg = theme.fg,
        model = escape(&bucket.model)
    )));
    lines.push(TooltipLine::Body(format!(
        "   {bar}  <span font_weight='bold' foreground='{color}'>{pct}%</span>  {pace_glyph}"
    )));
    lines.push(TooltipLine::Body(format!(
        " <span foreground='{dim}'>  ⏱  Resets in {reset}</span>",
        dim = theme.dim,
        reset = escape(&countdown::format(bucket.window.resets_at, now))
    )));
}

impl From<FetchOutcome> for VendorOutcome {
    fn from(outcome: FetchOutcome) -> Self {
        Self {
            snapshot: crate::usage::VendorSnapshot::Gemini(outcome.snapshot),
            stale: outcome.stale,
            last_error: outcome.last_error,
            cache_age: outcome.cache_age,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::UsageWindow;
    use chrono::TimeZone;
    use std::time::Duration;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 21, 12, 0, 0).unwrap()
    }

    fn sample() -> GeminiSnapshot {
        GeminiSnapshot {
            tier: "standard-tier".into(),
            buckets: vec![
                GeminiBucket {
                    model: "gemini-2.0-flash".into(),
                    window: UsageWindow {
                        utilization_pct: 80,
                        resets_at: Some(now() + chrono::Duration::hours(20)),
                        window_duration: chrono::Duration::hours(24),
                    },
                },
                GeminiBucket {
                    model: "gemini-2.5-pro".into(),
                    window: UsageWindow {
                        utilization_pct: 30,
                        resets_at: Some(now() + chrono::Duration::hours(10)),
                        window_duration: chrono::Duration::hours(24),
                    },
                },
                GeminiBucket {
                    model: "gemini-3-pro-preview".into(),
                    window: UsageWindow {
                        utilization_pct: 60,
                        resets_at: Some(now() + chrono::Duration::hours(8)),
                        window_duration: chrono::Duration::hours(24),
                    },
                },
            ],
        }
    }

    fn outcome(snapshot: GeminiSnapshot) -> VendorOutcome {
        VendorOutcome {
            snapshot: crate::usage::VendorSnapshot::Gemini(snapshot),
            stale: false,
            last_error: None,
            cache_age: Some(Duration::from_secs(5)),
        }
    }

    fn opts() -> RenderOpts {
        RenderOpts {
            format: None,
            tooltip_format: None,
            icon: None,
            pace_tolerance: 5,
            format_pace_color: false,
            tooltip_pace_pts: false,
        }
    }

    #[test]
    fn default_bar_uses_worst_pro_bucket_not_worst_overall() {
        let snapshot = sample();
        let values = build_placeholders(&snapshot, now());
        assert_eq!(values["gemini_pro_model"], "gemini-3-pro-preview");
        assert_eq!(values["gemini_pro_pct"], "60");
        assert_eq!(values["gemini_pro_reset"], "8h 00m");
        let rendered = render(
            &outcome(snapshot.clone()),
            &snapshot,
            &Theme::default(),
            &opts(),
            now(),
        );
        assert!(rendered.text.contains("60% · 8h 00m"));
        assert_eq!(rendered.class, Class::High);
    }

    #[test]
    fn tooltip_lists_every_bucket_and_tier() {
        let snapshot = sample();
        let rendered = render(
            &outcome(snapshot.clone()),
            &snapshot,
            &Theme::default(),
            &opts(),
            now(),
        );
        assert!(rendered.tooltip.contains("standard-tier"));
        for bucket in snapshot.buckets {
            assert!(rendered.tooltip.contains(&bucket.model));
        }
    }
}
