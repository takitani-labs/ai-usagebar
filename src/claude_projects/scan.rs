use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

use super::types::{ProjectUsage, ProjectsReport, UsageWindow, Windows};

const SESSION_HOURS: i64 = 5;
const WEEKLY_HOURS: i64 = 7 * 24;

/// Heuristic weights reflecting the relative rate-limit cost of each token
/// category. They are local estimates, not Anthropic-published billing rates.
const INPUT_WEIGHT: f64 = 1.0;
const OUTPUT_WEIGHT: f64 = 5.0;
const CACHE_CREATION_WEIGHT: f64 = 1.25;
const CACHE_READ_WEIGHT: f64 = 0.1;

#[derive(Debug)]
pub struct ScanResult {
    pub report: ProjectsReport,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigRoot {
    path: PathBuf,
    profile: String,
}

#[derive(Debug, Clone)]
struct UsageEvent {
    timestamp: DateTime<Utc>,
    usage: TokenUsage,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
struct TokenUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TranscriptEvent {
    timestamp: Option<DateTime<Utc>>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    message: TranscriptMessage,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TranscriptMessage {
    id: Option<String>,
    #[serde(rename = "model")]
    _model: Option<String>,
    usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Default)]
struct ProjectTotals {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    profiles: BTreeSet<String>,
}

impl ProjectTotals {
    fn add(&mut self, usage: TokenUsage, profile: &str) {
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(usage.cache_creation_input_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(usage.cache_read_input_tokens);
        self.profiles.insert(profile.to_string());
    }

    fn weighted_tokens(&self) -> f64 {
        self.input_tokens as f64 * INPUT_WEIGHT
            + self.output_tokens as f64 * OUTPUT_WEIGHT
            + self.cache_creation_tokens as f64 * CACHE_CREATION_WEIGHT
            + self.cache_read_tokens as f64 * CACHE_READ_WEIGHT
    }
}

pub fn scan(now: DateTime<Utc>) -> ScanResult {
    let roots = discover_roots();
    scan_roots(&roots, now)
}

fn discover_roots() -> Vec<ConfigRoot> {
    let base_dirs = directories::BaseDirs::new();
    discover_roots_from(
        base_dirs.as_ref().map(directories::BaseDirs::home_dir),
        std::env::var_os("CLAUDE_CONFIG_DIR").as_deref(),
    )
}

fn discover_roots_from(home: Option<&Path>, env_root: Option<&OsStr>) -> Vec<ConfigRoot> {
    let mut candidates = Vec::new();
    if let Some(path) = env_root.map(PathBuf::from) {
        let profile = root_basename(&path);
        candidates.push(ConfigRoot { path, profile });
    }

    if let Some(home) = home {
        candidates.push(ConfigRoot {
            path: home.join(".claude"),
            profile: "default".to_string(),
        });

        let profiles_dir = home.join(".claude-profiles");
        if let Ok(entries) = fs::read_dir(profiles_dir) {
            let mut profile_paths: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect();
            profile_paths.sort();
            candidates.extend(profile_paths.into_iter().map(|path| ConfigRoot {
                profile: root_basename(&path),
                path,
            }));
        }
    }

    let mut canonical_projects_paths = HashSet::new();
    candidates
        .into_iter()
        .filter_map(|root| {
            let canonical_root = fs::canonicalize(&root.path).ok()?;
            let dedup_path = fs::canonicalize(root.path.join("projects"))
                .unwrap_or_else(|_| canonical_root.clone());
            if canonical_projects_paths.insert(dedup_path) {
                Some(ConfigRoot {
                    path: canonical_root,
                    profile: root.profile,
                })
            } else {
                None
            }
        })
        .collect()
}

fn root_basename(path: &Path) -> String {
    path.file_name()
        .and_then(OsStr::to_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("default")
        .to_string()
}

fn scan_roots(roots: &[ConfigRoot], now: DateTime<Utc>) -> ScanResult {
    let session_cutoff = now - Duration::hours(SESSION_HOURS);
    let weekly_cutoff = now - Duration::hours(WEEKLY_HOURS);
    let mut session = HashMap::<String, ProjectTotals>::new();
    let mut weekly = HashMap::<String, ProjectTotals>::new();
    let mut warnings = Vec::new();

    for root in roots {
        scan_root(
            root,
            session_cutoff,
            weekly_cutoff,
            &mut session,
            &mut weekly,
            &mut warnings,
        );
    }

    ScanResult {
        report: ProjectsReport {
            generated_at: now,
            windows: Windows {
                session: finish_window(SESSION_HOURS as u64, session),
                weekly: finish_window(WEEKLY_HOURS as u64, weekly),
            },
        },
        warnings,
    }
}

fn scan_root(
    root: &ConfigRoot,
    session_cutoff: DateTime<Utc>,
    weekly_cutoff: DateTime<Utc>,
    session: &mut HashMap<String, ProjectTotals>,
    weekly: &mut HashMap<String, ProjectTotals>,
    warnings: &mut Vec<String>,
) {
    let projects_dir = root.path.join("projects");
    let Ok(project_dirs) = fs::read_dir(&projects_dir) else {
        return;
    };

    for project_dir in project_dirs {
        let project_dir = match project_dir {
            Ok(entry) if entry.path().is_dir() => entry,
            Ok(_) => continue,
            Err(error) => {
                warnings.push(format!(
                    "could not read entry in {}: {error}",
                    projects_dir.display()
                ));
                continue;
            }
        };
        let slug = project_dir.file_name().to_string_lossy().into_owned();
        let path = project_dir.path();
        let files = match fs::read_dir(&path) {
            Ok(files) => files,
            Err(error) => {
                warnings.push(format!("could not read {}: {error}", path.display()));
                continue;
            }
        };

        for file in files {
            let file = match file {
                Ok(entry) => entry,
                Err(error) => {
                    warnings.push(format!(
                        "could not read entry in {}: {error}",
                        path.display()
                    ));
                    continue;
                }
            };
            let file_path = file.path();
            if file_path.extension() != Some(OsStr::new("jsonl")) {
                continue;
            }
            if !file_is_recent(&file_path, weekly_cutoff, warnings) {
                continue;
            }
            let events = read_events(&file_path, warnings);
            for event in events {
                if event.timestamp >= weekly_cutoff {
                    weekly
                        .entry(slug.clone())
                        .or_default()
                        .add(event.usage, &root.profile);
                }
                if event.timestamp >= session_cutoff {
                    session
                        .entry(slug.clone())
                        .or_default()
                        .add(event.usage, &root.profile);
                }
            }
        }
    }
}

fn file_is_recent(path: &Path, cutoff: DateTime<Utc>, warnings: &mut Vec<String>) -> bool {
    let modified = match fs::metadata(path).and_then(|metadata| metadata.modified()) {
        Ok(modified) => modified,
        Err(error) => {
            warnings.push(format!(
                "could not read metadata for {}: {error}",
                path.display()
            ));
            return false;
        }
    };
    DateTime::<Utc>::from(modified) >= cutoff
}

fn read_events(path: &Path, warnings: &mut Vec<String>) -> Vec<UsageEvent> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            warnings.push(format!("could not read {}: {error}", path.display()));
            return Vec::new();
        }
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut deduped = HashMap::<(Option<String>, Option<String>), UsageEvent>::new();
    let mut without_ids = Vec::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                warnings.push(format!(
                    "could not finish reading {}: {error}",
                    path.display()
                ));
                break;
            }
        }
        let Ok(event) = serde_json::from_str::<TranscriptEvent>(&line) else {
            continue;
        };
        let (Some(timestamp), Some(usage)) = (event.timestamp, event.message.usage) else {
            continue;
        };
        let usage_event = UsageEvent { timestamp, usage };
        let key = (event.message.id, event.request_id);
        if key.0.is_none() && key.1.is_none() {
            without_ids.push(usage_event);
        } else {
            deduped.insert(key, usage_event);
        }
    }

    without_ids.extend(deduped.into_values());
    without_ids
}

fn finish_window(hours: u64, totals: HashMap<String, ProjectTotals>) -> UsageWindow {
    let total_weighted: f64 = totals.values().map(ProjectTotals::weighted_tokens).sum();
    let mut projects: Vec<ProjectUsage> = totals
        .into_iter()
        .filter_map(|(id, totals)| {
            let weighted_tokens = totals.weighted_tokens();
            if weighted_tokens <= 0.0 {
                return None;
            }
            Some(ProjectUsage {
                label: id.strip_prefix('-').unwrap_or(&id).to_string(),
                id,
                share_pct: round_one_decimal(weighted_tokens / total_weighted * 100.0),
                weighted_tokens: round_two_decimals(weighted_tokens),
                input_tokens: totals.input_tokens,
                output_tokens: totals.output_tokens,
                cache_creation_tokens: totals.cache_creation_tokens,
                cache_read_tokens: totals.cache_read_tokens,
                profiles: totals.profiles.into_iter().collect(),
            })
        })
        .collect();
    projects.sort_by(|a, b| {
        b.weighted_tokens
            .total_cmp(&a.weighted_tokens)
            .then_with(|| a.id.cmp(&b.id))
    });
    UsageWindow { hours, projects }
}

fn round_one_decimal(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn round_two_decimals(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use chrono::SecondsFormat;
    use tempfile::TempDir;

    use super::*;

    fn root(temp: &TempDir, name: &str, profile: &str) -> ConfigRoot {
        let path = temp.path().join(name);
        fs::create_dir_all(path.join("projects")).unwrap();
        ConfigRoot {
            path,
            profile: profile.to_string(),
        }
    }

    fn write_project(root: &ConfigRoot, slug: &str, lines: &[String]) {
        let dir = root.path.join("projects").join(slug);
        fs::create_dir_all(&dir).unwrap();
        let mut file = File::create(dir.join("session.jsonl")).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    fn event(
        timestamp: DateTime<Utc>,
        message_id: Option<&str>,
        request_id: Option<&str>,
        input: u64,
        output: u64,
        cache_creation: u64,
        cache_read: u64,
    ) -> String {
        serde_json::json!({
            "timestamp": timestamp.to_rfc3339_opts(SecondsFormat::Millis, true),
            "requestId": request_id,
            "message": {
                "id": message_id,
                "model": "claude-sonnet-4-5",
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_creation_input_tokens": cache_creation,
                    "cache_read_input_tokens": cache_read
                }
            }
        })
        .to_string()
    }

    #[test]
    fn filters_events_into_rolling_windows() {
        let temp = TempDir::new().unwrap();
        let root = root(&temp, "config", "default");
        let now = Utc::now();
        write_project(
            &root,
            "-project",
            &[
                event(now - Duration::hours(2), None, None, 10, 0, 0, 0),
                event(now - Duration::hours(6), None, None, 20, 0, 0, 0),
                event(now - Duration::days(8), None, None, 40, 0, 0, 0),
            ],
        );

        let report = scan_roots(&[root], now).report;
        assert_eq!(report.windows.session.projects[0].input_tokens, 10);
        assert_eq!(report.windows.weekly.projects[0].input_tokens, 30);
    }

    #[test]
    fn dedupes_id_pairs_using_the_last_occurrence() {
        let temp = TempDir::new().unwrap();
        let root = root(&temp, "config", "default");
        let now = Utc::now();
        write_project(
            &root,
            "-project",
            &[
                event(now, Some("msg"), Some("req"), 10, 1, 0, 0),
                event(now, Some("msg"), Some("req"), 25, 4, 0, 0),
                event(now, None, None, 3, 0, 0, 0),
                event(now, None, None, 7, 0, 0, 0),
            ],
        );

        let project = &scan_roots(&[root], now).report.windows.session.projects[0];
        assert_eq!(project.input_tokens, 35);
        assert_eq!(project.output_tokens, 4);
    }

    #[test]
    fn computes_weighted_totals_shares_and_sort_order() {
        let temp = TempDir::new().unwrap();
        let root = root(&temp, "config", "default");
        let now = Utc::now();
        write_project(&root, "-alpha", &[event(now, None, None, 10, 2, 4, 10)]);
        write_project(&root, "-beta", &[event(now, None, None, 13, 0, 0, 0)]);

        let projects = scan_roots(&[root], now).report.windows.session.projects;
        assert_eq!(projects[0].id, "-alpha");
        assert_eq!(projects[0].weighted_tokens, 26.0);
        assert_eq!(projects[0].share_pct, 66.7);
        assert_eq!(projects[1].weighted_tokens, 13.0);
        assert_eq!(projects[1].share_pct, 33.3);
    }

    #[test]
    fn merges_a_slug_across_profiles() {
        let temp = TempDir::new().unwrap();
        let first = root(&temp, "first", "zeta");
        let second = root(&temp, "second", "alpha");
        let now = Utc::now();
        write_project(&first, "-shared", &[event(now, None, None, 2, 0, 0, 0)]);
        write_project(&second, "-shared", &[event(now, None, None, 3, 0, 0, 0)]);

        let projects = scan_roots(&[first, second], now)
            .report
            .windows
            .session
            .projects;
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].input_tokens, 5);
        assert_eq!(projects[0].profiles, ["alpha", "zeta"]);
        assert_eq!(projects[0].label, "shared");
    }

    #[test]
    fn skips_malformed_and_usage_less_lines() {
        let temp = TempDir::new().unwrap();
        let root = root(&temp, "config", "default");
        let now = Utc::now();
        write_project(
            &root,
            "-project",
            &[
                "not json".to_string(),
                serde_json::json!({"timestamp": now, "message": {"id": "unused"}}).to_string(),
                event(now, None, None, 9, 0, 0, 0),
            ],
        );

        let result = scan_roots(&[root], now);
        assert!(result.warnings.is_empty());
        assert_eq!(result.report.windows.session.projects[0].input_tokens, 9);
    }

    #[test]
    fn discovers_every_root_source_and_dedupes_canonical_paths() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join(".claude")).unwrap();
        let profile_a = temp.path().join(".claude-profiles/a");
        let profile_b = temp.path().join(".claude-profiles/b");
        fs::create_dir_all(&profile_a).unwrap();
        fs::create_dir_all(&profile_b).unwrap();

        let roots = discover_roots_from(Some(temp.path()), Some(profile_a.as_os_str()));
        assert_eq!(roots.len(), 3);
        assert_eq!(roots[0].profile, "a");
        assert_eq!(roots[1].profile, "default");
        assert_eq!(roots[2].profile, "b");
    }

    #[test]
    fn discovers_env_root_without_a_home_directory() {
        let temp = TempDir::new().unwrap();
        let env_root = temp.path().join("env-config");
        fs::create_dir_all(&env_root).unwrap();

        let roots = discover_roots_from(None, Some(env_root.as_os_str()));

        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].profile, "env-config");
        assert_eq!(roots[0].path, fs::canonicalize(env_root).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn dedupes_roots_whose_projects_dirs_resolve_to_the_same_target() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let shared_projects = temp.path().join("shared-projects");
        fs::create_dir_all(&shared_projects).unwrap();

        let first_path = temp.path().join("first");
        let default_path = temp.path().join(".claude");
        fs::create_dir_all(&first_path).unwrap();
        fs::create_dir_all(&default_path).unwrap();
        symlink(&shared_projects, first_path.join("projects")).unwrap();
        symlink(&shared_projects, default_path.join("projects")).unwrap();

        let first = ConfigRoot {
            path: first_path.clone(),
            profile: "first".to_string(),
        };
        let now = Utc::now();
        write_project(&first, "-shared", &[event(now, None, None, 7, 0, 0, 0)]);

        let roots = discover_roots_from(Some(temp.path()), Some(first_path.as_os_str()));
        let projects = scan_roots(&roots, now).report.windows.session.projects;

        assert_eq!(roots.len(), 1);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].input_tokens, 7);
        assert_eq!(projects[0].profiles, ["first"]);
    }
}
