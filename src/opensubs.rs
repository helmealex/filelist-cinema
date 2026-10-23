//! OpenSubtitles.com REST API: find subtitles by IMDb id in seconds, instead of reading the whole movie to
//! extract embedded tracks. Needs a (free) API key; logging in raises the daily download limit.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::util::TtlCache;

const API: &str = "https://api.opensubtitles.com/api/v1";
const UA: &str = "FileListCinema v2.0";

#[derive(Clone, Debug, Serialize)]
pub struct OnlineSub {
    pub file_id: u64,
    pub language: String,
    pub release: String,
    pub downloads: u64,
    pub hearing_impaired: bool,
    pub machine_translated: bool,
}

pub struct OpenSubs {
    api_key: String,
    username: String,
    password: String,
    languages: String,
    http: reqwest::Client,
    token: Mutex<Option<(String, String)>>, // (token, api base)
    cache: TtlCache<Vec<OnlineSub>>,
}

impl OpenSubs {
    pub fn new(api_key: &str, username: &str, password: &str, languages: &str) -> Option<OpenSubs> {
        if api_key.trim().is_empty() {
            return None;
        }
        Some(OpenSubs {
            api_key: api_key.trim().to_string(),
            username: username.to_string(),
            password: password.to_string(),
            languages: languages.split(',').map(|l| l.trim().to_lowercase()).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(","),
            http: reqwest::Client::builder().timeout(Duration::from_secs(20)).user_agent(UA).build().expect("http client"),
            token: Mutex::new(None),
            cache: TtlCache::default(),
        })
    }

    fn languages(&self) -> Vec<String> {
        self.languages.split(',').map(str::to_string).collect()
    }

    /// Subtitles for an IMDb id ("tt0092666"), preferred languages first, most downloaded first.
    pub async fn search(&self, imdb: &str) -> Result<Vec<OnlineSub>> {
        if let Some(hit) = self.cache.get(imdb, Duration::from_secs(3600)) {
            return Ok(hit);
        }
        let numeric: u64 = imdb.trim_start_matches("tt").parse().context("invalid IMDb id")?;
        let res = self
            .http
            .get(format!("{API}/subtitles"))
            .header("Api-Key", &self.api_key)
            .query(&[("imdb_id", numeric.to_string()), ("languages", self.languages.clone()), ("order_by", "download_count".into())])
            .send()
            .await?;
        if !res.status().is_success() {
            bail!("OpenSubtitles search failed (HTTP {})", res.status());
        }
        let data: Value = res.json().await?;
        let langs = self.languages();
        let mut subs: Vec<OnlineSub> = data["data"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|item| {
                let a = &item["attributes"];
                let file = a["files"].as_array()?.first()?;
                Some(OnlineSub {
                    file_id: file["file_id"].as_u64()?,
                    language: a["language"].as_str().unwrap_or("").to_lowercase(),
                    release: a["release"].as_str().or(file["file_name"].as_str()).unwrap_or("").to_string(),
                    downloads: a["download_count"].as_u64().unwrap_or(0),
                    hearing_impaired: a["hearing_impaired"].as_bool().unwrap_or(false),
                    machine_translated: a["machine_translated"].as_bool().unwrap_or(false) || a["ai_translated"].as_bool().unwrap_or(false),
                })
            })
            .collect();
        subs.sort_by_key(|s| {
            (langs.iter().position(|l| *l == s.language).unwrap_or(99), s.machine_translated, std::cmp::Reverse(s.downloads))
        });
        subs.truncate(12);
        self.cache.put(imdb, subs.clone());
        Ok(subs)
    }

    async fn auth(&self) -> Option<(String, String)> {
        if self.username.is_empty() {
            return None;
        }
        let mut token = self.token.lock().await;
        if token.is_none() {
            let res = self
                .http
                .post(format!("{API}/login"))
                .header("Api-Key", &self.api_key)
                .json(&json!({ "username": self.username, "password": self.password }))
                .send()
                .await
                .ok()?;
            let data: Value = res.json().await.ok()?;
            let t = data["token"].as_str()?.to_string();
            let base = data["base_url"].as_str().map(|b| format!("https://{b}/api/v1")).unwrap_or_else(|| API.to_string());
            *token = Some((t, base));
        }
        token.clone()
    }

    /// The subtitle file's bytes (as published: usually SRT).
    pub async fn download(&self, file_id: u64) -> Result<Vec<u8>> {
        let auth = self.auth().await;
        let base = auth.as_ref().map(|(_, b)| b.clone()).unwrap_or_else(|| API.to_string());
        let mut req = self
            .http
            .post(format!("{base}/download"))
            .header("Api-Key", &self.api_key)
            .header("Accept", "application/json")
            .json(&json!({ "file_id": file_id }));
        if let Some((token, _)) = &auth {
            req = req.bearer_auth(token);
        }
        let res = req.send().await?;
        let status = res.status();
        let data: Value = res.json().await.unwrap_or(Value::Null);
        let Some(link) = data["link"].as_str() else {
            let msg = data["message"].as_str().or(data["errors"][0].as_str()).unwrap_or("no download link");
            bail!("OpenSubtitles download failed (HTTP {status}): {msg}");
        };
        Ok(self.http.get(link).send().await?.bytes().await?.to_vec())
    }
}
