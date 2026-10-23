//! FileList: website login + browse.php scraping, or the official API (username + passkey).

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use reqwest::header::{CONTENT_TYPE, ORIGIN, REFERER};
use reqwest::Url;
use scraper::{ElementRef, Html, Node, Selector};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::util::{decode_html_entities, format_bytes};

const BASE: &str = "https://filelist.io";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36";

/// FileList movie categories (id, label as shown on the tracker).
pub const MOVIE_CATEGORIES: &[(u32, &str)] = &[
    (4, "Filme HD"),
    (19, "Filme HD-RO"),
    (6, "Filme 4K"),
    (26, "Filme 4K Blu-Ray"),
    (20, "Filme Blu-Ray"),
    (1, "Filme SD"),
    (2, "Filme DVD"),
    (3, "Filme DVD-RO"),
    (25, "Filme 3D"),
];

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Movie {
    pub id: String,
    pub name: String,
    pub imdb: Option<String>,
    pub poster: Option<String>,
    pub category: String,
    pub category_id: Option<u32>,
    pub size: String,
    pub seeders: Option<u64>,
    pub leechers: Option<u64>,
    pub added: String,
    pub freeleech: bool,
    pub description: String,
    pub downloaded: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub items: Vec<Movie>,
    pub has_more: bool,
}

fn normalize_cats(cats: &[u32]) -> Vec<u32> {
    let valid: Vec<u32> = cats.iter().copied().filter(|c| MOVIE_CATEGORIES.iter().any(|(id, _)| id == c)).collect();
    if valid.is_empty() { MOVIE_CATEGORIES.iter().map(|(id, _)| *id).collect() } else { valid }
}

// FileList ignores the category filter when searching (games, apps, music… come back too), so filter ourselves.
static MOVIE_CATEGORY_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(filme|movies)\b").unwrap());

fn is_wanted(m: &Movie, cats: &[u32]) -> bool {
    match m.category_id {
        Some(id) => cats.contains(&id),
        None => MOVIE_CATEGORY_NAME.is_match(&m.category),
    }
}

pub enum FileList {
    Web(WebClient),
    Api(ApiClient),
}

impl FileList {
    pub fn create(username: &str, password: &str, passkey: &str) -> Result<FileList> {
        if username.trim().is_empty() {
            bail!("FILELIST_USERNAME is missing from .env");
        }
        if !password.is_empty() {
            return Ok(FileList::Web(WebClient::new(username, password)));
        }
        if !passkey.trim().is_empty() {
            return Ok(FileList::Api(ApiClient::new(username, passkey)));
        }
        bail!("Set FILELIST_PASSWORD (website login) or FILELIST_PASSKEY (API) in .env")
    }

    pub fn mode(&self) -> &'static str {
        match self {
            FileList::Web(_) => "web",
            FileList::Api(_) => "api",
        }
    }

    /// Check the credentials (used by the setup screen).
    pub async fn verify(&self) -> Result<()> {
        match self {
            FileList::Web(c) => c.login().await,
            FileList::Api(c) => c.call(&[("action", "latest-torrents"), ("limit", "1")]).await.map(|_| ()),
        }
    }

    pub async fn browse(&self, page: u32, query: &str, cats: &[u32]) -> Result<Page> {
        let wanted = normalize_cats(cats);
        let mut result = match self {
            FileList::Web(c) => c.browse(page, query, &wanted).await?,
            FileList::Api(c) => c.browse(page, query, &wanted).await?,
        };
        result.items.retain(|m| is_wanted(m, &wanted));
        Ok(result)
    }

    pub async fn download(&self, id: &str) -> Result<Vec<u8>> {
        match self {
            FileList::Web(c) => c.download(id).await,
            FileList::Api(c) => c.download(id).await,
        }
    }
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(UA)
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::limited(8))
        .timeout(Duration::from_secs(40))
        .build()
        .expect("http client")
}

// ---------- website login + scraping ----------

pub struct WebClient {
    username: String,
    password: String,
    session: Mutex<Session>,
}

struct Session {
    http: reqwest::Client,
    logged_in: bool,
    /// After a refused login or FileList's hourly limit: don't contact the site again until then (every retry
    /// counts against the limit and keeps the account locked).
    blocked: Option<(Instant, String)>,
}

impl Session {
    fn check_blocked(&mut self) -> Result<()> {
        match &self.blocked {
            Some((until, reason)) if Instant::now() < *until => {
                let mins = until.saturating_duration_since(Instant::now()).as_secs().div_ceil(60);
                bail!("{reason} (the app will try again in {mins} min)")
            }
            _ => {
                self.blocked = None;
                Ok(())
            }
        }
    }

    fn block(&mut self, wait: Duration, reason: String) -> anyhow::Error {
        self.logged_in = false;
        self.blocked = Some((Instant::now() + wait, reason.clone()));
        anyhow!(reason)
    }
}

const LIMIT_WAIT: Duration = Duration::from_secs(60 * 60);
const LOGIN_RETRY_WAIT: Duration = Duration::from_secs(5 * 60);

/// FileList's "Numarul maxim permis de actiuni pentru acest cont a fost depasit. Reveniti peste o ora."
fn is_rate_limited(html: &str) -> bool {
    html.to_lowercase().contains("maxim permis de actiuni")
}

fn rate_limit_error() -> String {
    "FileList blocked this account for about an hour: it opened more pages than the site allows per hour".into()
}

impl WebClient {
    fn new(username: &str, password: &str) -> Self {
        WebClient {
            username: username.to_string(),
            password: password.to_string(),
            session: Mutex::new(Session { http: http_client(), logged_in: false, blocked: None }),
        }
    }

    pub async fn login(&self) -> Result<()> {
        let mut session = self.session.lock().await;
        self.login_locked(&mut session).await
    }

    async fn login_locked(&self, session: &mut Session) -> Result<()> {
        session.check_blocked()?;
        // fresh cookie jar for every login
        session.http = http_client();
        session.logged_in = false;
        let html = session.http.get(format!("{BASE}/login.php")).send().await?.text().await?;
        if is_rate_limited(&html) {
            return Err(session.block(LIMIT_WAIT, rate_limit_error()));
        }
        let (action, mut fields) = parse_login_form(&html)
            .ok_or_else(|| anyhow!("Could not find the login form on filelist.io (site layout changed or blocked)."))?;
        set_field(&mut fields, "username", &self.username);
        set_field(&mut fields, "password", &self.password);
        let url = Url::parse(BASE)?.join(&action)?;
        let html = session
            .http
            .post(url)
            .header(REFERER, format!("{BASE}/login.php"))
            .header(ORIGIN, BASE)
            .form(&fields)
            .send()
            .await?
            .text()
            .await?;
        if is_rate_limited(&html) {
            return Err(session.block(LIMIT_WAIT, rate_limit_error()));
        }
        if !is_logged_in(&html) {
            return Err(session.block(LOGIN_RETRY_WAIT, format!("FileList login failed: {}", login_error(&html))));
        }
        session.logged_in = true;
        Ok(())
    }

    /// A logged-in HTTP client (logging in first if needed).
    async fn client(&self, force_login: bool) -> Result<reqwest::Client> {
        let mut session = self.session.lock().await;
        session.check_blocked()?;
        if force_login || !session.logged_in {
            self.login_locked(&mut session).await?;
        }
        Ok(session.http.clone())
    }

    /// A page from the site, or the hourly-limit error (which also stops further requests for a while).
    async fn get_text(&self, http: reqwest::Client, url: Url) -> Result<String> {
        let html = http.get(url).send().await?.text().await?;
        if is_rate_limited(&html) {
            return Err(self.session.lock().await.block(LIMIT_WAIT, rate_limit_error()));
        }
        Ok(html)
    }

    async fn fetch_html(&self, url: Url) -> Result<String> {
        let html = self.get_text(self.client(false).await?, url.clone()).await?;
        if is_logged_in(&html) {
            return Ok(html);
        }
        // session expired: log in again once
        self.get_text(self.client(true).await?, url).await
    }

    async fn browse(&self, page: u32, query: &str, cats: &[u32]) -> Result<Page> {
        let mut params: Vec<(String, String)> = cats.iter().map(|c| ("cats[]".to_string(), c.to_string())).collect();
        if !query.is_empty() {
            params.push(("search".into(), query.into()));
            let imdb = query.len() > 2 && query[..2].eq_ignore_ascii_case("tt") && query[2..].chars().all(|c| c.is_ascii_digit());
            params.push(("searchin".into(), if imdb { "3" } else { "1" }.into()));
        }
        params.push(("page".into(), page.to_string()));
        let url = Url::parse_with_params(&format!("{BASE}/browse.php"), &params)?;
        let html = self.fetch_html(url).await?;
        Ok(parse_browse(&html, page))
    }

    async fn download(&self, id: &str) -> Result<Vec<u8>> {
        let url = format!("{BASE}/download.php?id={}", urlencode(id));
        for attempt in 0..2 {
            let res = self.client(attempt > 0).await?.get(&url).send().await?;
            let is_html = res.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("text/html"));
            if is_html {
                if is_rate_limited(&res.text().await.unwrap_or_default()) {
                    return Err(self.session.lock().await.block(LIMIT_WAIT, rate_limit_error()));
                }
                continue; // logged out: log in again and retry
            }
            if !res.status().is_success() {
                bail!("Could not download torrent {id} (HTTP {})", res.status());
            }
            return Ok(res.bytes().await?.to_vec());
        }
        bail!("Could not download torrent {id}: FileList returned a web page instead of a .torrent file")
    }
}

fn is_logged_in(html: &str) -> bool {
    html.to_ascii_lowercase().contains("logout.php")
}

fn set_field(fields: &mut Vec<(String, String)>, name: &str, value: &str) {
    match fields.iter_mut().find(|(k, _)| k == name) {
        Some(f) => f.1 = value.to_string(),
        None => fields.push((name.to_string(), value.to_string())),
    }
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("valid selector")
}

/// The login form's action and its fields (hidden inputs such as the CSRF "validator" included).
fn parse_login_form(html: &str) -> Option<(String, Vec<(String, String)>)> {
    let doc = Html::parse_document(html);
    let form = doc
        .select(&sel("form"))
        .find(|f| f.select(&sel(r#"input[name="password"]"#)).next().is_some())?;
    let mut fields = Vec::new();
    for input in form.select(&sel("input")) {
        let el = input.value();
        let Some(name) = el.attr("name") else { continue };
        let kind = el.attr("type").unwrap_or("text").to_ascii_lowercase();
        if (kind == "checkbox" || kind == "radio") && el.attr("checked").is_none() {
            continue;
        }
        if kind == "submit" {
            continue;
        }
        fields.push((name.to_string(), el.attr("value").unwrap_or("").to_string()));
    }
    let action = form.value().attr("action").unwrap_or("/takelogin.php").to_string();
    Some((action, fields))
}

fn login_error(html: &str) -> String {
    let doc = Html::parse_document(html);
    doc.select(&sel(".error, .alert, .notice, .cblock-innercontent, .main .text, td.text"))
        .next()
        .map(|e| text_of(e))
        .filter(|t| !t.is_empty())
        .map(|t| t.chars().take(200).collect())
        .unwrap_or_else(|| "unknown reason".into())
}

/// Text of an element, with <br> treated as a space and whitespace collapsed.
fn text_of(el: ElementRef) -> String {
    let mut out = String::new();
    for node in el.descendants() {
        match node.value() {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) if e.name() == "br" => out.push(' '),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

static ID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"id=(\d+)").unwrap());
static CAT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"cat=(\d+)").unwrap());
static IMDB_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"tt\d{7,9}").unwrap());
static SIZE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(\d+(?:[.,]\d+)?\s*(?:TB|GB|MB|KB))\b").unwrap());
static TIME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d{1,2}:\d{2}").unwrap());
static FREELEECH_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)freeleech").unwrap());
static POSTER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)https?://[^\s"'<>()]+?\.(?:jpe?g|png|webp)(?:\?[^\s"'<>]*)?"#).unwrap());
static NOT_POSTER_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)/(styles|cat|flags|icons?|smilies)/").unwrap());

fn parse_browse(html: &str, page: u32) -> Page {
    let doc = Html::parse_document(html);
    let details = sel(r#"a[href*="details.php?id="]"#);
    let cells_sel = sel(".torrenttable");
    let cat_link = sel(r#"a[href*="cat="]"#);
    let cat_img = sel(r#"a[href*="cat="] img"#);
    let mut items = Vec::new();

    for row in doc.select(&sel(".torrentrow")) {
        let Some(link) = row.select(&details).next() else { continue };
        let Some(id) = link.value().attr("href").and_then(|h| ID_RE.captures(h)).map(|c| c[1].to_string()) else {
            continue;
        };
        let row_html = row.html();
        let cells: Vec<String> = row.select(&cells_sel).map(text_of).collect();
        let size_idx = cells.iter().position(|t| SIZE_RE.is_match(t));
        let after: Vec<Option<u64>> = size_idx
            .map(|i| {
                cells[i + 1..]
                    .iter()
                    .map(|t| t.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        let name = link.value().attr("title").map(str::to_string).unwrap_or_else(|| text_of(link));
        items.push(Movie {
            id,
            name,
            imdb: IMDB_RE.find(&row_html).map(|m| m.as_str().to_string()),
            poster: find_poster_url(&row_html),
            category: row.select(&cat_img).next().and_then(|i| i.value().attr("alt")).unwrap_or("").to_string(),
            category_id: row
                .select(&cat_link)
                .next()
                .and_then(|a| a.value().attr("href"))
                .and_then(|h| CAT_RE.captures(h))
                .and_then(|c| c[1].parse().ok()),
            size: size_idx.and_then(|i| SIZE_RE.captures(&cells[i])).map(|c| c[1].to_string()).unwrap_or_default(),
            seeders: after.get(1).copied().flatten(),
            leechers: after.get(2).copied().flatten(),
            added: cells.iter().find(|t| TIME_RE.is_match(t)).cloned().unwrap_or_default(),
            freeleech: FREELEECH_RE.is_match(&row_html),
            description: String::new(),
            downloaded: false,
        });
    }

    // "more" depends on FileList's pager, not on how many rows survive the movie filter
    let has_more = doc.select(&sel(&format!(r#"a[href*="page={}"]"#, page + 1))).next().is_some();
    Page { items, has_more }
}

/// Posters on the browse page live in tooltip markup, so look for any external image URL in the row.
fn find_poster_url(row_html: &str) -> Option<String> {
    let decoded = decode_html_entities(row_html);
    POSTER_RE.find_iter(&decoded).map(|m| m.as_str()).find(|u| !NOT_POSTER_RE.is_match(u)).map(str::to_string)
}

fn urlencode(s: &str) -> String {
    Url::parse_with_params("http://x/", [("v", s)])
        .ok()
        .and_then(|u| u.query().map(|q| q.trim_start_matches("v=").to_string()))
        .unwrap_or_default()
}

// ---------- official API (username + passkey) ----------

pub struct ApiClient {
    username: String,
    passkey: String,
    http: reqwest::Client,
}

impl ApiClient {
    fn new(username: &str, passkey: &str) -> Self {
        ApiClient { username: username.into(), passkey: passkey.trim().into(), http: http_client() }
    }

    async fn call(&self, params: &[(&str, &str)]) -> Result<Value> {
        let mut all: Vec<(&str, &str)> = vec![("username", &self.username), ("passkey", &self.passkey)];
        all.extend_from_slice(params);
        let url = Url::parse_with_params(&format!("{BASE}/api.php"), &all)?;
        let res = self.http.get(url).send().await?;
        let status = res.status();
        let data: Value = res.json().await.unwrap_or(Value::Null);
        if !status.is_success() || data.is_null() || data.get("error").is_some() {
            let msg = data.get("error").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("HTTP {status}"));
            bail!("FileList API error: {msg}");
        }
        Ok(data)
    }

    async fn browse(&self, page: u32, query: &str, cats: &[u32]) -> Result<Page> {
        if page > 0 {
            return Ok(Page { items: vec![], has_more: false }); // the API has no paging
        }
        let category = cats.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        let data = if query.is_empty() {
            self.call(&[("action", "latest-torrents"), ("limit", "100"), ("category", &category)]).await?
        } else {
            let kind = if query.to_ascii_lowercase().starts_with("tt") { "imdb" } else { "name" };
            self.call(&[("action", "search-torrents"), ("type", kind), ("query", query), ("category", &category)]).await?
        };
        let items = data
            .as_array()
            .context("unexpected API response")?
            .iter()
            .map(|t| Movie {
                id: t["id"].as_u64().map(|n| n.to_string()).or_else(|| t["id"].as_str().map(str::to_string)).unwrap_or_default(),
                name: t["name"].as_str().unwrap_or("").to_string(),
                imdb: t["imdb"].as_str().filter(|s| !s.is_empty()).map(str::to_string),
                poster: None,
                category: t["category"].as_str().unwrap_or("").to_string(),
                category_id: None,
                size: format_bytes(t["size"].as_u64().unwrap_or(0)),
                seeders: t["seeders"].as_u64(),
                leechers: t["leechers"].as_u64(),
                added: t["upload_date"].as_str().unwrap_or("").to_string(),
                freeleech: t["freeleech"].as_u64().unwrap_or(0) != 0,
                description: t["small_description"].as_str().unwrap_or("").to_string(),
                downloaded: false,
            })
            .collect();
        Ok(Page { items, has_more: false })
    }

    async fn download(&self, id: &str) -> Result<Vec<u8>> {
        let url = Url::parse_with_params(&format!("{BASE}/download.php"), [("id", id), ("passkey", self.passkey.as_str())])?;
        let res = self.http.get(url).send().await?;
        let is_html = res.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("text/html"));
        if !res.status().is_success() || is_html {
            bail!("Could not download torrent {id} (HTTP {})", res.status());
        }
        Ok(res.bytes().await?.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROW: &str = r#"<div class='torrentrow'>
 <div class='torrenttable'><a href='browse.php?cat=4'><img alt='Filme HD' src='styles/images/cat/hd.png'></a></div>
 <div class='torrenttable'><span title="&lt;img src='https://image.tmdb.org/t/p/w300/abc.jpg'&gt;"><a href='details.php?id=123456' title='Dune.Part.Two.2024.1080p.BluRay.x264-GRP'><b>Dune</b></a></span> <a href='https://www.imdb.com/title/tt15239678/'>imdb</a> <img src='styles/images/tags/freeleech.png'></div>
 <div class='torrenttable'><a href='download.php?id=123456'>dl</a></div>
 <div class='torrenttable'><span>13:37<br>23/10/2026</span></div>
 <div class='torrenttable'>14.52<br>GB</div>
 <div class='torrenttable'>1,234<br>times</div>
 <div class='torrenttable'><b>456</b></div>
 <div class='torrenttable'>12</div>
</div>
<div class='torrentrow'><div class='torrenttable'><a href='browse.php?cat=27'><img alt='Games PC'></a></div><div class='torrenttable'><a href='details.php?id=9' title='FIFA'>x</a></div></div>
<a href='browse.php?page=1'>Next</a>"#;

    #[test]
    fn parses_browse_rows() {
        let page = parse_browse(ROW, 0);
        assert!(page.has_more);
        let m = &page.items[0];
        assert_eq!(m.id, "123456");
        assert_eq!(m.name, "Dune.Part.Two.2024.1080p.BluRay.x264-GRP");
        assert_eq!(m.imdb.as_deref(), Some("tt15239678"));
        assert_eq!(m.poster.as_deref(), Some("https://image.tmdb.org/t/p/w300/abc.jpg"));
        assert_eq!((m.category.as_str(), m.category_id), ("Filme HD", Some(4)));
        assert_eq!(m.size, "14.52 GB");
        assert_eq!((m.seeders, m.leechers), (Some(456), Some(12)));
        assert_eq!(m.added, "13:37 23/10/2026");
        assert!(m.freeleech);
        // the game row is dropped by the movie filter
        let wanted = normalize_cats(&[]);
        assert!(!is_wanted(&page.items[1], &wanted));
    }

    #[test]
    fn parses_login_form() {
        let html = r#"<form action="/takelogin.php" method="post"><input type="hidden" name="validator" value="abc"><input name="username"><input type="password" name="password"><input type="checkbox" name="unlock" value="1" checked><input type="checkbox" name="other" value="1"><input type="submit" value="Login"></form>"#;
        let (action, fields) = parse_login_form(html).unwrap();
        assert_eq!(action, "/takelogin.php");
        assert!(fields.contains(&("validator".into(), "abc".into())));
        assert!(fields.contains(&("unlock".into(), "1".into())));
        assert!(!fields.iter().any(|(k, _)| k == "other"));
    }

    #[test]
    fn hourly_limit_page() {
        let html = "<div class='cblock-header'><h4>Eroare</h4></div><div class='cblock-content'><div class='cblock-innercontent'>
	Numarul maxim permis de actiuni pentru acest cont a fost depasit. Reveniti peste o ora.
	</div></div>";
        assert!(is_rate_limited(html));
        assert!(login_error(html).starts_with("Numarul maxim permis"));
        assert!(!is_rate_limited(ROW));
    }
}
