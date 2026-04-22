use std::{
    collections::HashMap,
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, HeaderValue},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, Utc};
use regex::Regex;
use reqwest::Client;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

const OWNER: &str = "zanfranceschi";
const REPO: &str = "rinha-de-backend-2026";
const RESULTS_BOT: &str = "arinhadebackend";
const REFRESH_INTERVAL: Duration = Duration::from_secs(300);

#[tokio::main]
async fn main() {
    let app_state = AppState {
        github: GitHubClient::new().expect("failed to initialize GitHub client"),
        leaderboard_state: Arc::new(RwLock::new(LeaderboardState::default())),
    };
    let refresh_state = app_state.clone();

    let port = env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(3000);
    let address = SocketAddr::from(([0, 0, 0, 0], port));

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/api/leaderboard", get(api_handler))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind(address)
        .await
        .expect("failed to bind TCP listener");

    tokio::spawn(async move {
        leaderboard_refresh_loop(refresh_state).await;
    });

    println!("Leaderboard listening on http://{address}");

    axum::serve(listener, app)
        .await
        .expect("failed to serve application");
}

#[derive(Clone)]
struct AppState {
    github: GitHubClient,
    leaderboard_state: Arc<RwLock<LeaderboardState>>,
}

#[derive(Clone, Default)]
struct LeaderboardState {
    leaderboard: Vec<LeaderboardEntry>,
    last_refreshed_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
}

#[derive(Clone)]
struct GitHubClient {
    client: Client,
    cache_db_path: PathBuf,
}

#[derive(Clone, Debug)]
struct SourcedIssue {
    issue: GitHubIssue,
    source: IssueSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IssueSource {
    Fresh,
    Cached,
}

#[derive(Clone, Copy, Debug)]
enum HttpFetchPolicy {
    CacheFirst,
    NetworkFirst,
}

#[derive(Clone, Debug)]
struct CachedResponse {
    body: String,
}

#[derive(Clone, Debug)]
struct StoredIssueResult {
    result_json: String,
}

impl GitHubClient {
    fn new() -> Result<Self, AppError> {
        let mut headers = HeaderMap::new();
        headers.insert("Accept", HeaderValue::from_static("application/vnd.github+json"));
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static("2022-11-28"),
        );

        if let Ok(token) = env::var("GITHUB_TOKEN") {
            let value = format!("Bearer {token}");

            if let Ok(header) = HeaderValue::from_str(&value) {
                headers.insert("Authorization", header);
            }
        }

        let client = Client::builder()
            .default_headers(headers)
            .user_agent("rinha-leaderboard")
            .build()
            .expect("failed to build reqwest client");

        let cache_db_path = PathBuf::from(
            env::var("CACHE_DB_PATH").unwrap_or_else(|_| "leaderboard-cache.sqlite3".to_string()),
        );
        initialize_http_cache(&cache_db_path)?;

        Ok(Self {
            client,
            cache_db_path,
        })
    }

    async fn leaderboard(&self) -> Result<Vec<LeaderboardEntry>, AppError> {
        let issues = self.fetch_all_closed_issues().await?;
        let json_block = Regex::new(r"(?s)```json\s*(\{.*?\})\s*```")
            .map_err(|error| AppError::Internal(error.to_string()))?;

        let mut entries = Vec::new();
        for sourced_issue in issues {
            let Some(result) = self.fetch_issue_result(&sourced_issue, &json_block).await? else {
                continue;
            };

            if let Some(entry) = LeaderboardEntry::from_issue_result(&sourced_issue.issue, result) {
                entries.push(entry);
            }
        }

        let mut entries = best_entry_per_user(entries);
        entries.sort_by(compare_entries);

        for (index, entry) in entries.iter_mut().enumerate() {
            entry.rank = index + 1;
        }

        Ok(entries)
    }

    async fn fetch_all_closed_issues(&self) -> Result<Vec<SourcedIssue>, AppError> {
        let mut page = 1;
        let mut issues = Vec::new();
        let mut use_cached_tail = false;
        let mut updated_pages = Vec::new();
        let mut page_logs = Vec::new();

        loop {
            let url = format!(
                "https://api.github.com/repos/{OWNER}/{REPO}/issues?state=closed&per_page=100&page={page}"
            );
            let cached = self.read_cached_entry(&url).await?;
            let (body, source) = if use_cached_tail {
                if let Some(cached) = cached {
                    page_logs.push(format!("page {page}: reused cached tail"));
                    (cached.body, IssueSource::Cached)
                } else {
                    let body = self.fetch_body_from_network(&url).await?;
                    self.write_cached_body(&url, &body).await?;
                    updated_pages.push(page);
                    page_logs.push(format!("page {page}: fetched fresh (cached tail missing)"));
                    (body, IssueSource::Fresh)
                }
            } else {
                match self.fetch_body_from_network(&url).await {
                    Ok(body) => {
                        let is_unchanged = cached
                            .as_ref()
                            .map(|cached| cached.body == body)
                            .unwrap_or(false);
                        self.write_cached_body(&url, &body).await?;
                        if is_unchanged {
                            use_cached_tail = true;
                            page_logs.push(format!(
                                "page {page}: no change from GitHub, remaining pages can reuse cache"
                            ));
                        } else {
                            updated_pages.push(page);
                            page_logs.push(format!("page {page}: updated from GitHub"));
                        }
                        (body, IssueSource::Fresh)
                    }
                    Err(error) => {
                        if let Some(cached) = cached {
                            use_cached_tail = true;
                            page_logs.push(format!(
                                "page {page}: GitHub failed ({error}), reused cached page"
                            ));
                            (cached.body, IssueSource::Cached)
                        } else {
                            return Err(error);
                        }
                    }
                }
            };

            let batch = parse_json_body::<Vec<GitHubIssue>>(&body)?;
            let batch_len = batch.len();

            issues.extend(
                batch
                    .into_iter()
                    .filter(|issue| issue.pull_request.is_none() && issue.comments > 0)
                    .map(|issue| SourcedIssue { issue, source }),
            );

            if batch_len < 100 {
                break;
            }

            page += 1;
        }

        if updated_pages.is_empty() {
            println!(
                "leaderboard refresh: no new issue pages found; {}",
                page_logs.join(" | ")
            );
        } else {
            println!(
                "leaderboard refresh: updated issue pages {:?}; {}",
                updated_pages,
                page_logs.join(" | ")
            );
        }

        Ok(issues)
    }

    async fn fetch_issue_result(
        &self,
        issue: &SourcedIssue,
        json_block: &Regex,
    ) -> Result<Option<BenchmarkResult>, AppError> {
        if let Some(stored) = self.read_stored_issue_result(issue.issue.number).await? {
            println!(
                "leaderboard refresh: issue #{} reused stored result without refetching comments",
                issue.issue.number
            );
            return serde_json::from_str::<BenchmarkResult>(&stored.result_json)
                .map(Some)
                .map_err(|error| AppError::Internal(format!("invalid stored benchmark JSON: {error}")));
        }

        let url = format!(
            "https://api.github.com/repos/{OWNER}/{REPO}/issues/{issue_number}/comments?per_page=100",
            issue_number = issue.issue.number,
        );
        let comments = self
            .get_json_with_policy::<Vec<GitHubComment>>(
                &url,
                match issue.source {
                    IssueSource::Fresh => HttpFetchPolicy::NetworkFirst,
                    IssueSource::Cached => HttpFetchPolicy::CacheFirst,
                },
            )
            .await?;

        let Some(comment) = comments.into_iter().find(|comment| {
            comment.user.login == RESULTS_BOT && comment.body.contains("```json")
        }) else {
            return Ok(None);
        };

        let Some(captures) = json_block.captures(&comment.body) else {
            return Ok(None);
        };
        let Some(matched_json) = captures.get(1) else {
            return Ok(None);
        };

        self.write_issue_result(issue.issue.number, matched_json.as_str())
            .await?;

        let parsed = serde_json::from_str::<BenchmarkResult>(matched_json.as_str())
            .map_err(|error| AppError::Internal(format!("invalid benchmark JSON: {error}")))?;

        Ok(Some(parsed))
    }

    async fn get_json_with_policy<T>(&self, url: &str, policy: HttpFetchPolicy) -> Result<T, AppError>
    where
        T: serde::de::DeserializeOwned,
    {
        match policy {
            HttpFetchPolicy::CacheFirst => {
                if let Some(cached) = self.read_cached_entry(url).await? {
                    return parse_json_body::<T>(&cached.body);
                }

                let body = self.fetch_body_from_network(url).await?;
                self.write_cached_body(url, &body).await?;
                parse_json_body::<T>(&body)
            }
            HttpFetchPolicy::NetworkFirst => match self.fetch_body_from_network(url).await {
                Ok(body) => {
                    self.write_cached_body(url, &body).await?;
                    parse_json_body::<T>(&body)
                }
                Err(error) => {
                    if let Some(cached) = self.read_cached_entry(url).await? {
                        parse_json_body::<T>(&cached.body)
                    } else {
                        Err(error)
                    }
                }
            },
        }
    }

    async fn fetch_body_from_network(&self, url: &str) -> Result<String, AppError> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|error| AppError::Upstream(format!("GitHub request failed: {error}")))?;

        if !response.status().is_success() {
            return Err(AppError::Upstream(format!(
                "GitHub returned {} for {url}",
                response.status()
            )));
        }

        response
            .text()
            .await
            .map_err(|error| AppError::Upstream(format!("GitHub response read failed: {error}")))
    }

    async fn read_cached_entry(&self, url: &str) -> Result<Option<CachedResponse>, AppError> {
        let path = self.cache_db_path.clone();
        let url = url.to_string();

        tokio::task::spawn_blocking(move || -> Result<Option<CachedResponse>, AppError> {
            let connection = rusqlite::Connection::open(path)
                .map_err(|error| AppError::Internal(format!("failed to open cache database: {error}")))?;

            let cached = connection
                .query_row(
                    "SELECT body, fetched_at_unix FROM github_http_cache WHERE url = ?1",
                    [url],
                    |row| {
                        Ok(CachedResponse {
                            body: row.get::<_, String>(0)?,
                        })
                    },
                )
                .optional()
                .map_err(|error| AppError::Internal(format!("failed to read cache entry: {error}")))?;

            Ok(cached)
        })
        .await
        .map_err(|error| AppError::Internal(format!("cache read task failed: {error}")))?
    }

    async fn write_cached_body(&self, url: &str, body: &str) -> Result<(), AppError> {
        let path = self.cache_db_path.clone();
        let url = url.to_string();
        let body = body.to_string();

        tokio::task::spawn_blocking(move || -> Result<(), AppError> {
            let connection = rusqlite::Connection::open(path)
                .map_err(|error| AppError::Internal(format!("failed to open cache database: {error}")))?;

            connection
                .execute(
                    "INSERT INTO github_http_cache (url, body, fetched_at_unix)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(url) DO UPDATE SET
                         body = excluded.body,
                         fetched_at_unix = excluded.fetched_at_unix",
                    rusqlite::params![url, body, Utc::now().timestamp()],
                )
                .map_err(|error| AppError::Internal(format!("failed to write cache entry: {error}")))?;

            Ok(())
        })
        .await
        .map_err(|error| AppError::Internal(format!("cache write task failed: {error}")))?
    }

    async fn read_stored_issue_result(
        &self,
        issue_number: u64,
    ) -> Result<Option<StoredIssueResult>, AppError> {
        let path = self.cache_db_path.clone();
        let issue_number = issue_number as i64;

        tokio::task::spawn_blocking(move || -> Result<Option<StoredIssueResult>, AppError> {
            let connection = rusqlite::Connection::open(path)
                .map_err(|error| AppError::Internal(format!("failed to open cache database: {error}")))?;

            let stored = connection
                .query_row(
                    "SELECT result_json FROM issue_results WHERE issue_number = ?1",
                    [issue_number],
                    |row| {
                        Ok(StoredIssueResult {
                            result_json: row.get::<_, String>(0)?,
                        })
                    },
                )
                .optional()
                .map_err(|error| AppError::Internal(format!("failed to read stored issue result: {error}")))?;

            Ok(stored)
        })
        .await
        .map_err(|error| AppError::Internal(format!("stored issue result read task failed: {error}")))?
    }

    async fn write_issue_result(&self, issue_number: u64, result_json: &str) -> Result<(), AppError> {
        let path = self.cache_db_path.clone();
        let issue_number = issue_number as i64;
        let result_json = result_json.to_string();

        tokio::task::spawn_blocking(move || -> Result<(), AppError> {
            let connection = rusqlite::Connection::open(path)
                .map_err(|error| AppError::Internal(format!("failed to open cache database: {error}")))?;

            connection
                .execute(
                    "INSERT INTO issue_results (issue_number, result_json, stored_at_unix)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(issue_number) DO UPDATE SET
                         result_json = excluded.result_json,
                         stored_at_unix = excluded.stored_at_unix",
                    rusqlite::params![issue_number, result_json, Utc::now().timestamp()],
                )
                .map_err(|error| AppError::Internal(format!("failed to write stored issue result: {error}")))?;

            Ok(())
        })
        .await
        .map_err(|error| AppError::Internal(format!("stored issue result write task failed: {error}")))?
    }
}

fn initialize_http_cache(path: &Path) -> Result<(), AppError> {
    let connection = rusqlite::Connection::open(path)
        .map_err(|error| AppError::Internal(format!("failed to open cache database: {error}")))?;

    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS github_http_cache (
                url TEXT PRIMARY KEY,
                body TEXT NOT NULL,
                fetched_at_unix INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS issue_results (
                issue_number INTEGER PRIMARY KEY,
                result_json TEXT NOT NULL,
                stored_at_unix INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_github_http_cache_fetched_at
                ON github_http_cache (fetched_at_unix);
            CREATE INDEX IF NOT EXISTS idx_issue_results_stored_at
                ON issue_results (stored_at_unix);",
        )
        .map_err(|error| AppError::Internal(format!("failed to initialize cache database: {error}")))?;

    Ok(())
}

fn parse_json_body<T>(body: &str) -> Result<T, AppError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_str::<T>(body)
        .map_err(|error| AppError::Upstream(format!("GitHub JSON decode failed: {error}")))
}

async fn index_handler(State(state): State<AppState>) -> Response {
    let snapshot = state.leaderboard_state.read().await.clone();
    Html(render_html(&snapshot)).into_response()
}

async fn api_handler(State(state): State<AppState>) -> Response {
    let snapshot = state.leaderboard_state.read().await.clone();
    Json(snapshot.leaderboard).into_response()
}

async fn leaderboard_refresh_loop(state: AppState) {
    loop {
        let started_at = Utc::now();
        match state.github.leaderboard().await {
            Ok(leaderboard) => {
                let mut snapshot = state.leaderboard_state.write().await;
                log_leaderboard_updates(&snapshot.leaderboard, &leaderboard);
                snapshot.leaderboard = leaderboard;
                snapshot.last_refreshed_at = Some(Utc::now());
                snapshot.last_error = None;
                println!(
                    "leaderboard refresh succeeded: {} entries at {}",
                    snapshot.leaderboard.len(),
                    started_at.format("%Y-%m-%d %H:%M:%S UTC")
                );
            }
            Err(error) => {
                let mut snapshot = state.leaderboard_state.write().await;
                snapshot.last_error = Some(error.to_string());
                eprintln!("leaderboard refresh failed: {error}");
            }
        }

        tokio::time::sleep(REFRESH_INTERVAL).await;
    }
}

fn log_leaderboard_updates(previous: &[LeaderboardEntry], current: &[LeaderboardEntry]) {
    let previous_by_user = previous
        .iter()
        .map(|entry| (entry.username.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let current_by_user = current
        .iter()
        .map(|entry| (entry.username.as_str(), entry))
        .collect::<HashMap<_, _>>();

    let mut changes = Vec::new();

    for entry in current {
        match previous_by_user.get(entry.username.as_str()) {
            None => changes.push(format!(
                "new user {} -> issue #{}, score {}, p99 {}",
                entry.username,
                entry.issue_number,
                entry.final_score,
                format_p99(entry.p99_ms)
            )),
            Some(previous_entry)
                if previous_entry.issue_number != entry.issue_number
                    || previous_entry.final_score != entry.final_score
                    || previous_entry.p99_ms != entry.p99_ms =>
            {
                changes.push(format!(
                    "{} updated: issue #{} score {} p99 {} -> issue #{} score {} p99 {}",
                    entry.username,
                    previous_entry.issue_number,
                    previous_entry.final_score,
                    format_p99(previous_entry.p99_ms),
                    entry.issue_number,
                    entry.final_score,
                    format_p99(entry.p99_ms)
                ));
            }
            Some(_) => {}
        }
    }

    for entry in previous {
        if !current_by_user.contains_key(entry.username.as_str()) {
            changes.push(format!(
                "user {} removed from leaderboard (was issue #{} score {} p99 {})",
                entry.username,
                entry.issue_number,
                entry.final_score,
                format_p99(entry.p99_ms)
            ));
        }
    }

    if changes.is_empty() {
        println!("leaderboard refresh: no score changes");
    } else {
        println!("leaderboard refresh: {}", changes.join(" | "));
    }
}

fn best_entry_per_user(entries: Vec<LeaderboardEntry>) -> Vec<LeaderboardEntry> {
    let mut best = HashMap::<String, LeaderboardEntry>::new();

    for entry in entries {
        best.entry(entry.username.clone())
            .and_modify(|current| {
                if compare_entries(&entry, current).is_lt() {
                    *current = entry.clone();
                }
            })
            .or_insert(entry);
    }

    best.into_values().collect()
}

fn compare_entries(left: &LeaderboardEntry, right: &LeaderboardEntry) -> std::cmp::Ordering {
    right
        .final_score
        .cmp(&left.final_score)
        .then_with(|| compare_optional_f64(left.p99_ms, right.p99_ms))
        .then_with(|| right.issue_number.cmp(&left.issue_number))
}

fn compare_optional_f64(left: Option<f64>, right: Option<f64>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left
            .partial_cmp(&right)
            .unwrap_or(std::cmp::Ordering::Equal),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn format_p99(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.2}ms"))
        .unwrap_or_else(|| "-".to_string())
}

fn parse_ms(value: Option<&str>) -> Option<f64> {
    value.and_then(|raw| raw.strip_suffix("ms")).and_then(|raw| raw.parse().ok())
}

fn format_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

fn round_to_i64(value: Option<f64>) -> Option<i64> {
    value.map(|number| number.round() as i64)
}

fn format_timestamp(value: &str) -> String {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| {
            timestamp
                .with_timezone(&Utc)
                .format("%Y-%m-%d %H:%M UTC")
                .to_string()
        })
        .unwrap_or_else(|_| value.to_string())
}

fn render_html(snapshot: &LeaderboardState) -> String {
    let entries = &snapshot.leaderboard;
    let generated_at = snapshot
        .last_refreshed_at
        .map(|timestamp| timestamp.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "warming up".to_string());
    let leader_score = entries.first().map(|entry| entry.final_score).unwrap_or_default();
    let status_message = snapshot
        .last_error
        .as_ref()
        .map(|error| format!("Last refresh failed: {error}"))
        .unwrap_or_else(|| "Background refresh runs every 5 minutes.".to_string());

    let rows = entries
        .iter()
        .map(|entry| {
            format!(
                "<tr>\
                    <td class=\"rank\">#{rank}</td>\
                    <td><a href=\"{issue_url}\" target=\"_blank\" rel=\"noreferrer\">{username}</a><span class=\"muted\">issue #{issue_number}</span></td>\
                    <td>{score}</td>\
                    <td>{accuracy}</td>\
                    <td>{p99}</td>\
                    <td>{p90}</td>\
                    <td>{med}</td>\
                    <td>{cpu}</td>\
                    <td>{mem}</td>\
                    <td>{http_errors}</td>\
                    <td title=\"{breakdown_title}\">{tp}/{tn}/{fp}/{fn}</td>\
                    <td><code>{commit}</code></td>\
                    <td>{updated_at}</td>\
                </tr>",
                rank = entry.rank,
                issue_url = escape_html(&entry.issue_url),
                username = escape_html(&entry.username),
                issue_number = entry.issue_number,
                score = entry.final_score,
                accuracy = escape_html(&entry.detection_accuracy),
                p99 = entry
                    .p99_ms
                    .map(|value| format!("{value:.2}ms"))
                    .unwrap_or_else(|| "-".to_string()),
                p90 = entry
                    .p90_ms
                    .map(|value| format!("{value:.2}ms"))
                    .unwrap_or_else(|| "-".to_string()),
                med = entry
                    .median_ms
                    .map(|value| format!("{value:.2}ms"))
                    .unwrap_or_else(|| "-".to_string()),
                cpu = entry
                    .cpu
                    .map(format_number)
                    .unwrap_or_else(|| "-".to_string()),
                mem = entry
                    .memory_mb
                    .map(format_number)
                    .unwrap_or_else(|| "-".to_string()),
                http_errors = entry.http_errors,
                breakdown_title = escape_html(&format!(
                    "true positives: {}, true negatives: {}, false positives: {}, false negatives: {}",
                    entry.true_positives, entry.true_negatives, entry.false_positives, entry.false_negatives
                )),
                tp = entry.true_positives,
                tn = entry.true_negatives,
                fp = entry.false_positives,
                fn = entry.false_negatives,
                commit = escape_html(entry.commit.as_deref().unwrap_or("-")),
                updated_at = escape_html(&format_timestamp(&entry.created_at)),
            )
        })
        .collect::<Vec<_>>()
        .join("");

    format!(
        "<!DOCTYPE html>
<html lang=\"en\">
<head>
  <meta charset=\"utf-8\" />
  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />
  <title>Rinha de Backend 2026 Leaderboard</title>
  <style>
    :root {{ color-scheme: dark; }}
    * {{ box-sizing: border-box; }}
    body {{ margin: 0; font-family: Inter, ui-sans-serif, system-ui, sans-serif; background: #09111f; color: #ebf2ff; }}
    main {{ max-width: 1440px; margin: 0 auto; padding: 32px 20px 48px; }}
    h1 {{ margin: 0; font-size: clamp(2rem, 3vw, 3rem); }}
    p {{ color: #b5c3de; line-height: 1.6; }}
    .hero {{ display: grid; gap: 16px; margin-bottom: 24px; }}
    .cards {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(180px, 1fr)); gap: 16px; margin: 24px 0; }}
    .card, .table-wrap {{ background: rgba(11, 20, 39, 0.88); border: 1px solid #22314f; border-radius: 18px; box-shadow: 0 12px 40px rgba(0,0,0,.22); }}
    .card {{ padding: 20px; }}
    .label {{ display: block; font-size: 0.8rem; text-transform: uppercase; color: #7f91b2; margin-bottom: 8px; letter-spacing: .08em; }}
    .value {{ font-size: 1.9rem; font-weight: 700; }}
    .table-wrap {{ overflow: auto; }}
    table {{ width: 100%; border-collapse: collapse; min-width: 1200px; }}
    th, td {{ padding: 14px 16px; text-align: left; border-bottom: 1px solid #1b2742; vertical-align: top; }}
    th {{ position: sticky; top: 0; background: #0c1528; color: #8fa4cb; font-size: 0.8rem; text-transform: uppercase; letter-spacing: .06em; }}
    tr:hover td {{ background: rgba(21, 34, 61, 0.78); }}
    a {{ color: #88b4ff; text-decoration: none; font-weight: 600; display: block; }}
    a:hover {{ text-decoration: underline; }}
    .muted {{ display: block; color: #6f81a5; margin-top: 4px; font-size: 0.88rem; }}
    .rank {{ font-weight: 700; color: #ffd166; }}
    code {{ font-family: ui-monospace, SFMono-Regular, monospace; color: #d6e3ff; }}
    .footer {{ margin-top: 16px; color: #7f91b2; font-size: .95rem; }}
    .empty {{ padding: 32px; text-align: center; color: #9eb0cf; }}
  </style>
</head>
<body>
  <main>
    <section class=\"hero\">
      <div>
        <h1>Rinha de Backend 2026 Leaderboard</h1>
        <p>All closed submissions are evaluated, and the <strong>best result per GitHub user</strong> is kept. Ranking is based on <strong>final_score</strong>, with lower p99 breaking ties.</p>
      </div>
      <div class=\"cards\">
        <article class=\"card\"><span class=\"label\">Participants</span><span class=\"value\">{participant_count}</span></article>
        <article class=\"card\"><span class=\"label\">Top score</span><span class=\"value\">{leader_score}</span></article>
        <article class=\"card\"><span class=\"label\">Refresh interval</span><span class=\"value\">5 min</span></article>
        <article class=\"card\"><span class=\"label\">Updated</span><span class=\"value\">{generated_at}</span></article>
      </div>
    </section>
    <section class=\"table-wrap\">
      {table_content}
    </section>
    <p class=\"footer\">JSON API available at <code>/api/leaderboard</code>. {status_message}</p>
  </main>
</body>
</html>",
        participant_count = entries.len(),
        leader_score = leader_score,
        generated_at = generated_at,
        status_message = escape_html(&status_message),
        table_content = if entries.is_empty() {
            "<div class=\"empty\">No leaderboard snapshot yet. The background sync may still be warming up, or GitHub may have rejected the latest refresh.</div>".to_string()
        } else {
            format!(
                "<table>
                    <thead>
                        <tr>
                            <th>Rank</th>
                            <th>User</th>
                            <th>Final score</th>
                            <th>Accuracy</th>
                            <th>P99</th>
                            <th>P90</th>
                            <th>Median</th>
                            <th>CPU</th>
                            <th>Mem (MB)</th>
                            <th>HTTP errors</th>
                            <th>Breakdown</th>
                            <th>Commit</th>
                            <th>Issue time</th>
                        </tr>
                    </thead>
                    <tbody>{rows}</tbody>
                </table>",
                rows = rows,
            )
        }
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[derive(Clone, Debug, Serialize)]
struct LeaderboardEntry {
    rank: usize,
    username: String,
    issue_number: u64,
    issue_url: String,
    issue_title: String,
    created_at: String,
    final_score: i64,
    raw_score: Option<i64>,
    detection_accuracy: String,
    latency_multiplier: Option<f64>,
    p99_ms: Option<f64>,
    p90_ms: Option<f64>,
    median_ms: Option<f64>,
    true_positives: i64,
    true_negatives: i64,
    false_positives: i64,
    false_negatives: i64,
    http_errors: i64,
    cpu: Option<f64>,
    memory_mb: Option<f64>,
    commit: Option<String>,
}

impl LeaderboardEntry {
    fn from_issue_result(issue: &GitHubIssue, result: BenchmarkResult) -> Option<Self> {
        let scoring = result.test_results.scoring?;
        let response_times = result.test_results.response_times;
        let breakdown = scoring.breakdown.unwrap_or_default();

        Some(Self {
            rank: 0,
            username: issue.user.login.clone(),
            issue_number: issue.number,
            issue_url: issue.html_url.clone(),
            issue_title: issue.title.clone(),
            created_at: issue.created_at.clone(),
            final_score: round_to_i64(scoring.final_score)?,
            raw_score: round_to_i64(scoring.raw_score),
            detection_accuracy: scoring
                .detection_accuracy
                .unwrap_or_else(|| "-".to_string()),
            latency_multiplier: scoring.latency_multiplier,
            p99_ms: parse_ms(response_times.p99.as_deref()),
            p90_ms: parse_ms(response_times.p90.as_deref()),
            median_ms: parse_ms(response_times.med.as_deref()),
            true_positives: round_to_i64(breakdown.true_positive_detections).unwrap_or_default(),
            true_negatives: round_to_i64(breakdown.true_negative_detections).unwrap_or_default(),
            false_positives: round_to_i64(breakdown.false_positive_detections).unwrap_or_default(),
            false_negatives: round_to_i64(breakdown.false_negative_detections).unwrap_or_default(),
            http_errors: round_to_i64(breakdown.http_errors).unwrap_or_default(),
            cpu: result.runtime_info.cpu,
            memory_mb: result.runtime_info.mem,
            commit: result.runtime_info.commit,
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
struct GitHubIssue {
    number: u64,
    title: String,
    html_url: String,
    created_at: String,
    comments: u64,
    user: GitHubUser,
    pull_request: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize)]
struct GitHubComment {
    body: String,
    user: GitHubUser,
}

#[derive(Clone, Debug, Deserialize)]
struct GitHubUser {
    login: String,
}

#[derive(Clone, Debug, Deserialize)]
struct BenchmarkResult {
    #[serde(rename = "runtime-info")]
    runtime_info: RuntimeInfo,
    #[serde(rename = "test-results")]
    test_results: TestResults,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RuntimeInfo {
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    mem: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    cpu: Option<f64>,
    commit: Option<String>,
}

fn deserialize_optional_number<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;

    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(number)) => number
            .as_f64()
            .ok_or_else(|| serde::de::Error::custom("invalid numeric value"))
            .map(Some),
        Some(serde_json::Value::String(raw)) => raw
            .parse::<f64>()
            .map(Some)
            .map_err(|_| serde::de::Error::custom("invalid numeric string")),
        Some(_) => Err(serde::de::Error::custom("expected numeric value")),
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct TestResults {
    #[serde(default)]
    response_times: ResponseTimes,
    scoring: Option<Scoring>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ResponseTimes {
    p99: Option<String>,
    p90: Option<String>,
    med: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct Scoring {
    breakdown: Option<Breakdown>,
    detection_accuracy: Option<String>,
    latency_multiplier: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    raw_score: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    final_score: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Breakdown {
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    false_positive_detections: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    false_negative_detections: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    true_positive_detections: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    true_negative_detections: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    http_errors: Option<f64>,
}

#[derive(Clone, Debug)]
enum AppError {
    Internal(String),
    Upstream(String),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Internal(message) | Self::Upstream(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": self.to_string() })),
        )
            .into_response()
    }
}
