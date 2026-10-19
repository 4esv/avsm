//! Adapters: built-in metrics and levers that reach the world. They run in
//! the warden, each with the one key it is scoped to; a role never sees a
//! key and a venture never runs an adapter itself. A venture directory
//! holding only a mandate is complete when every metric and lever is one.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::config::Adapters;
use crate::record::{Payload, Takes};
use crate::warden;

/// How long an adapter may take to answer.
pub const TIMEOUT: Duration = Duration::from_secs(60);

/// The built-in metrics, by name.
pub const METRICS: &[&str] = &[
    "stripe.mrr",
    "stripe.charges_today",
    "github.open_issues",
    "github.failed_ci",
    "imap.unread",
    "http.status",
    "file.number",
    "shell",
];

/// The built-in levers, by name.
pub const LEVERS: &[&str] = &[
    "github.issue",
    "github.comment",
    "github.pr",
    "mail.send",
    "http.post",
];

/// A metric an adapter measures.
#[derive(Clone, Debug, PartialEq)]
pub enum Metric {
    /// Monthly recurring revenue from active Stripe subscriptions.
    StripeMrr,
    /// The sum of Stripe charges that succeeded since midnight UTC.
    StripeChargesToday,
    /// Open issues in a repository, pull requests not counted.
    GithubOpenIssues { repo: String },
    /// Failed check runs on the head of the default branch.
    GithubFailedCi { repo: String },
    /// Unseen messages in INBOX over IMAP; TLS on port 993.
    ImapUnread { host: String, port: u16 },
    /// The status code a GET returns.
    HttpStatus { url: String },
    /// The last number in a file in the venture directory.
    FileNumber { path: String },
    /// The last number a shell command prints.
    Shell { command: String },
}

impl Metric {
    /// Read a `metric:` value. None means it names no adapter: a plain
    /// metric the venture's own `report` prints.
    pub fn parse(spec: &str) -> Result<Option<Metric>, String> {
        let spec = spec.trim();
        let (name, arg) = match spec.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (spec, ""),
        };
        let needs = |what: &str, example: &str| -> Result<String, String> {
            if arg.is_empty() {
                Err(format!(
                    "{name} needs {what} after it, like: metric: {name} {example}"
                ))
            } else {
                Ok(arg.to_string())
            }
        };
        let none = |what: &str| -> Result<(), String> {
            if arg.is_empty() {
                Ok(())
            } else {
                Err(format!("{name} takes nothing after it; {what}"))
            }
        };
        Ok(Some(match name {
            "stripe.mrr" => {
                none("the stripe key in the keys file says which account")?;
                Metric::StripeMrr
            }
            "stripe.charges_today" => {
                none("the stripe key in the keys file says which account")?;
                Metric::StripeChargesToday
            }
            "github.open_issues" => Metric::GithubOpenIssues {
                repo: repo_name(&needs("a repository", "owner/repo")?)?,
            },
            "github.failed_ci" => Metric::GithubFailedCi {
                repo: repo_name(&needs("a repository", "owner/repo")?)?,
            },
            "imap.unread" => {
                let (host, port) = host_port(&needs("a mail server", "imap.example.com")?, 993)?;
                Metric::ImapUnread { host, port }
            }
            "http.status" => Metric::HttpStatus {
                url: url_of(&needs("an address", "https://example.com/health")?)?,
            },
            "file.number" => Metric::FileNumber {
                path: needs("a file path", "out/count")?,
            },
            "shell" => Metric::Shell {
                command: needs("a command", "\"df --output=pcent / | tail -1\"")?,
            },
            _ => return Ok(None),
        }))
    }

    /// The adapter's name: what the report records carry as the metric.
    pub fn name(&self) -> &'static str {
        match self {
            Metric::StripeMrr => "stripe.mrr",
            Metric::StripeChargesToday => "stripe.charges_today",
            Metric::GithubOpenIssues { .. } => "github.open_issues",
            Metric::GithubFailedCi { .. } => "github.failed_ci",
            Metric::ImapUnread { .. } => "imap.unread",
            Metric::HttpStatus { .. } => "http.status",
            Metric::FileNumber { .. } => "file.number",
            Metric::Shell { .. } => "shell",
        }
    }

    /// The name of the one key this adapter may read, if it needs one.
    pub fn key_name(&self) -> Option<&'static str> {
        match self {
            Metric::StripeMrr | Metric::StripeChargesToday => Some("stripe"),
            Metric::GithubOpenIssues { .. } | Metric::GithubFailedCi { .. } => Some("github"),
            Metric::ImapUnread { .. } => Some("imap"),
            _ => None,
        }
    }

    /// The host this adapter reaches, for a venture's network list.
    pub fn host(&self, adapters: &Adapters) -> Option<String> {
        match self {
            Metric::StripeMrr | Metric::StripeChargesToday => host_of(&adapters.stripe.url),
            Metric::GithubOpenIssues { .. } | Metric::GithubFailedCi { .. } => {
                host_of(&adapters.github.url)
            }
            Metric::ImapUnread { host, .. } => Some(host.clone()),
            Metric::HttpStatus { url } => host_of(url),
            Metric::FileNumber { .. } | Metric::Shell { .. } => None,
        }
    }

    /// Measure: the value, or None with a note saying what went wrong.
    pub fn measure(
        &self,
        dir: &Path,
        key: Option<&str>,
        adapters: &Adapters,
    ) -> (Option<f64>, String) {
        match self.read(dir, key, adapters) {
            Ok((v, note)) => (Some(v), note),
            Err(e) => (None, e),
        }
    }

    fn read(
        &self,
        dir: &Path,
        key: Option<&str>,
        adapters: &Adapters,
    ) -> Result<(f64, String), String> {
        match self {
            Metric::StripeMrr => {
                let key = key.ok_or("stripe.mrr needs a key named stripe in the keys file")?;
                let subs = stripe_list(
                    &format!(
                        "{}/subscriptions?status=active&limit=100",
                        adapters.stripe.url
                    ),
                    key,
                )?;
                let (mrr, n) = stripe_mrr(&subs);
                Ok((mrr, format!("{n} active subscriptions")))
            }
            Metric::StripeChargesToday => {
                let key =
                    key.ok_or("stripe.charges_today needs a key named stripe in the keys file")?;
                let since = midnight_utc(warden::now());
                let charges = stripe_list(
                    &format!(
                        "{}/charges?limit=100&created[gte]={since}",
                        adapters.stripe.url
                    ),
                    key,
                )?;
                let (total, n) = stripe_charges(&charges);
                Ok((total, format!("{n} charges since midnight UTC")))
            }
            Metric::GithubOpenIssues { repo } => {
                let mut open = 0usize;
                for page in 1..=10 {
                    let url = format!(
                        "{}/repos/{repo}/issues?state=open&per_page=100&page={page}",
                        adapters.github.url
                    );
                    let items = get_json(&url, &github_headers(key))?;
                    let items = items.as_array().cloned().unwrap_or_default();
                    open += items
                        .iter()
                        .filter(|i| i.get("pull_request").is_none())
                        .count();
                    if items.len() < 100 {
                        break;
                    }
                }
                Ok((open as f64, format!("open issues in {repo}")))
            }
            Metric::GithubFailedCi { repo } => {
                let info = get_json(
                    &format!("{}/repos/{repo}", adapters.github.url),
                    &github_headers(key),
                )?;
                let branch = info["default_branch"]
                    .as_str()
                    .unwrap_or("main")
                    .to_string();
                let runs = get_json(
                    &format!(
                        "{}/repos/{repo}/commits/{branch}/check-runs?per_page=100",
                        adapters.github.url
                    ),
                    &github_headers(key),
                )?;
                let (failed, total) = failed_checks(&runs);
                Ok((
                    failed as f64,
                    format!("{failed} of {total} checks failed on {branch}"),
                ))
            }
            Metric::ImapUnread { host, port } => {
                let key = key.ok_or(
                    "imap.unread needs a key named imap in the keys file, as user:password",
                )?;
                let (user, pass) = key.split_once(':').ok_or("the imap key is user:password")?;
                let unseen = imap_unseen(host, *port, user, pass)?;
                Ok((unseen as f64, format!("unseen in INBOX at {host}")))
            }
            Metric::HttpStatus { url } => {
                let (status, _) = http("GET", url, &[], None)?;
                Ok((f64::from(status), format!("GET {url}")))
            }
            Metric::FileNumber { path } => {
                let full = dir.join(path);
                let text = std::fs::read_to_string(&full)
                    .map_err(|e| format!("cannot read {}: {e}", full.display()))?;
                Ok((number_from(&text)?, String::new()))
            }
            Metric::Shell { command } => {
                let ran = warden::run(dir, "sh", &["-c", command], "", TIMEOUT);
                if !ran.ok {
                    return Err(format!("command failed: {}", ran.detail(TIMEOUT)));
                }
                Ok((number_from(&ran.stdout)?, String::new()))
            }
        }
    }
}

/// A lever an adapter pulls.
#[derive(Clone, Debug, PartialEq)]
pub enum Lever {
    /// Open an issue: the first line of the text is the title.
    GithubIssue { repo: String },
    /// Comment on an issue: the one named here, or the `#n` on the text's
    /// first line.
    GithubComment { repo: String, issue: Option<u64> },
    /// Apply a diff on a new branch of the checkout in the venture
    /// directory, push it, and open a pull request. Irreversible.
    GithubPr { repo: String },
    /// Send mail: the first line of the text is the subject. Irreversible.
    MailSend { to: String },
    /// Post JSON to an address. Irreversible.
    HttpPost { url: String },
}

/// What a lever adapter runs for: named in the title and body it writes.
pub struct Occasion<'a> {
    pub venture: &'a str,
    pub action: &'a str,
    /// The proof, rendered.
    pub proof: &'a str,
}

impl Lever {
    /// Read an `adapter:` value.
    pub fn parse(spec: &str) -> Result<Lever, String> {
        let spec = spec.trim();
        let (name, arg) = match spec.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (spec, ""),
        };
        let needs = |what: &str, example: &str| -> Result<String, String> {
            if arg.is_empty() {
                Err(format!(
                    "{name} needs {what} after it, like: adapter: {name} {example}"
                ))
            } else {
                Ok(arg.to_string())
            }
        };
        Ok(match name {
            "github.issue" => Lever::GithubIssue {
                repo: repo_name(&needs("a repository", "owner/repo")?)?,
            },
            "github.comment" => {
                let arg = needs("a repository", "owner/repo or owner/repo#12")?;
                let (repo, issue) = match arg.split_once('#') {
                    Some((r, n)) => (
                        r.to_string(),
                        Some(n.parse::<u64>().map_err(|_| {
                            format!("github.comment: after # comes an issue number, not {n:?}")
                        })?),
                    ),
                    None => (arg, None),
                };
                Lever::GithubComment {
                    repo: repo_name(&repo)?,
                    issue,
                }
            }
            "github.pr" => Lever::GithubPr {
                repo: repo_name(&needs("a repository", "owner/repo")?)?,
            },
            "mail.send" => {
                let to = needs("an address", "you@example.com")?;
                if !to.contains('@') || to.contains(char::is_whitespace) {
                    return Err(format!("mail.send: {to:?} is not a mail address"));
                }
                Lever::MailSend { to }
            }
            "http.post" => Lever::HttpPost {
                url: url_of(&needs("an address", "https://example.com/hook")?)?,
            },
            _ => {
                return Err(format!(
                    "{name:?} is not a built-in action; the built-in ones are {}",
                    LEVERS.join(", ")
                ))
            }
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Lever::GithubIssue { .. } => "github.issue",
            Lever::GithubComment { .. } => "github.comment",
            Lever::GithubPr { .. } => "github.pr",
            Lever::MailSend { .. } => "mail.send",
            Lever::HttpPost { .. } => "http.post",
        }
    }

    /// The shape of payload the lever consumes.
    pub fn takes(&self) -> Takes {
        match self {
            Lever::GithubIssue { .. } | Lever::GithubComment { .. } | Lever::MailSend { .. } => {
                Takes::Text
            }
            Lever::GithubPr { .. } => Takes::Diff,
            Lever::HttpPost { .. } => Takes::Json,
        }
    }

    /// Can what it does be undone? An issue can be closed and a comment
    /// deleted; a pull request, a mail and a post are out of avsm's hands
    /// once sent.
    pub fn reversible(&self) -> bool {
        matches!(
            self,
            Lever::GithubIssue { .. } | Lever::GithubComment { .. }
        )
    }

    /// The name of the one key this adapter may read, if it needs one.
    pub fn key_name(&self) -> Option<&'static str> {
        match self {
            Lever::GithubIssue { .. } | Lever::GithubComment { .. } | Lever::GithubPr { .. } => {
                Some("github")
            }
            Lever::MailSend { .. } | Lever::HttpPost { .. } => None,
        }
    }

    /// The host this adapter reaches, for a venture's network list.
    pub fn host(&self, adapters: &Adapters) -> Option<String> {
        match self {
            Lever::GithubIssue { .. } | Lever::GithubComment { .. } | Lever::GithubPr { .. } => {
                host_of(&adapters.github.url)
            }
            Lever::HttpPost { url } => host_of(url),
            Lever::MailSend { .. } => None,
        }
    }

    /// Say, by a run's idempotency key, whether a run whose outcome was
    /// never written happened: Some((happened, what was found)) when the
    /// adapter can check, None when it cannot. None of the built-in levers
    /// can yet: none of them leaves the key on the other side to look for,
    /// so a human says.
    pub fn confirm(
        &self,
        _key: Option<&str>,
        _adapters: &Adapters,
        _run_key: &str,
    ) -> Option<(bool, String)> {
        match self {
            Lever::GithubIssue { .. }
            | Lever::GithubComment { .. }
            | Lever::GithubPr { .. }
            | Lever::MailSend { .. }
            | Lever::HttpPost { .. } => None,
        }
    }

    /// Pull the lever with a payload. Returns whether it went well and one
    /// line of what happened.
    pub fn run(
        &self,
        dir: &Path,
        key: Option<&str>,
        adapters: &Adapters,
        payload: &Payload,
        occasion: &Occasion<'_>,
    ) -> (bool, String) {
        match self.pull(dir, key, adapters, payload, occasion) {
            Ok(detail) => (true, detail),
            Err(e) => (false, e),
        }
    }

    fn pull(
        &self,
        dir: &Path,
        key: Option<&str>,
        adapters: &Adapters,
        payload: &Payload,
        occasion: &Occasion<'_>,
    ) -> Result<String, String> {
        if payload.takes() != self.takes() {
            return Err(format!(
                "{} takes {}, not {}",
                self.name(),
                self.takes(),
                payload.takes()
            ));
        }
        let text = payload.text();
        match self {
            Lever::GithubIssue { repo } => {
                let (title, body) = title_and_body(&text);
                let reply = post_json(
                    &format!("{}/repos/{repo}/issues", adapters.github.url),
                    &github_headers(key),
                    &serde_json::json!({ "title": title, "body": body }),
                )?;
                Ok(format!(
                    "opened issue #{} {}",
                    reply["number"],
                    reply["html_url"].as_str().unwrap_or("")
                )
                .trim()
                .to_string())
            }
            Lever::GithubComment { repo, issue } => {
                let (issue, body) = match issue {
                    Some(n) => (*n, text.trim().to_string()),
                    None => {
                        let (first, rest) = title_and_body(&text);
                        let n = first
                            .trim()
                            .strip_prefix('#')
                            .and_then(|n| n.trim().parse::<u64>().ok())
                            .ok_or(
                                "github.comment: the first line of the text names the issue, like #12",
                            )?;
                        (n, rest)
                    }
                };
                let reply = post_json(
                    &format!(
                        "{}/repos/{repo}/issues/{issue}/comments",
                        adapters.github.url
                    ),
                    &github_headers(key),
                    &serde_json::json!({ "body": body }),
                )?;
                Ok(format!(
                    "commented on #{issue} {}",
                    reply["html_url"].as_str().unwrap_or("")
                )
                .trim()
                .to_string())
            }
            Lever::GithubPr { repo } => github_pr(dir, key, adapters, repo, &text, occasion),
            Lever::MailSend { to } => {
                let (subject, body) = title_and_body(&text);
                let message = mail_message(to, adapters.mail.from.as_deref(), &subject, &body);
                let ran = warden::run(
                    dir,
                    "sh",
                    &["-c", &adapters.mail.command],
                    &message,
                    TIMEOUT,
                );
                if !ran.ok {
                    return Err(format!(
                        "{} failed: {}",
                        adapters.mail.command,
                        ran.detail(TIMEOUT)
                    ));
                }
                Ok(format!("sent to {to}: {subject}"))
            }
            Lever::HttpPost { url } => {
                let body: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                let (status, reply) = http("POST", url, &[], Some(&body))?;
                if status >= 300 {
                    return Err(format!("{url} answered {status}: {}", head(&reply, 200)));
                }
                Ok(format!("{url} answered {status}: {}", head(&reply, 200))
                    .trim_end_matches(": ")
                    .to_string())
            }
        }
    }
}

// ----- helpers -----

/// `owner/repo`, checked.
fn repo_name(s: &str) -> Result<String, String> {
    let ok = s.split_once('/').is_some_and(|(o, r)| {
        !o.is_empty()
            && !r.is_empty()
            && o.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && r.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    });
    if ok {
        Ok(s.to_string())
    } else {
        Err(format!("a repository is written owner/repo, not {s:?}"))
    }
}

fn host_port(s: &str, default: u16) -> Result<(String, u16), String> {
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Ok((
            h.to_string(),
            p.parse()
                .map_err(|_| format!("after the host comes a port number, not {p:?}"))?,
        )),
        _ => Ok((s.to_string(), default)),
    }
}

fn url_of(s: &str) -> Result<String, String> {
    if s.starts_with("http://") || s.starts_with("https://") {
        Ok(s.to_string())
    } else {
        Err(format!(
            "an address starts with http:// or https://, not {s:?}"
        ))
    }
}

/// The host part of an address.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !h.contains(':') => h,
        _ => host,
    };
    Some(host.trim_matches(['[', ']']).to_string())
}

/// The last number in some text, as a report prints it.
pub fn number_from(text: &str) -> Result<f64, String> {
    let last = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    match last.trim().parse::<f64>() {
        Ok(n) if n.is_finite() => Ok(n),
        _ => Err(format!("did not print a number: {}", head(last.trim(), 80))),
    }
}

fn head(s: &str, n: usize) -> String {
    let s = s.replace('\n', "  ");
    if s.chars().count() <= n {
        s
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

/// The first non-empty line, and the rest.
fn title_and_body(text: &str) -> (String, String) {
    let text = text.trim_start_matches('\n');
    match text.split_once('\n') {
        Some((first, rest)) => (first.trim().to_string(), rest.trim().to_string()),
        None => (text.trim().to_string(), String::new()),
    }
}

/// Midnight UTC of the day holding `now`, unix seconds.
pub fn midnight_utc(now: i64) -> i64 {
    now - now.rem_euclid(86_400)
}

fn github_headers(key: Option<&str>) -> Vec<(String, String)> {
    let mut h = vec![
        (
            "accept".to_string(),
            "application/vnd.github+json".to_string(),
        ),
        ("user-agent".to_string(), "avsm".to_string()),
    ];
    if let Some(k) = key {
        h.push(("authorization".to_string(), format!("Bearer {k}")));
    }
    h
}

/// Every item of a Stripe list, page by page.
fn stripe_list(url: &str, key: &str) -> Result<Vec<Value>, String> {
    let headers = vec![("authorization".to_string(), format!("Bearer {key}"))];
    let mut items = Vec::new();
    let mut next = url.to_string();
    for _ in 0..20 {
        let page = get_json(&next, &headers)?;
        let data = page["data"].as_array().cloned().unwrap_or_default();
        let last = data
            .last()
            .and_then(|d| d["id"].as_str())
            .map(str::to_string);
        items.extend(data);
        if page["has_more"].as_bool() != Some(true) {
            break;
        }
        match last {
            Some(id) => next = format!("{url}&starting_after={id}"),
            None => break,
        }
    }
    Ok(items)
}

/// Monthly recurring revenue, in whole units of the currency, and the
/// number of subscriptions counted. Yearly, weekly and daily prices are
/// brought to a month.
pub fn stripe_mrr(subscriptions: &[Value]) -> (f64, usize) {
    let mut cents = 0.0;
    for sub in subscriptions {
        let items = sub["items"]["data"].as_array().cloned().unwrap_or_default();
        for item in items {
            let price = &item["price"];
            let amount = price["unit_amount"].as_f64().unwrap_or(0.0);
            let quantity = item["quantity"].as_f64().unwrap_or(1.0);
            let count = price["recurring"]["interval_count"]
                .as_f64()
                .unwrap_or(1.0)
                .max(1.0);
            let per_month = match price["recurring"]["interval"].as_str() {
                Some("year") => amount / 12.0,
                Some("week") => amount * 52.0 / 12.0,
                Some("day") => amount * 365.0 / 12.0,
                _ => amount,
            } / count;
            cents += per_month * quantity;
        }
    }
    ((cents / 100.0 * 100.0).round() / 100.0, subscriptions.len())
}

/// The sum of charges that succeeded, in whole units, and how many.
pub fn stripe_charges(charges: &[Value]) -> (f64, usize) {
    let ok: Vec<&Value> = charges
        .iter()
        .filter(|c| c["status"].as_str() == Some("succeeded"))
        .collect();
    let cents: f64 = ok
        .iter()
        .map(|c| c["amount"].as_f64().unwrap_or(0.0) - c["amount_refunded"].as_f64().unwrap_or(0.0))
        .sum();
    ((cents / 100.0 * 100.0).round() / 100.0, ok.len())
}

/// Failed check runs, and all of them, from a check-runs reply.
pub fn failed_checks(reply: &Value) -> (usize, usize) {
    let runs = reply["check_runs"].as_array().cloned().unwrap_or_default();
    let failed = runs
        .iter()
        .filter(|r| r["conclusion"].as_str() == Some("failure"))
        .count();
    (failed, runs.len())
}

/// The mail as handed to the mail command.
pub fn mail_message(to: &str, from: Option<&str>, subject: &str, body: &str) -> String {
    let mut m = String::new();
    m.push_str(&format!("To: {to}\n"));
    if let Some(f) = from {
        m.push_str(&format!("From: {f}\n"));
    }
    m.push_str(&format!(
        "Subject: {}\n",
        subject.replace(['\r', '\n'], " ")
    ));
    m.push_str("MIME-Version: 1.0\nContent-Type: text/plain; charset=utf-8\n\n");
    m.push_str(body);
    if !body.ends_with('\n') {
        m.push('\n');
    }
    m
}

/// Open a pull request: apply the diff on a fresh branch from the default
/// branch in a temporary worktree of the checkout in `dir`, push it, and
/// ask GitHub for the pull request.
fn github_pr(
    dir: &Path,
    key: Option<&str>,
    adapters: &Adapters,
    repo: &str,
    diff: &str,
    occasion: &Occasion<'_>,
) -> Result<String, String> {
    use base64::prelude::*;
    let mut env: Vec<(String, String)> = vec![("GIT_TERMINAL_PROMPT".into(), "0".into())];
    if let Some(token) = key {
        let basic = BASE64_STANDARD.encode(format!("x-access-token:{token}"));
        env.push(("GIT_CONFIG_COUNT".into(), "1".into()));
        env.push((
            "GIT_CONFIG_KEY_0".into(),
            "http.https://github.com/.extraheader".into(),
        ));
        env.push((
            "GIT_CONFIG_VALUE_0".into(),
            format!("AUTHORIZATION: basic {basic}"),
        ));
    }
    let git = |cwd: &Path, args: &[&str]| -> Result<String, String> {
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let ran = warden::run_with(cwd, "git", args, &env, "", TIMEOUT);
        if ran.ok {
            Ok(ran.stdout.trim().to_string())
        } else {
            Err(format!("git {}: {}", args.join(" "), ran.detail(TIMEOUT)))
        }
    };
    git(dir, &["rev-parse", "--is-inside-work-tree"]).map_err(|_| {
        format!(
            "{} is not a git checkout; github.pr needs one",
            dir.display()
        )
    })?;
    let base = git(
        dir,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .ok()
    .and_then(|r| r.strip_prefix("origin/").map(str::to_string))
    .unwrap_or_else(|| "main".to_string());
    git(dir, &["fetch", "--quiet", "origin", &base])?;
    let stamp = warden::now();
    let branch = format!("avsm/{}-{stamp}", occasion.venture);
    let work = std::env::temp_dir().join(format!("avsm-pr-{}-{stamp}", std::process::id()));
    let work_str = work.to_string_lossy().into_owned();
    git(
        dir,
        &[
            "worktree",
            "add",
            "--detach",
            &work_str,
            &format!("origin/{base}"),
        ],
    )?;
    let result = (|| -> Result<String, String> {
        let patch = work.join(".avsm.diff");
        std::fs::write(&patch, diff).map_err(|e| e.to_string())?;
        git(&work, &["apply", "--index", ".avsm.diff"])?;
        let _ = std::fs::remove_file(&patch);
        let title = format!("{}: {}", occasion.venture, occasion.action);
        let message = format!("{title}\n\n{}", occasion.proof);
        git(
            &work,
            &[
                "-c",
                "user.name=avsm",
                "-c",
                "user.email=avsm@localhost",
                "commit",
                "--quiet",
                "-m",
                &message,
            ],
        )?;
        git(
            &work,
            &[
                "push",
                "--quiet",
                "origin",
                &format!("HEAD:refs/heads/{branch}"),
            ],
        )?;
        Ok(title)
    })();
    let _ = git(dir, &["worktree", "remove", "--force", &work_str]);
    let _ = std::fs::remove_dir_all(&work);
    let title = result?;
    let reply = post_json(
        &format!("{}/repos/{repo}/pulls", adapters.github.url),
        &github_headers(key),
        &serde_json::json!({
            "title": title,
            "head": branch,
            "base": base,
            "body": format!("```\n{}```\n", occasion.proof),
        }),
    )?;
    Ok(format!(
        "opened pull request #{} {}",
        reply["number"],
        reply["html_url"].as_str().unwrap_or("")
    )
    .trim()
    .to_string())
}

// ----- IMAP -----

trait Wire: Read + Write {}
impl<T: Read + Write> Wire for T {}

fn connect(host: &str, port: u16) -> Result<Box<dyn Wire>, String> {
    let tcp =
        TcpStream::connect((host, port)).map_err(|e| format!("cannot reach {host}:{port}: {e}"))?;
    tcp.set_read_timeout(Some(TIMEOUT))
        .map_err(|e| e.to_string())?;
    tcp.set_write_timeout(Some(TIMEOUT))
        .map_err(|e| e.to_string())?;
    if port != 993 {
        return Ok(Box::new(tcp));
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| format!("{host:?} is not a host name"))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| e.to_string())?;
    Ok(Box::new(rustls::StreamOwned::new(conn, tcp)))
}

/// How many messages INBOX holds that no one has seen.
fn imap_unseen(host: &str, port: u16, user: &str, pass: &str) -> Result<u64, String> {
    let wire = connect(host, port)?;
    let mut reader = BufReader::new(wire);
    let mut line = String::new();
    let read = |reader: &mut BufReader<Box<dyn Wire>>, line: &mut String| -> Result<(), String> {
        line.clear();
        let n = reader.read_line(line).map_err(|e| format!("{host}: {e}"))?;
        if n == 0 {
            return Err(format!("{host} hung up"));
        }
        Ok(())
    };
    read(&mut reader, &mut line)?;
    if !line.starts_with("* OK") && !line.starts_with("* PREAUTH") {
        return Err(format!("{host} did not greet: {}", line.trim()));
    }
    let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let mut unseen = None;
    for (tag, command) in [
        ("a1", format!("LOGIN {} {}", quote(user), quote(pass))),
        ("a2", "STATUS INBOX (UNSEEN)".to_string()),
        ("a3", "LOGOUT".to_string()),
    ] {
        reader
            .get_mut()
            .write_all(format!("{tag} {command}\r\n").as_bytes())
            .map_err(|e| format!("{host}: {e}"))?;
        loop {
            read(&mut reader, &mut line)?;
            if let Some(n) = imap_unseen_count(&line) {
                unseen = Some(n);
            }
            if let Some(rest) = line.strip_prefix(tag) {
                let rest = rest.trim();
                if rest.starts_with("OK") {
                    break;
                }
                return Err(format!(
                    "{host} refused {}: {}",
                    command.split_whitespace().next().unwrap_or(""),
                    rest
                ));
            }
        }
    }
    unseen.ok_or_else(|| format!("{host} did not say how many are unseen"))
}

/// The number in `* STATUS INBOX (UNSEEN 3)`, if this is that line.
pub fn imap_unseen_count(line: &str) -> Option<u64> {
    let rest = line.strip_prefix("* STATUS")?;
    let (_, inside) = rest.split_once('(')?;
    let inside = inside.split(')').next()?;
    let words: Vec<&str> = inside.split_whitespace().collect();
    let at = words
        .iter()
        .position(|w| w.eq_ignore_ascii_case("UNSEEN"))?;
    words.get(at + 1)?.parse().ok()
}

// ----- HTTP -----

/// One request. Returns the status and the body as text.
pub fn http(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&Value>,
) -> Result<(u16, String), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = match (method, body) {
        ("GET", _) => {
            let mut req = agent.get(url);
            for (k, v) in headers {
                req = req.header(k.as_str(), v.as_str());
            }
            req.call()
        }
        (_, Some(json)) => {
            let mut req = agent.post(url);
            for (k, v) in headers {
                req = req.header(k.as_str(), v.as_str());
            }
            req.send_json(json)
        }
        (_, None) => {
            let mut req = agent.post(url);
            for (k, v) in headers {
                req = req.header(k.as_str(), v.as_str());
            }
            req.send_empty()
        }
    }
    .map_err(|e| format!("cannot reach {url}: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("bad answer from {url}: {e}"))?;
    Ok((status, text))
}

/// GET and read JSON; a status of 300 or more is an error.
pub fn get_json(url: &str, headers: &[(String, String)]) -> Result<Value, String> {
    let (status, text) = http("GET", url, headers, None)?;
    if status >= 300 {
        return Err(format!("{url} answered {status}: {}", head(&text, 400)));
    }
    serde_json::from_str(&text).map_err(|e| format!("{url} did not answer JSON: {e}"))
}

/// POST JSON and read JSON back; a status of 300 or more is an error.
pub fn post_json(url: &str, headers: &[(String, String)], body: &Value) -> Result<Value, String> {
    let (status, text) = http("POST", url, headers, Some(body))?;
    if status >= 300 {
        return Err(format!("{url} answered {status}: {}", head(&text, 400)));
    }
    serde_json::from_str(&text).map_err(|e| format!("{url} did not answer JSON: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_specs_parse_with_plain_messages() {
        assert_eq!(Metric::parse("count").unwrap(), None);
        assert_eq!(
            Metric::parse("stripe.mrr").unwrap(),
            Some(Metric::StripeMrr)
        );
        assert_eq!(
            Metric::parse("github.open_issues 4esv/avsm").unwrap(),
            Some(Metric::GithubOpenIssues {
                repo: "4esv/avsm".into()
            })
        );
        assert_eq!(
            Metric::parse("imap.unread mail.example.com").unwrap(),
            Some(Metric::ImapUnread {
                host: "mail.example.com".into(),
                port: 993
            })
        );
        assert_eq!(
            Metric::parse("imap.unread 127.0.0.1:1430").unwrap(),
            Some(Metric::ImapUnread {
                host: "127.0.0.1".into(),
                port: 1430
            })
        );
        assert_eq!(
            Metric::parse("shell df --output=pcent / | tail -1").unwrap(),
            Some(Metric::Shell {
                command: "df --output=pcent / | tail -1".into()
            })
        );
        let msg = Metric::parse("github.failed_ci").unwrap_err();
        assert_eq!(
            msg,
            "github.failed_ci needs a repository after it, like: metric: github.failed_ci owner/repo"
        );
        let msg = Metric::parse("github.failed_ci avsm").unwrap_err();
        assert_eq!(msg, "a repository is written owner/repo, not \"avsm\"");
        let msg = Metric::parse("stripe.mrr acct_1").unwrap_err();
        assert!(
            msg.starts_with("stripe.mrr takes nothing after it"),
            "{msg}"
        );
        let msg = Metric::parse("http.status example.com").unwrap_err();
        assert_eq!(
            msg,
            "an address starts with http:// or https://, not \"example.com\""
        );
        for (spec, name) in [
            ("stripe.mrr", "stripe.mrr"),
            ("stripe.charges_today", "stripe.charges_today"),
            ("github.open_issues a/b", "github.open_issues"),
            ("github.failed_ci a/b", "github.failed_ci"),
            ("imap.unread h", "imap.unread"),
            ("http.status https://h/", "http.status"),
            ("file.number f", "file.number"),
            ("shell true", "shell"),
        ] {
            let m = Metric::parse(spec).unwrap().unwrap();
            assert_eq!(m.name(), name);
            assert!(METRICS.contains(&m.name()));
        }
    }

    #[test]
    fn lever_specs_parse_and_know_their_shape() {
        let l = Lever::parse("github.issue 4esv/avsm").unwrap();
        assert_eq!(l.takes(), Takes::Text);
        assert!(l.reversible());
        assert_eq!(l.key_name(), Some("github"));
        let l = Lever::parse("github.comment 4esv/avsm#12").unwrap();
        assert_eq!(
            l,
            Lever::GithubComment {
                repo: "4esv/avsm".into(),
                issue: Some(12)
            }
        );
        let l = Lever::parse("github.pr 4esv/avsm").unwrap();
        assert_eq!(l.takes(), Takes::Diff);
        assert!(!l.reversible());
        let l = Lever::parse("mail.send you@example.com").unwrap();
        assert_eq!(l.takes(), Takes::Text);
        assert!(!l.reversible());
        assert_eq!(l.key_name(), None);
        let l = Lever::parse("http.post https://example.com/hook").unwrap();
        assert_eq!(l.takes(), Takes::Json);
        assert!(!l.reversible());
        let msg = Lever::parse("github.issue").unwrap_err();
        assert_eq!(
            msg,
            "github.issue needs a repository after it, like: adapter: github.issue owner/repo"
        );
        let msg = Lever::parse("slack.post x").unwrap_err();
        assert!(msg.contains("not a built-in action"), "{msg}");
        assert!(Lever::parse("mail.send nobody").is_err());
        assert!(Lever::parse("github.comment a/b#x").is_err());
        for (spec, name) in [
            ("github.issue a/b", "github.issue"),
            ("github.comment a/b", "github.comment"),
            ("github.pr a/b", "github.pr"),
            ("mail.send a@b", "mail.send"),
            ("http.post https://h/", "http.post"),
        ] {
            let l = Lever::parse(spec).unwrap();
            assert_eq!(l.name(), name);
            assert!(LEVERS.contains(&l.name()));
        }
    }

    #[test]
    fn stripe_sums_bring_every_interval_to_a_month() {
        let subs = serde_json::json!([
            {"items": {"data": [
                {"quantity": 2, "price": {"unit_amount": 1000, "recurring": {"interval": "month", "interval_count": 1}}},
                {"quantity": 1, "price": {"unit_amount": 12000, "recurring": {"interval": "year", "interval_count": 1}}}
            ]}},
            {"items": {"data": [
                {"quantity": 1, "price": {"unit_amount": 300, "recurring": {"interval": "week", "interval_count": 1}}}
            ]}}
        ]);
        let (mrr, n) = stripe_mrr(subs.as_array().unwrap());
        assert_eq!(n, 2);
        assert_eq!(mrr, 43.0);
        let charges = serde_json::json!([
            {"amount": 2500, "amount_refunded": 500, "status": "succeeded"},
            {"amount": 9900, "status": "failed"},
            {"amount": 100, "status": "succeeded"}
        ]);
        assert_eq!(stripe_charges(charges.as_array().unwrap()), (21.0, 2));
        assert_eq!(midnight_utc(1_700_000_000), 1_699_920_000);
    }

    #[test]
    fn small_parsers_do_their_one_job() {
        let runs = serde_json::json!({"check_runs": [
            {"conclusion": "success"}, {"conclusion": "failure"}, {"conclusion": null}
        ]});
        assert_eq!(failed_checks(&runs), (1, 3));
        assert_eq!(
            imap_unseen_count("* STATUS INBOX (MESSAGES 10 UNSEEN 3)\r\n"),
            Some(3)
        );
        assert_eq!(imap_unseen_count("* STATUS \"INBOX\" (UNSEEN 0)"), Some(0));
        assert_eq!(imap_unseen_count("a2 OK done"), None);
        assert_eq!(number_from("x\n 12.5 \n\n").unwrap(), 12.5);
        assert!(number_from("none").is_err());
        assert_eq!(
            host_of("https://api.stripe.com/v1"),
            Some("api.stripe.com".into())
        );
        assert_eq!(
            host_of("http://user@h.example:8080/x?y"),
            Some("h.example".into())
        );
        assert_eq!(host_of("nope"), None);
        let m = mail_message("a@b", Some("avsm@b"), "Hi\nthere", "body");
        assert_eq!(
            m,
            "To: a@b\nFrom: avsm@b\nSubject: Hi there\nMIME-Version: 1.0\nContent-Type: text/plain; charset=utf-8\n\nbody\n"
        );
        assert_eq!(
            title_and_body("\nTitle\n\nBody\n"),
            ("Title".into(), "Body".into())
        );
    }
}
