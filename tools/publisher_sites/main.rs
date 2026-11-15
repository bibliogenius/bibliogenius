//! Maintenance tool for the publisher site table (`data/publisher_sites/`).
//!
//! ```text
//! cargo run --release --bin publisher_sites -- wikidata
//! cargo run --release --bin publisher_sites -- probe [--site KEY,KEY] [--new] [--status S]
//!                                                    [--older-than DAYS] [--limit N]
//!                                                    [--workers N] [--pause SECS]
//! cargo run --release --bin publisher_sites -- check [--site KEY,KEY] [--limit N]
//!                                                    [--workers N] [--pause SECS]
//! cargo run --release --bin publisher_sites -- build [--offline]
//! cargo run --release --bin publisher_sites -- report [--out FILE]
//! cargo run --release --bin publisher_sites -- lint
//! cargo run --release --bin publisher_sites -- diff [--base GIT_REF|FILE]
//! cargo run --release --bin publisher_sites -- measure --books FILE [--out DIR]
//!                                                      [--workers N] [--pause SECS]
//! ```
//!
//! See `data/publisher_sites/README.md` for what each step does and when to
//! run it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;
use url::Url;

use rust_lib_app::modules::integrations::API_USER_AGENT;
use rust_lib_app::modules::integrations::publisher_sites::build::{
    CorrectionState, SiteSource, build_table, candidate_table, normalize_prefix,
    pending_corrections, site_sources,
};
use rust_lib_app::modules::integrations::publisher_sites::check::{
    CheckResult, after_reprobe, check_recipes, keeps_previous, next_record,
};
use rust_lib_app::modules::integrations::publisher_sites::cover::{PageCoverError, page_cover};
use rust_lib_app::modules::integrations::publisher_sites::fetch::{
    FetchError, PoliteFetcher, SiteFetcher,
};
use rust_lib_app::modules::integrations::publisher_sites::lint::{Severity, diff, lint};
use rust_lib_app::modules::integrations::publisher_sites::lookup::{
    BookQuery, Lookup, SitemapCache, find_book_page_within,
};
use rust_lib_app::modules::integrations::publisher_sites::probe::{find_witnesses, probe_site};
use rust_lib_app::modules::integrations::publisher_sites::table::{
    CheckStatus, Overrides, ProbeReport, ProbeStatus, PublisherTable, RecipeCheck, RecipeKind,
    SiteOverride, SiteProbe, SiteRecipes, WikidataPublisher, Witness,
};

/// The report page; its `/*DATA*/null` placeholder receives the data.
const REPORT_TEMPLATE: &str = include_str!("report.html");

const WIKIDATA_ENDPOINT: &str = "https://query.wikidata.org/sparql";

/// Publishers with an ISBN prefix in the French-language group (978-2,
/// 979-10), their site when they have one. Old ISBN-10 style values (`2-07`)
/// are kept and normalised by the tool.
const WIKIDATA_QUERY: &str = r#"
SELECT ?p ?label ?prefix ?site WHERE {
  ?p wdt:P3035 ?prefix .
  FILTER(REGEX(?prefix, "^(978-?2|979-?10|2-)"))
  OPTIONAL { ?p wdt:P856 ?site }
  OPTIONAL { ?p rdfs:label ?fr FILTER(LANG(?fr) = "fr") }
  OPTIONAL { ?p rdfs:label ?en FILTER(LANG(?en) = "en") }
  BIND(COALESCE(?fr, ?en, "") AS ?label)
}
"#;

/// Longest time given to one site's probe: about a hundred requests at the
/// longest Crawl-delay honoured (60 s), with room to spare.
const SITE_DEADLINE: Duration = Duration::from_secs(2 * 60 * 60);

struct Paths {
    dir: PathBuf,
}

impl Paths {
    fn wikidata(&self) -> PathBuf {
        self.dir.join("wikidata.json")
    }
    fn probe(&self) -> PathBuf {
        self.dir.join("probe.json")
    }
    fn overrides(&self) -> PathBuf {
        self.dir.join("overrides.json")
    }
    fn table(&self) -> PathBuf {
        self.dir.join("table.json")
    }
}

/// Command-line flags after the command: `--name value` pairs and bare
/// `--name` switches.
struct Flags(BTreeMap<String, Option<String>>);

impl Flags {
    /// Parse `args` against `known`: each option's name and whether it takes
    /// a value. An unknown option, or one missing its value, is an error: a
    /// typo must not silently widen a run (`--sites a.fr` probing every site).
    fn parse(args: &[String], known: &[(&str, bool)]) -> Result<Self, String> {
        let mut map = BTreeMap::new();
        let mut i = 0;
        while i < args.len() {
            let name = args[i]
                .strip_prefix("--")
                .ok_or_else(|| format!("unexpected argument {}", args[i]))?;
            let takes_value = known
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| *v)
                .ok_or_else(|| format!("unknown option --{name}"))?;
            let value = if takes_value {
                let v = args
                    .get(i + 1)
                    .filter(|v| !v.starts_with("--"))
                    .ok_or_else(|| format!("--{name} needs a value"))?;
                i += 2;
                Some(v.clone())
            } else {
                i += 1;
                None
            };
            map.insert(name.to_owned(), value);
        }
        Ok(Self(map))
    }
    fn has(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }
    fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).and_then(|v| v.as_deref())
    }
    fn number<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T, String> {
        match self.get(name) {
            None => Ok(default),
            Some(v) => v
                .parse()
                .map_err(|_| format!("--{name}: not a number: {v}")),
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let paths = Paths {
        dir: Path::new(env!("CARGO_MANIFEST_DIR")).join("data/publisher_sites"),
    };
    let result = match args.first().map(String::as_str) {
        Some(cmd @ ("wikidata" | "probe" | "check" | "build" | "report" | "lint" | "diff" | "measure")) => {
            const PACING: [(&str, bool); 2] = [("workers", true), ("pause", true)];
            let known: Vec<(&str, bool)> = match cmd {
                "probe" => [
                    ("site", true),
                    ("new", false),
                    ("status", true),
                    ("older-than", true),
                    ("limit", true),
                ]
                .into_iter()
                .chain(PACING)
                .collect(),
                "check" => [("site", true), ("limit", true)]
                    .into_iter()
                    .chain(PACING)
                    .collect(),
                "build" => std::iter::once(("offline", false)).chain(PACING).collect(),
                "report" => vec![("out", true)],
                "diff" => vec![("base", true)],
                "measure" => [("books", true), ("out", true)]
                    .into_iter()
                    .chain(PACING)
                    .collect(),
                _ => Vec::new(),
            };
            match Flags::parse(&args[1..], &known) {
                Ok(flags) => match cmd {
                    "wikidata" => cmd_wikidata(&paths).await,
                    "probe" => cmd_probe(&paths, &flags).await,
                    "check" => cmd_check(&paths, &flags).await,
                    "build" => cmd_build(&paths, &flags).await,
                    "lint" => cmd_lint(&paths),
                    "diff" => cmd_diff(&paths, &flags),
                    "measure" => cmd_measure(&paths, &flags).await,
                    _ => cmd_report(&paths, &flags),
                },
                Err(e) => Err(e),
            }
        }
        _ => Err(
            "usage: publisher_sites wikidata | probe | check | build | report | lint | diff | measure (see the file header)".into(),
        ),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn read_json<T: DeserializeOwned + Default>(path: &Path) -> Result<T, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Write pretty JSON through a temporary file, so an interrupted run never
/// leaves a truncated file behind.
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    text.push('\n');
    write_text(path, text)
}

/// `probe.json`, one site per line: see [`ProbeReport::to_json_by_site`].
fn write_probe(path: &Path, report: &ProbeReport) -> Result<(), String> {
    write_text(path, report.to_json_by_site().map_err(|e| e.to_string())?)
}

fn write_text(path: &Path, text: String) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

// ------------------------------------------------------------------ wikidata

async fn cmd_wikidata(paths: &Paths) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .user_agent(API_USER_AGENT)
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|e| e.to_string())?;
    let response: serde_json::Value = client
        .get(WIKIDATA_ENDPOINT)
        .query(&[("query", WIKIDATA_QUERY)])
        .header("Accept", "application/sparql-results+json")
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("Wikidata: {e}"))?
        .json()
        .await
        .map_err(|e| format!("Wikidata answer: {e}"))?;
    let bindings = response["results"]["bindings"]
        .as_array()
        .ok_or("Wikidata answer without results")?;

    let mut by_qid: BTreeMap<String, WikidataPublisher> = BTreeMap::new();
    let mut skipped: Vec<String> = Vec::new();
    for b in bindings {
        let value = |k: &str| b[k]["value"].as_str().unwrap_or("").to_owned();
        let qid = value("p").rsplit('/').next().unwrap_or("").to_owned();
        let Some(prefix) = normalize_prefix(&value("prefix")) else {
            skipped.push(value("prefix"));
            continue;
        };
        let publisher = by_qid
            .entry(qid.clone())
            .or_insert_with(|| WikidataPublisher {
                qid,
                name: value("label"),
                prefixes: Vec::new(),
                sites: Vec::new(),
            });
        if !publisher.prefixes.contains(&prefix) {
            publisher.prefixes.push(prefix);
        }
        let site = value("site");
        if !site.is_empty() && !publisher.sites.contains(&site) {
            publisher.sites.push(site);
        }
    }
    let mut publishers: Vec<WikidataPublisher> = by_qid.into_values().collect();
    for p in &mut publishers {
        p.prefixes.sort();
        p.sites.sort();
    }
    let with_site = publishers.iter().filter(|p| !p.sites.is_empty()).count();
    if !skipped.is_empty() {
        eprintln!(
            "warning: {} prefix values are not publisher prefixes and were skipped: {}",
            skipped.len(),
            skipped.join(", ")
        );
    }
    write_json(&paths.wikidata(), &publishers)?;
    println!(
        "{} publishers with a prefix, {with_site} with a site -> {}",
        publishers.len(),
        paths.wikidata().display()
    );
    Ok(())
}

// --------------------------------------------------------------------- probe

/// Which sites a probe run looks at, from its flags.
fn select_sites(
    sources: BTreeMap<String, SiteSource>,
    report: &ProbeReport,
    flags: &Flags,
) -> Result<Vec<(String, SiteSource)>, String> {
    let older_than = match flags.get("older-than") {
        Some(_) => Some(flags.number::<i64>("older-than", 0)?),
        None => None,
    };
    let cutoff = older_than.map(|days| {
        (chrono::Local::now() - chrono::Duration::days(days))
            .format("%Y-%m-%d")
            .to_string()
    });
    let status = flags.get("status");
    let mut todo: Vec<(String, SiteSource)> = sources
        .into_iter()
        .filter(|(_, s)| !s.excluded)
        .filter(|(key, _)| {
            flags
                .get("site")
                .is_none_or(|k| k.split(',').any(|k| k == key))
        })
        .filter(|(key, _)| !flags.has("new") || !report.sites.contains_key(key))
        .filter(|(key, _)| {
            status.is_none_or(|s| {
                report
                    .sites
                    .get(key)
                    .is_some_and(|p| status_name(p.status) == s)
            })
        })
        .filter(|(key, _)| {
            cutoff.as_deref().is_none_or(|c| {
                report
                    .sites
                    .get(key)
                    .is_none_or(|p| p.probed_at.as_str() < c)
            })
        })
        .collect();
    if let Some(site) = flags.get("site")
        && todo.is_empty()
    {
        return Err(format!("{site}: no such site (or excluded)"));
    }
    todo.truncate(flags.number("limit", usize::MAX)?);
    Ok(todo)
}

fn status_name(s: ProbeStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

async fn cmd_probe(paths: &Paths, flags: &Flags) -> Result<(), String> {
    let wikidata: Vec<WikidataPublisher> = read_json(&paths.wikidata())?;
    if wikidata.is_empty() {
        return Err("no Wikidata data: run `publisher_sites wikidata` first".into());
    }
    let overrides: Overrides = read_json(&paths.overrides())?;
    let report: ProbeReport = read_json(&paths.probe())?;
    let todo = select_sites(site_sources(&wikidata, &overrides), &report, flags)?;
    let workers = flags.number("workers", 12usize)?.max(1);
    let pause = flags.number("pause", 1.5f64)?.max(0.5);
    println!(
        "probing {} sites, {workers} at a time, {pause} s between two requests to a host",
        todo.len()
    );

    let fetcher = Arc::new(PoliteFetcher::new(Duration::from_secs_f64(pause)));
    let report = Arc::new(Mutex::new(report));
    let total = todo.len();
    let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let date = today();
    let results: Vec<Result<(), String>> = stream::iter(todo)
        .map(|(key, source)| {
            let (fetcher, report, done, overrides, date) = (
                fetcher.clone(),
                report.clone(),
                done.clone(),
                &overrides,
                &date,
            );
            async move {
                let extra = overrides
                    .sites
                    .get(&key)
                    .map(|o| o.witnesses.clone())
                    .unwrap_or_default();
                let result = match Url::parse(&source.home) {
                    Ok(home) => tokio::time::timeout(
                        SITE_DEADLINE,
                        probe_site(fetcher.as_ref(), &home, &extra, date),
                    )
                    .await
                    .unwrap_or_else(|_| failed(&source.home, date, "probe took too long")),
                    Err(_) => failed(&source.home, date, "home page address does not parse"),
                };
                let n = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                let mut report = report.lock().await;
                if keeps_previous(report.sites.get(&key), &result) {
                    println!(
                        "[{n}/{total}] {key}: {}; last good probe kept",
                        summary(&result)
                    );
                } else {
                    println!("[{n}/{total}] {key}: {}", summary(&result));
                    report.sites.insert(key, result);
                }
                // Saved after every site: a long run can be stopped and resumed with --new.
                write_probe(&paths.probe(), &report)
            }
        })
        .buffer_unordered(workers)
        .collect()
        .await;
    results.into_iter().collect::<Result<(), String>>()?;
    print_statuses(&report.lock().await.sites);
    Ok(())
}

fn failed(home: &str, date: &str, why: &str) -> SiteProbe {
    SiteProbe {
        home: home.to_owned(),
        status: ProbeStatus::Offline,
        probed_at: date.to_owned(),
        note: Some(why.to_owned()),
        witnesses: Vec::new(),
        product: None,
        search: None,
        tried: Vec::new(),
        book_sitemap: None,
        sitemap: None,
    }
}

fn summary(p: &SiteProbe) -> String {
    let mut parts = vec![
        status_name(p.status),
        format!("{} witnesses", p.witnesses.len()),
    ];
    if let Some(c) = &p.product {
        parts.push(format!(
            "product {}{}",
            c.recipe.template,
            if c.recipe.slug_ignored {
                " (slug ignored)"
            } else {
                ""
            }
        ));
    }
    if let Some(c) = &p.search {
        parts.push(format!(
            "search {} (isbn {}, title {})",
            c.recipe.url, c.found_by_isbn, c.found_by_title
        ));
    }
    if let Some(c) = &p.sitemap {
        parts.push(format!(
            "sitemap {} ({} of {})",
            c.recipe.url, c.found, c.tried
        ));
    }
    if let Some(n) = &p.note {
        parts.push(n.clone());
    }
    parts.join(" · ")
}

fn print_statuses(sites: &BTreeMap<String, SiteProbe>) {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for p in sites.values() {
        *counts.entry(status_name(p.status)).or_default() += 1;
    }
    let line: Vec<String> = counts.iter().map(|(s, n)| format!("{s} {n}")).collect();
    println!("probe.json: {} sites: {}", sites.len(), line.join(", "));
}

// --------------------------------------------------------------------- check

/// What the check of one site concluded, and the new probe when it took one.
struct CheckOutcome {
    status: CheckStatus,
    note: Option<String>,
    failed: Vec<RecipeKind>,
    witnesses: Option<Vec<Witness>>,
    reprobe: Option<SiteProbe>,
}

fn failed_names(result: &CheckResult) -> Vec<&'static str> {
    result
        .failed()
        .into_iter()
        .map(|kind| match kind {
            RecipeKind::Product => "product",
            RecipeKind::Search => "search",
            RecipeKind::Sitemap => "sitemap",
        })
        .collect()
}

/// Check one site: replay its recipes on its witnesses (new ones from the
/// home page when none is left), probe it again when a probed recipe fails,
/// flag it when a hand-written one does.
async fn check_one(
    fetcher: &PoliteFetcher,
    recipes: &SiteRecipes,
    known: Vec<Witness>,
    correction: Option<&SiteOverride>,
    date: &str,
) -> CheckOutcome {
    let outcome = |status, note: Option<String>| CheckOutcome {
        status,
        note,
        failed: Vec::new(),
        witnesses: None,
        reprobe: None,
    };
    let unreachable = |e: FetchError| match e {
        FetchError::Blocked(why) => outcome(CheckStatus::Blocked, Some(why)),
        other => outcome(CheckStatus::Offline, Some(format!("{other:?}"))),
    };
    let mut witnesses = known;
    let mut result = match check_recipes(fetcher, recipes, &witnesses).await {
        Ok(r) => r,
        Err(e) => return unreachable(e),
    };
    let mut fresh = None;
    if result.live_witnesses == 0 {
        // The witnesses left the catalogue: take today's books of the home page.
        let Ok(home) = Url::parse(&recipes.home) else {
            return outcome(
                CheckStatus::NoWitness,
                Some("home page address does not parse".into()),
            );
        };
        let new = match fetcher.get(&home).await {
            Ok(page) => match find_witnesses(fetcher, &page).await {
                Ok(w) => w,
                Err(e) => return unreachable(e),
            },
            Err(e) => return unreachable(e),
        };
        if new.is_empty() {
            return outcome(
                CheckStatus::NoWitness,
                Some("no book found on the home page".into()),
            );
        }
        witnesses = new.clone();
        witnesses.extend(correction.map(|o| o.witnesses.clone()).unwrap_or_default());
        fresh = Some(new);
        result = match check_recipes(fetcher, recipes, &witnesses).await {
            Ok(r) => r,
            Err(e) => return unreachable(e),
        };
        if result.live_witnesses == 0 {
            return outcome(
                CheckStatus::NoWitness,
                Some("new witnesses could not be read".into()),
            );
        }
    }
    if result.all_work() {
        return CheckOutcome {
            witnesses: fresh,
            ..outcome(CheckStatus::Works, None)
        };
    }
    if !result.needs_reprobe(correction) {
        return CheckOutcome {
            witnesses: fresh,
            failed: result.failed(),
            ..outcome(
                CheckStatus::Broken,
                Some(format!(
                    "hand-written {} recipe fails",
                    failed_names(&result).join(" and ")
                )),
            )
        };
    }
    let extra = correction.map(|o| o.witnesses.clone()).unwrap_or_default();
    let probe = match Url::parse(&recipes.home) {
        Ok(home) => tokio::time::timeout(SITE_DEADLINE, probe_site(fetcher, &home, &extra, date))
            .await
            .unwrap_or_else(|_| failed(&recipes.home, date, "probe took too long")),
        Err(_) => failed(&recipes.home, date, "home page address does not parse"),
    };
    let status = after_reprobe(&probe);
    let note = Some(format!(
        "{} recipe failed; probed again: {}",
        failed_names(&result).join(" and "),
        status_name(probe.status)
    ));
    if status != CheckStatus::Reprobed {
        // Blocked or offline on the way: the previous probe stands.
        return outcome(status, note);
    }
    CheckOutcome {
        status,
        note,
        failed: result.failed(),
        witnesses: None,
        reprobe: Some(probe),
    }
}

fn check_status_name(s: CheckStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

async fn cmd_check(paths: &Paths, flags: &Flags) -> Result<(), String> {
    let wikidata: Vec<WikidataPublisher> = read_json(&paths.wikidata())?;
    let overrides: Overrides = read_json(&paths.overrides())?;
    let report: ProbeReport = read_json(&paths.probe())?;
    // Every site with a recipe: hand-written ones whatever their state, and
    // those left out for being unreachable, so they can come back.
    let candidates = candidate_table(&wikidata, &report, &overrides, &today());
    let mut todo: Vec<(String, SiteRecipes)> = candidates
        .sites
        .into_iter()
        .filter(|(key, _)| {
            flags
                .get("site")
                .is_none_or(|k| k.split(',').any(|k| k == key))
        })
        .collect();
    todo.truncate(flags.number("limit", usize::MAX)?);
    let date = run_checks(paths, &overrides, report, todo, flags).await?;

    let report: ProbeReport = read_json(&paths.probe())?;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for c in report.checks.values().filter(|c| c.checked_at == date) {
        *counts.entry(check_status_name(c.status)).or_default() += 1;
    }
    let line: Vec<String> = counts.iter().map(|(s, n)| format!("{s} {n}")).collect();
    println!(
        "checked today: {}. Run `build` to update table.json.",
        line.join(", ")
    );
    Ok(())
}

/// Check `todo`'s recipes, saving `probe.json` after every site. Returns the
/// date the checks are recorded under.
async fn run_checks(
    paths: &Paths,
    overrides: &Overrides,
    report: ProbeReport,
    todo: Vec<(String, SiteRecipes)>,
    flags: &Flags,
) -> Result<String, String> {
    let workers = flags.number("workers", 12usize)?.max(1);
    let pause = flags.number("pause", 1.5f64)?.max(0.5);
    println!("checking {} sites, {workers} at a time", todo.len());

    let fetcher = Arc::new(PoliteFetcher::new(Duration::from_secs_f64(pause)));
    let report = Arc::new(Mutex::new(report));
    let total = todo.len();
    let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let date = today();
    let results: Vec<Result<(), String>> = stream::iter(todo)
        .map(|(key, recipes)| {
            let (fetcher, report, done, date) =
                (fetcher.clone(), report.clone(), done.clone(), &date);
            async move {
                let correction = overrides.sites.get(&key);
                // Witnesses given by hand first: someone chose them to prove
                // this site's recipes.
                let mut witnesses = correction.map(|o| o.witnesses.clone()).unwrap_or_default();
                let probed = {
                    let report = report.lock().await;
                    report
                        .sites
                        .get(&key)
                        .map(|p| p.witnesses.clone())
                        .unwrap_or_default()
                };
                for w in probed {
                    if !witnesses.iter().any(|x| x.isbn == w.isbn) {
                        witnesses.push(w);
                    }
                }
                let outcome = check_one(&fetcher, &recipes, witnesses, correction, date).await;
                let n = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                println!(
                    "[{n}/{total}] {key}: {}{}",
                    check_status_name(outcome.status),
                    outcome
                        .note
                        .as_deref()
                        .map(|n| format!(" · {n}"))
                        .unwrap_or_default()
                );
                let mut report = report.lock().await;
                let record = next_record(
                    report.checks.get(&key),
                    date,
                    outcome.status,
                    outcome.note,
                    outcome.failed,
                    &recipes,
                );
                if let Some(probe) = outcome.reprobe {
                    report.sites.insert(key.clone(), probe);
                } else if let (Some(fresh), Some(probe)) =
                    (outcome.witnesses, report.sites.get_mut(&key))
                {
                    probe.witnesses = fresh;
                }
                report.checks.insert(key, record);
                write_probe(&paths.probe(), &report)
            }
        })
        .buffer_unordered(workers)
        .collect()
        .await;
    results.into_iter().collect::<Result<(), String>>()?;
    Ok(date)
}

// --------------------------------------------------------------------- build

async fn cmd_build(paths: &Paths, flags: &Flags) -> Result<(), String> {
    let wikidata: Vec<WikidataPublisher> = read_json(&paths.wikidata())?;
    let overrides: Overrides = read_json(&paths.overrides())?;
    let mut report: ProbeReport = read_json(&paths.probe())?;

    // Hand-written recipes new or changed since their last check are checked
    // now: none enters the table without having worked in its current form.
    let unverified: Vec<String> = pending_corrections(&report, &overrides)
        .into_iter()
        .filter(|c| c.state == CorrectionState::Unverified)
        .map(|c| c.site)
        .collect();
    if !unverified.is_empty() && !flags.has("offline") {
        let candidates = candidate_table(&wikidata, &report, &overrides, &today());
        let todo: Vec<(String, SiteRecipes)> = candidates
            .sites
            .into_iter()
            .filter(|(key, _)| unverified.contains(key))
            .collect();
        run_checks(paths, &overrides, report, todo, flags).await?;
        report = read_json(&paths.probe())?;
    }

    for pending in pending_corrections(&report, &overrides) {
        let kind = match pending.kind {
            RecipeKind::Product => "product",
            RecipeKind::Search => "search",
            RecipeKind::Sitemap => "sitemap",
        };
        let why = match pending.state {
            CorrectionState::Failed => "failed its last check",
            _ => "not checked in its current form (--offline, or the site did not answer)",
        };
        eprintln!(
            "warning: {}: hand-written {kind} recipe {why}; left out of the table",
            pending.site
        );
    }
    let table = build_table(&wikidata, &report, &overrides, &today());
    write_json(&paths.table(), &table)?;
    let size = std::fs::metadata(paths.table())
        .map(|m| m.len())
        .unwrap_or(0);
    println!(
        "{} sites, {} prefixes -> {} ({} KB)",
        table.sites.len(),
        table.prefixes.len(),
        paths.table().display(),
        size / 1024
    );
    Ok(())
}

// -------------------------------------------------------------------- report

/// One site as the report page shows it.
#[derive(Serialize)]
struct ReportSite<'a> {
    key: &'a str,
    home: &'a str,
    publishers: &'a [String],
    prefixes: &'a [String],
    excluded: bool,
    in_table: bool,
    probe: Option<&'a SiteProbe>,
    check: Option<&'a RecipeCheck>,
    correction: Option<&'a SiteOverride>,
}

fn cmd_report(paths: &Paths, flags: &Flags) -> Result<(), String> {
    let wikidata: Vec<WikidataPublisher> = read_json(&paths.wikidata())?;
    let overrides: Overrides = read_json(&paths.overrides())?;
    let report: ProbeReport = read_json(&paths.probe())?;
    let table: PublisherTable = match std::fs::read_to_string(paths.table()) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("table.json: {e}"))?,
        Err(_) => build_table(&wikidata, &report, &overrides, &today()),
    };
    let mut sources = site_sources(&wikidata, &overrides);
    // The prefixes `build` read from the books found on a site.
    for entry in &table.prefixes {
        for publisher in &entry.publishers {
            if let Some(source) = sources.get_mut(&publisher.site)
                && !source.prefixes.contains(&entry.prefix)
            {
                source.prefixes.push(entry.prefix.clone());
            }
        }
    }
    let sites: Vec<ReportSite> = sources
        .iter()
        .map(|(key, s)| ReportSite {
            key,
            home: &s.home,
            publishers: &s.publishers,
            prefixes: &s.prefixes,
            excluded: s.excluded,
            in_table: table.sites.contains_key(key),
            probe: report.sites.get(key),
            check: report.checks.get(key),
            correction: overrides.sites.get(key),
        })
        .collect();
    let data = serde_json::json!({
        "generated": today(),
        "table_generated": table.generated,
        "wikidata_publishers": wikidata.len(),
        "sites": sites,
        "overrides": overrides,
    });
    // No `<` at all inside the inline script: `</script>` would end it, and
    // `<!--` followed by `<script` would derail the parser. `\u003c` is the
    // same character to JSON.
    let data = serde_json::to_string(&data)
        .map_err(|e| e.to_string())?
        .replace('<', "\\u003c");
    let html = REPORT_TEMPLATE.replace("/*DATA*/null", &data);
    let out = flags.get("out").map(PathBuf::from).unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("target/publisher_sites/index.html")
    });
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(&out, html).map_err(|e| format!("{}: {e}", out.display()))?;
    println!("{} sites -> {}", sites.len(), out.display());
    Ok(())
}

// ---------------------------------------------------------------- lint, diff

/// Defects a probe can leave in the table without failing: see `lint.rs`.
/// Fails when one of them is an error.
fn cmd_lint(paths: &Paths) -> Result<(), String> {
    let table = read_table(paths)?;
    let report: ProbeReport = read_json(&paths.probe())?;
    let findings = lint(&table, &report, &today());
    for f in &findings {
        let level = match f.rule.severity() {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        println!("{level} {} {}: {}", f.rule.name(), f.site, f.detail);
    }
    let errors = findings
        .iter()
        .filter(|f| f.rule.severity() == Severity::Error)
        .count();
    println!(
        "{} sites: {errors} errors, {} warnings",
        table.sites.len(),
        findings.len() - errors
    );
    if errors > 0 {
        return Err(format!("{errors} errors in table.json"));
    }
    Ok(())
}

/// What changed in table.json since `--base`: a git revision (`HEAD` by
/// default) or a file.
fn cmd_diff(paths: &Paths, flags: &Flags) -> Result<(), String> {
    let base = flags.get("base").unwrap_or("HEAD");
    if base.starts_with('-') {
        return Err(format!("--base {base}: not a revision nor a file"));
    }
    let old_text = if Path::new(base).is_file() {
        std::fs::read_to_string(base).map_err(|e| format!("{base}: {e}"))?
    } else {
        let out = std::process::Command::new("git")
            .current_dir(&paths.dir)
            .args(["show", &format!("{base}:./table.json")])
            .output()
            .map_err(|e| format!("git: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git show {base}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        String::from_utf8(out.stdout).map_err(|e| format!("{base}: {e}"))?
    };
    let old: PublisherTable =
        serde_json::from_str(&old_text).map_err(|e| format!("{base}: {e}"))?;
    let new = read_table(paths)?;
    let d = diff(&old, &new);
    let kind = |k: RecipeKind| match k {
        RecipeKind::Product => "product",
        RecipeKind::Search => "search",
        RecipeKind::Sitemap => "sitemap",
    };
    let none = |r: &Option<String>| r.clone().unwrap_or_else(|| "none".into());
    for key in &d.added_sites {
        println!("+ {key}");
    }
    for key in &d.removed_sites {
        println!("- {key}");
    }
    for (key, k, before, after) in &d.changed_recipes {
        println!("~ {key} {}: {} -> {}", kind(*k), none(before), none(after));
    }
    for (prefix, before, after) in &d.changed_prefixes {
        let list = |v: &Option<Vec<String>>| match v {
            None => "absent".to_owned(),
            Some(v) if v.is_empty() => "kept without a site".to_owned(),
            Some(v) => v.join(", "),
        };
        println!("  prefix {prefix}: {} -> {}", list(before), list(after));
    }
    println!(
        "since {base}: {} sites added, {} removed, {} recipes changed, {} prefixes changed ({} -> {} sites)",
        d.added_sites.len(),
        d.removed_sites.len(),
        d.changed_recipes.len(),
        d.changed_prefixes.len(),
        old.sites.len(),
        new.sites.len()
    );
    Ok(())
}

/// table.json, which `build` writes: unlike the other files, a missing one
/// is an error.
fn read_table(paths: &Paths) -> Result<PublisherTable, String> {
    let path = paths.table();
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

// ------------------------------------------------------------------- measure

/// A book of the measure's list, as the library export gives it.
#[derive(serde::Deserialize)]
struct MeasureBook {
    isbn: String,
    title: String,
    #[serde(default)]
    authors: String,
    #[serde(default)]
    publisher: Option<String>,
    /// The cover the library already has, shown beside the one found.
    #[serde(default)]
    cover_url: Option<String>,
}

#[derive(Serialize)]
struct Measured {
    isbn: String,
    title: String,
    publisher: String,
    library_cover: String,
    /// found, image_forbidden, image_broken, no_image, not_found, refused,
    /// unreachable, no_site, bad_isbn
    outcome: &'static str,
    site: String,
    recipe: String,
    page: String,
    image: String,
}

/// Look for each book of `--books` with the table, as the app will, and
/// write a contact sheet to judge the covers by eye: a wrong one is the
/// failure that counts. Each image kept is loaded once, as the app would.
async fn cmd_measure(paths: &Paths, flags: &Flags) -> Result<(), String> {
    let books_path = flags.get("books").ok_or("--books FILE is required")?;
    let books: Vec<MeasureBook> = serde_json::from_str(
        &std::fs::read_to_string(books_path).map_err(|e| format!("{books_path}: {e}"))?,
    )
    .map_err(|e| format!("{books_path}: {e}"))?;
    let table = Arc::new(read_table(paths)?);
    let workers = flags.number("workers", 12usize)?.max(1);
    let pause = flags.number("pause", 1.5f64)?.max(0.5);
    let fetcher = Arc::new(PoliteFetcher::new(Duration::from_secs_f64(pause)));
    let maps = Arc::new(SitemapCache::default());
    let total = books.len();
    println!(
        "measuring {total} books, {workers} at a time, {pause} s between two requests to a host"
    );
    let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut results: Vec<(usize, Measured)> = stream::iter(books.into_iter().enumerate())
        .map(|(i, book)| {
            let (fetcher, table, maps, done) =
                (fetcher.clone(), table.clone(), maps.clone(), done.clone());
            async move {
                let m = measure_one(fetcher.as_ref(), &table, &maps, book).await;
                let n = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                println!(
                    "[{n}/{total}] {} {}: {} {}",
                    m.isbn, m.title, m.outcome, m.site
                );
                (i, m)
            }
        })
        .buffer_unordered(workers)
        .collect()
        .await;
    results.sort_by_key(|(i, _)| *i);
    let results: Vec<Measured> = results.into_iter().map(|(_, m)| m).collect();

    let out = flags.get("out").map(PathBuf::from).unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("target/publisher_sites/measure")
    });
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    write_json(&out.join("results.json"), &results)?;
    std::fs::write(out.join("index.html"), contact_sheet(&results))
        .map_err(|e| format!("{}: {e}", out.display()))?;
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for r in &results {
        *counts.entry(r.outcome).or_default() += 1;
    }
    let summary: Vec<String> = counts.iter().map(|(k, n)| format!("{k} {n}")).collect();
    println!(
        "{total} books: {} -> {}",
        summary.join(", "),
        out.join("index.html").display()
    );
    Ok(())
}

/// Longest one book's lookup runs in a measure, its wait for its turn on a
/// host shared with other books included.
const MEASURE_BUDGET: Duration = Duration::from_secs(15 * 60);

async fn measure_one(
    fetcher: &PoliteFetcher,
    table: &PublisherTable,
    maps: &SitemapCache,
    book: MeasureBook,
) -> Measured {
    let mut m = Measured {
        isbn: book.isbn.clone(),
        title: book.title.clone(),
        publisher: book.publisher.clone().unwrap_or_default(),
        library_cover: book.cover_url.clone().unwrap_or_default(),
        outcome: "bad_isbn",
        site: String::new(),
        recipe: String::new(),
        page: String::new(),
        image: String::new(),
    };
    let Some(isbn) = rust_lib_app::utils::isbn::to_isbn13(&book.isbn) else {
        return m;
    };
    m.isbn = isbn.clone();
    let query = BookQuery {
        isbn: isbn.clone(),
        title: book.title,
        authors: book.authors,
        publisher: book.publisher,
    };
    // The books of one host wait for each other's turn there (Gallmeister
    // asks 10 s between two requests): the app's deadline would measure the
    // queue, not the recipes.
    match find_book_page_within(fetcher, table, &query, maps, MEASURE_BUDGET).await {
        Lookup::InvalidIsbn => m.outcome = "bad_isbn",
        Lookup::NoSite => m.outcome = "no_site",
        Lookup::NotFound {
            sites,
            refused,
            unreachable,
        } => {
            // A site that did not answer proves nothing: the book is not
            // counted missing, whatever the other sites said.
            m.outcome = if !unreachable.is_empty() {
                "unreachable"
            } else if !refused.is_empty() && refused.len() == sites.len() {
                "refused"
            } else {
                "not_found"
            };
            m.site = sites.join(", ");
        }
        Lookup::Found { site, kind, page } => {
            m.site = site;
            m.recipe = match kind {
                RecipeKind::Product => "product",
                RecipeKind::Search => "search",
                RecipeKind::Sitemap => "sitemap",
            }
            .into();
            m.page = page.url.to_string();
            // The image as the app takes it: robots.txt, the pause, and an
            // image the app's pipeline can decode.
            m.outcome = match page_cover(fetcher, &page, &isbn).await {
                Ok(cover) => {
                    m.image = cover.image_url;
                    "found"
                }
                Err(PageCoverError::Forbidden) => "image_forbidden",
                Err(PageCoverError::Broken) => "image_broken",
                Err(PageCoverError::Unreachable) => "unreachable",
                Err(PageCoverError::NoImage) => "no_image",
            };
        }
    }
    m
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Only http(s) addresses go into the sheet's links and images.
fn safe_url(s: &str) -> String {
    match Url::parse(s) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => escape_html(u.as_str()),
        _ => String::new(),
    }
}

/// The contact sheet: every book found, its cover beside the library's, then
/// the books not found, by outcome.
fn contact_sheet(results: &[Measured]) -> String {
    let mut cards = String::new();
    for r in results.iter().filter(|r| r.outcome == "found") {
        let library = if r.library_cover.is_empty() {
            "<div class=\"none\">no cover in the library</div>".to_owned()
        } else {
            format!(
                "<img src=\"{}\" alt=\"Library cover\" loading=\"lazy\">",
                safe_url(&r.library_cover)
            )
        };
        cards.push_str(&format!(
            "<figure><div class=\"pair\">{library}<img src=\"{}\" alt=\"Cover found\" loading=\"lazy\"></div>\
             <figcaption><b>{}</b><br>{} · {}<br><a href=\"{}\">{} ({})</a></figcaption></figure>\n",
            safe_url(&r.image),
            escape_html(&r.title),
            escape_html(&r.isbn),
            escape_html(&r.publisher),
            safe_url(&r.page),
            escape_html(&r.site),
            escape_html(&r.recipe),
        ));
    }
    let mut rows = String::new();
    for r in results.iter().filter(|r| r.outcome != "found") {
        rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
            escape_html(r.outcome),
            escape_html(&r.isbn),
            escape_html(&r.title),
            escape_html(&r.publisher),
            escape_html(&r.site),
        ));
    }
    let found = results.iter().filter(|r| r.outcome == "found").count();
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="no-referrer">
<title>Cover measure</title>
<style>
:root {{ color-scheme: light dark; --bg: #fafaf7; --fg: #1d1d1b; --muted: #6b6b66; --line: #ddd; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg: #1b1b1a; --fg: #eee; --muted: #aaa; --line: #444; }} }}
body {{ background: var(--bg); color: var(--fg); font: 14px/1.4 system-ui, sans-serif; margin: 16px; }}
.grid {{ display: grid; grid-template-columns: repeat(auto-fill, minmax(260px, 1fr)); gap: 16px; }}
figure {{ margin: 0; border: 1px solid var(--line); padding: 8px; }}
.pair {{ display: flex; gap: 8px; height: 180px; }}
.pair img, .none {{ flex: 1; min-width: 0; object-fit: contain; height: 100%; }}
.none {{ display: flex; align-items: center; justify-content: center; color: var(--muted); border: 1px dashed var(--line); font-size: 12px; text-align: center; }}
figcaption {{ margin-top: 6px; font-size: 12px; overflow-wrap: anywhere; }}
a {{ color: inherit; }}
table {{ border-collapse: collapse; width: 100%; font-size: 12px; }}
td {{ border-top: 1px solid var(--line); padding: 4px; overflow-wrap: anywhere; }}
</style></head><body>
<h1>Cover measure</h1>
<p>{found} of {total} books found. In each pair: the library's cover on the left, the one found on the right. Look for a wrong one.</p>
<div class="grid">
{cards}</div>
<h2>Not found</h2>
<table><tbody>
{rows}</tbody></table>
</body></html>
"#,
        total = results.len(),
    )
}
