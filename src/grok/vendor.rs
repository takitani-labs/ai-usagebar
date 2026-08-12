//! Grok renderer — bar text + bordered Pango tooltip.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::countdown;
use crate::format::{placeholders, substitute, updated_at_hm};
use crate::pacing::PaceSeverity;
use crate::pango::{color_span, escape, severity_color, severity_for};
use crate::theme::Theme;
use crate::tooltip::{Line as TooltipLine, render_bordered};
use crate::usage::GrokSnapshot;
use crate::vendor::{RenderOpts, VendorOutcome};
use crate::waybar::{Class, WaybarOutput};

use super::fetch::{
    EXPIRED_TOKEN_MESSAGE, FetchOutcome, SCHEMA_DRIFT_MESSAGE, SPENDING_BLOCK_MESSAGE,
};

/// Presentation classification for the legacy `(u16, String)` cached
/// diagnostic. Code zero has never meant HTTP, so the stable markers let the
/// tooltip separate schema drift and a lapsed token from other local failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarningKind {
    /// The CLI's log shows turns being refused for spending. Checked before
    /// the generic HTTP arm: it carries 403 to say what upstream answered, but
    /// it is a standing state, not one failed fetch.
    SpendingBlock,
    Http(u16),
    SchemaDrift,
    ExpiredToken,
    Other,
}

pub fn warning_kind(code: u16, message: &str) -> WarningKind {
    if message == SPENDING_BLOCK_MESSAGE {
        WarningKind::SpendingBlock
    } else if code != 0 {
        WarningKind::Http(code)
    } else if message == SCHEMA_DRIFT_MESSAGE {
        WarningKind::SchemaDrift
    } else if message == EXPIRED_TOKEN_MESSAGE {
        WarningKind::ExpiredToken
    } else {
        WarningKind::Other
    }
}

/// Whether this outcome carries a standing spending refusal.
pub fn is_blocked(outcome: &VendorOutcome) -> bool {
    outcome
        .last_error
        .as_ref()
        .is_some_and(|(code, msg)| warning_kind(*code, msg) == WarningKind::SpendingBlock)
}

pub const DEFAULT_FORMAT: &str = "{grok_monthly_pct}%";
/// A percentage needs a denominator. Accounts whose tier is not metered in
/// credits have none, so the bar falls back to the one number still moving:
/// what was spent this billing period.
pub const DEFAULT_FORMAT_NO_POOL: &str = "{grok_monthly_used} credits";

/// Credits are billed fractionally but almost always land on whole numbers;
/// print them the way the Grok Build UI does instead of as `4000.0`.
pub fn credits(value: f64) -> String {
    if (value.fract()).abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// xAI publishes no tier name anywhere in the API (the JWT carries an opaque
/// `tier: N`), so the monthly allowance stands in for the plan label: it is the
/// number that actually differs between subscriptions.
///
/// Tiers without a pool have no such number, and "0 credits" would read as an
/// allowance spent down to nothing rather than one that never existed. Those
/// fall back to the entitlement, which is the only other thing xAI will say
/// about the account.
pub fn plan_label(snap: &GrokSnapshot) -> String {
    if snap.has_pool() {
        return format!("{} credits", credits(snap.monthly_limit));
    }
    match snap.has_grok_code_access {
        Some(true) => "Grok Code, no pool".to_string(),
        Some(false) => "no Grok Code access".to_string(),
        None => "no credit pool".to_string(),
    }
}

pub fn build_placeholders(
    snap: &GrokSnapshot,
    now: DateTime<Utc>,
) -> HashMap<&'static str, String> {
    let plan = plan_label(snap);
    // An absent pool yields an empty percentage rather than "0": a format that
    // asks for one on an unmetered account should come out visibly blank, not
    // quietly wrong.
    let monthly_pct = snap
        .monthly_pct()
        .map(|p| p.to_string())
        .unwrap_or_default();
    let monthly_reset = countdown::format(snap.period_end, now);
    placeholders(vec![
        ("icon", "󰚩".to_string()),
        ("vendor_short", "grk".to_string()),
        // Cross-vendor aliases. Grok has no rolling window, and reporting an
        // absent window as 0% would read as "nothing used in the last 5h".
        ("plan", plan.clone()),
        ("weekly_pct", monthly_pct.clone()),
        ("weekly_reset", monthly_reset.clone()),
        ("session_pct", String::new()),
        ("session_reset", String::new()),
        // Grok-specific placeholders.
        ("grok_plan", plan),
        ("grok_monthly_pct", monthly_pct),
        ("grok_monthly_used", credits(snap.monthly_used)),
        ("grok_monthly_limit", credits(snap.monthly_limit)),
        ("grok_monthly_remaining", credits(snap.monthly_remaining)),
        ("grok_monthly_reset", monthly_reset),
        ("grok_on_demand_cap", credits(snap.on_demand_cap)),
        // Markers, in the shape of {grok_blocked}: empty unless they apply, so
        // a consumer can branch on them without parsing prose.
        (
            "grok_nopool",
            if snap.has_pool() { "" } else { "nopool" }.to_string(),
        ),
        (
            "grok_access",
            match snap.has_grok_code_access {
                Some(true) => "yes",
                Some(false) => "no",
                None => "",
            }
            .to_string(),
        ),
    ])
}

pub fn severity(snap: &GrokSnapshot) -> PaceSeverity {
    match snap.monthly_pct() {
        Some(pct) => severity_for(pct),
        // Nothing to pace against. This must not fall to Low: Low is the green
        // that reported calm headroom for two days while the counter sat frozen
        // and the pool read zero. An entitlement xAI actively denies is worth
        // more than a warning, since no amount of waiting fixes it.
        None if snap.has_grok_code_access == Some(false) => PaceSeverity::Critical,
        None => PaceSeverity::Mid,
    }
}

pub fn render(
    outcome: &VendorOutcome,
    snap: &GrokSnapshot,
    theme: &Theme,
    opts: &RenderOpts,
    now: DateTime<Utc>,
) -> WaybarOutput {
    // A refusal outranks the pool: 36% of the credits left means nothing while
    // every turn comes back 403, so the row goes critical regardless of pace.
    let blocked = is_blocked(outcome);
    let class = if blocked {
        Class::Critical
    } else {
        Class::from(severity(snap))
    };
    let format = opts.format.clone().unwrap_or_else(|| {
        if snap.has_pool() {
            DEFAULT_FORMAT
        } else {
            DEFAULT_FORMAT_NO_POOL
        }
        .to_string()
    });
    let mut values = build_placeholders(snap, now);
    values.insert("grok_blocked", if blocked { "blocked" } else { "" }.into());
    // User formats are Pango markup after Waybar renders them. Escape API
    // strings there, while retaining raw values for the default tooltip (which
    // escapes exactly once at its markup insertion point).
    let mut pango_values = values.clone();
    for key in ["plan", "grok_plan"] {
        if let Some(value) = pango_values.get_mut(key) {
            *value = escape(value);
        }
    }

    let mut text = substitute(&format, &pango_values);
    if outcome.stale {
        text.push_str(" ⏸");
    }

    let wrapper_color = severity_color(severity(snap), theme).to_string();
    let icon_prefix = match opts.icon.as_deref() {
        Some(ic) if !ic.is_empty() => format!("{ic} "),
        _ => String::new(),
    };
    let bar_text = color_span(&wrapper_color, &format!("{icon_prefix}{text}"));

    let tooltip = if let Some(fmt) = opts.tooltip_format.as_deref() {
        substitute(fmt, &pango_values)
    } else {
        render_tooltip(outcome, snap, theme, now)
    };

    WaybarOutput {
        text: bar_text,
        tooltip,
        class,
    }
}

fn render_tooltip(
    outcome: &VendorOutcome,
    snap: &GrokSnapshot,
    theme: &Theme,
    now: DateTime<Utc>,
) -> String {
    let blue = &theme.blue;
    let dim = &theme.dim;
    let fg = &theme.fg;

    let monthly_color = severity_color(severity(snap), theme);

    let mut lines: Vec<TooltipLine> = Vec::new();
    lines.push(TooltipLine::Center(format!(
        "<span font_weight='bold' foreground='{blue}'>Grok Build</span>"
    )));
    lines.push(TooltipLine::Sep);
    lines.push(TooltipLine::Body("".into()));

    lines.push(TooltipLine::Body(format!(
        " <span foreground='{fg}'>  󰣖  Plan</span>"
    )));
    lines.push(TooltipLine::Body(format!(
        "   <span font_weight='bold' foreground='{monthly_color}'>{}</span>",
        escape(&plan_label(snap))
    )));

    lines.push(TooltipLine::Body("".into()));
    let reset = escape(&countdown::format(snap.period_end, now));
    match snap.monthly_pct() {
        Some(monthly_pct) => {
            lines.push(TooltipLine::Body(format!(
                " <span foreground='{fg}'>  󰅄  Monthly credits</span>"
            )));
            lines.push(TooltipLine::Body(format!(
                "   <span font_weight='bold' foreground='{monthly_color}'>{used} / {limit}</span>  ({monthly_pct}%)",
                used = credits(snap.monthly_used),
                limit = credits(snap.monthly_limit)
            )));
            lines.push(TooltipLine::Body(format!(
                " <span foreground='{dim}'>     {remaining} remaining · reset {reset}</span>",
                remaining = credits(snap.monthly_remaining),
            )));
        }
        // No denominator: report the spend and say plainly that there is
        // nothing to measure it against, rather than implying a full tank.
        None => {
            lines.push(TooltipLine::Body(format!(
                " <span foreground='{fg}'>  󰅄  Credits this period</span>"
            )));
            lines.push(TooltipLine::Body(format!(
                "   <span font_weight='bold' foreground='{monthly_color}'>{used} used</span>",
                used = credits(snap.monthly_used),
            )));
            lines.push(TooltipLine::Body(format!(
                " <span foreground='{dim}'>     no included allowance · reset {reset}</span>"
            )));

            let (icon, ecolor, detail) = match snap.has_grok_code_access {
                Some(true) => (
                    "󰀪",
                    theme.orange.as_str(),
                    "active · this tier is not metered in credits",
                ),
                Some(false) => (
                    "󰅚",
                    theme.red.as_str(),
                    "none · the account cannot run Grok Code",
                ),
                None => (
                    "󰀪",
                    theme.orange.as_str(),
                    "unknown · /v1/user did not answer",
                ),
            };
            lines.push(TooltipLine::Body("".into()));
            lines.push(TooltipLine::Body(format!(
                " <span foreground='{ecolor}'>  {icon}  Grok Code access</span>"
            )));
            lines.push(TooltipLine::Body(format!(
                " <span foreground='{dim}'>     {detail}</span>"
            )));
        }
    }

    if snap.on_demand_cap > 0.0 {
        lines.push(TooltipLine::Body("".into()));
        lines.push(TooltipLine::Body(format!(
            " <span foreground='{fg}'>  󰆼  On-demand cap</span>"
        )));
        lines.push(TooltipLine::Body(format!(
            "   <span font_weight='bold' foreground='{fg}'>{}</span>",
            credits(snap.on_demand_cap)
        )));
    }

    if let Some((code, msg)) = outcome.last_error.as_ref() {
        let (label, icon, ecolor) = match warning_kind(*code, msg) {
            WarningKind::SpendingBlock => {
                ("Grok spending limit".to_string(), "󰅚", theme.red.as_str())
            }
            WarningKind::SchemaDrift => {
                ("Grok API schema drift".to_string(), "󰅚", theme.red.as_str())
            }
            WarningKind::ExpiredToken => {
                ("Grok token expired".to_string(), "󰀪", theme.orange.as_str())
            }
            WarningKind::Other => ("Grok error".to_string(), "󰅚", theme.red.as_str()),
            WarningKind::Http(code) if code >= 500 => {
                (format!("HTTP {code}"), "󰅚", theme.red.as_str())
            }
            WarningKind::Http(code) => (format!("HTTP {code}"), "󰀪", theme.orange.as_str()),
        };
        lines.push(TooltipLine::Body("".into()));
        lines.push(TooltipLine::Sep);
        lines.push(TooltipLine::Body(format!(
            " <span foreground='{ecolor}'>  {icon}  {label}</span>"
        )));
        if msg != &label {
            lines.push(TooltipLine::Body(format!(
                "     <span foreground='{dim}'>{}</span>",
                escape(msg)
            )));
        }
    }

    let updated = updated_at_hm(now, outcome.cache_age);
    lines.push(TooltipLine::Body("".into()));
    lines.push(TooltipLine::Sep);
    lines.push(TooltipLine::Body(format!(
        " <span foreground='{dim}'>  󰅐  Updated {updated}</span>"
    )));

    render_bordered(&lines, theme)
}

impl From<FetchOutcome> for VendorOutcome {
    fn from(o: FetchOutcome) -> Self {
        Self {
            snapshot: crate::usage::VendorSnapshot::Grok(o.snapshot),
            stale: o.stale,
            last_error: o.last_error,
            cache_age: o.cache_age,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap()
    }

    fn sample_snap() -> GrokSnapshot {
        GrokSnapshot {
            monthly_limit: 4000.0,
            monthly_used: 1200.0,
            monthly_remaining: 2800.0,
            on_demand_cap: 0.0,
            period_start: Some(Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap()),
            period_end: Some(Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap()),
            has_grok_code_access: Some(true),
        }
    }

    fn sample_outcome(snap: GrokSnapshot) -> VendorOutcome {
        VendorOutcome {
            snapshot: crate::usage::VendorSnapshot::Grok(snap),
            stale: false,
            last_error: None,
            cache_age: None,
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
    fn bar_text_defaults_to_the_monthly_percentage() {
        let snap = sample_snap();
        let out = render(
            &sample_outcome(snap.clone()),
            &snap,
            &Theme::default(),
            &opts(),
            now(),
        );
        assert!(out.text.contains("30%"), "got {}", out.text);
    }

    #[test]
    fn plan_label_reports_the_monthly_allowance() {
        assert_eq!(plan_label(&sample_snap()), "4000 credits");
    }

    /// The tier-7 shape read off the live account on 2026-08-12: entitled,
    /// spending, and no pool for any of it to be measured against.
    fn no_pool_snap(access: Option<bool>) -> GrokSnapshot {
        GrokSnapshot {
            monthly_limit: 0.0,
            monthly_used: 224.0,
            monthly_remaining: 0.0,
            has_grok_code_access: access,
            ..sample_snap()
        }
    }

    #[test]
    fn an_absent_pool_reports_credits_instead_of_a_fabricated_zero() {
        let snap = no_pool_snap(Some(true));
        let out = render(
            &sample_outcome(snap.clone()),
            &snap,
            &Theme::default(),
            &opts(),
            now(),
        );
        assert!(out.text.contains("224 credits"), "got {}", out.text);
        assert!(
            !out.text.contains("0%"),
            "an absent pool must not read as 0% used: {}",
            out.text
        );
    }

    #[test]
    fn an_absent_pool_leaves_the_percentage_placeholders_empty() {
        let values = build_placeholders(&no_pool_snap(Some(true)), now());
        assert_eq!(values.get("grok_monthly_pct").map(String::as_str), Some(""));
        assert_eq!(values.get("weekly_pct").map(String::as_str), Some(""));
        assert_eq!(
            values.get("grok_nopool").map(String::as_str),
            Some("nopool")
        );
        assert_eq!(values.get("grok_access").map(String::as_str), Some("yes"));
    }

    #[test]
    fn an_absent_pool_never_renders_as_calm() {
        // Low is the green that reported headroom for two days while the
        // counter sat frozen. Anything but that.
        assert_eq!(severity(&no_pool_snap(Some(true))), PaceSeverity::Mid);
        assert_eq!(severity(&no_pool_snap(None)), PaceSeverity::Mid);
        assert_eq!(severity(&no_pool_snap(Some(false))), PaceSeverity::Critical);
    }

    #[test]
    fn plan_label_falls_back_to_the_entitlement_without_a_pool() {
        assert_eq!(plan_label(&no_pool_snap(Some(true))), "Grok Code, no pool");
        assert_eq!(
            plan_label(&no_pool_snap(Some(false))),
            "no Grok Code access"
        );
        assert_eq!(plan_label(&no_pool_snap(None)), "no credit pool");
    }

    #[test]
    fn the_tooltip_names_the_missing_allowance_instead_of_printing_a_ratio() {
        let snap = no_pool_snap(Some(true));
        let tip = render_tooltip(
            &sample_outcome(snap.clone()),
            &snap,
            &Theme::default(),
            now(),
        );
        assert!(tip.contains("224 used"), "got {tip}");
        assert!(tip.contains("no included allowance"), "got {tip}");
        assert!(!tip.contains("224 / 0"), "the ratio is the lie: {tip}");
        assert!(tip.contains("Grok Code access"), "got {tip}");
    }

    #[test]
    fn credits_drop_the_decimal_only_when_whole() {
        assert_eq!(credits(4000.0), "4000");
        assert_eq!(credits(12.5), "12.5");
    }

    #[test]
    fn session_aliases_are_blank_because_grok_has_no_rolling_window() {
        let values = build_placeholders(&sample_snap(), now());
        assert_eq!(values.get("session_pct").map(String::as_str), Some(""));
        assert_eq!(values.get("weekly_pct").map(String::as_str), Some("30"));
    }

    #[test]
    fn tooltip_shows_credits_and_the_billing_reset() {
        let snap = sample_snap();
        let tip = render_tooltip(
            &sample_outcome(snap.clone()),
            &snap,
            &Theme::default(),
            now(),
        );
        assert!(tip.contains("Grok Build"));
        assert!(tip.contains("1200 / 4000"));
        assert!(tip.contains("2800 remaining"));
    }

    #[test]
    fn a_spending_block_turns_the_row_critical_whatever_the_pool_says() {
        // The case this exists for: 30% of the pool spent, every turn refused.
        let snap = sample_snap();
        let mut outcome = sample_outcome(snap.clone());
        outcome.last_error = Some((403, SPENDING_BLOCK_MESSAGE.to_string()));
        assert_eq!(severity(&snap), severity_for(30), "pool alone reads calm");

        let out = render(&outcome, &snap, &Theme::default(), &opts(), now());
        assert_eq!(out.class, Class::Critical);
        assert!(out.tooltip.contains("Grok spending limit"));
        assert_eq!(
            warning_kind(403, SPENDING_BLOCK_MESSAGE),
            WarningKind::SpendingBlock,
            "must not fall through to the generic HTTP arm"
        );
    }

    #[test]
    fn the_blocked_placeholder_is_empty_when_nothing_is_refused() {
        let snap = sample_snap();
        let mut o = opts();
        o.format = Some("[{grok_blocked}]".into());
        let out = render(
            &sample_outcome(snap.clone()),
            &snap,
            &Theme::default(),
            &o,
            now(),
        );
        assert!(out.text.contains("[]"), "got {}", out.text);

        let mut blocked = sample_outcome(snap.clone());
        blocked.last_error = Some((403, SPENDING_BLOCK_MESSAGE.to_string()));
        let out = render(&blocked, &snap, &Theme::default(), &o, now());
        assert!(out.text.contains("[blocked]"), "got {}", out.text);
    }

    #[test]
    fn expired_token_warning_names_the_fix() {
        let snap = sample_snap();
        let mut outcome = sample_outcome(snap.clone());
        outcome.last_error = Some((0, EXPIRED_TOKEN_MESSAGE.to_string()));
        let tip = render_tooltip(&outcome, &snap, &Theme::default(), now());
        assert!(tip.contains("Grok token expired"));
        assert_eq!(
            warning_kind(0, EXPIRED_TOKEN_MESSAGE),
            WarningKind::ExpiredToken
        );
    }

    #[test]
    fn plan_is_pango_escaped_in_bar_text() {
        let snap = sample_snap();
        let mut o = opts();
        o.format = Some("{plan}".into());
        let out = render(
            &sample_outcome(snap.clone()),
            &snap,
            &Theme::default(),
            &o,
            now(),
        );
        // Nothing to escape in a plain credit count, but the value must survive.
        assert!(out.text.contains("4000 credits"));
    }
}
