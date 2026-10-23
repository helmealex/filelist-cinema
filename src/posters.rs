//! Poster + basic title info from IMDb's public suggestion endpoint (no API key). Cached in data/posters.json.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, Notify};

use crate::util::parse_release_name;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Meta {
    pub imdb: String,
    pub title: Option<String>,
    pub year: Option<i64>,
    pub stars: Option<String>,
    pub poster: String,
    #[serde(rename = "posterLarge")]
    pub poster_large: String,
}

pub struct Posters {
    file: PathBuf,
    // key -> Some(meta) or None (looked up, nothing found)
    cache: Mutex<HashMap<String, Option<Meta>>>,
    pending: Mutex<HashMap<String, Arc<Notify>>>,
    http: reqwest::Client,
    save: Notify,
}

impl Posters {
    pub fn new(data_dir: &std::path::Path) -> Arc<Posters> {
        let file = data_dir.join("posters.json");
        let mut cache = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(&file) {
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) {
                for (k, v) in map {
                    // older caches stored only the poster URL; those are looked up again for the full info
                    match v {
                        Value::Null => {
                            cache.insert(k, None);
                        }
                        Value::Object(_) => {
                            if let Ok(meta) = serde_json::from_value::<Meta>(v) {
                                cache.insert(k, Some(meta));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        let posters = Arc::new(Posters {
            file,
            cache: Mutex::new(cache),
            pending: Mutex::new(HashMap::new()),
            http: reqwest::Client::builder().timeout(Duration::from_secs(8)).build().expect("http client"),
            save: Notify::new(),
        });
        // debounced saving
        let p = posters.clone();
        tokio::spawn(async move {
            loop {
                p.save.notified().await;
                tokio::time::sleep(Duration::from_secs(2)).await;
                let json = serde_json::to_string(&*p.cache.lock().await).unwrap_or_default();
                let _ = tokio::fs::write(&p.file, json).await;
            }
        });
        posters
    }

    pub async fn meta(&self, imdb: Option<&str>, name: &str) -> Option<Meta> {
        let (title, year) = parse_release_name(name);
        let key = match imdb.filter(|s| !s.is_empty()) {
            Some(id) => id.to_string(),
            None => format!("n:{}|{}", title.to_lowercase(), year.clone().unwrap_or_default()),
        };
        loop {
            if let Some(hit) = self.cache.lock().await.get(&key) {
                return hit.clone();
            }
            // one lookup per key at a time; others wait for it
            let wait = {
                let mut pending = self.pending.lock().await;
                match pending.get(&key) {
                    Some(n) => Some(n.clone()),
                    None => {
                        pending.insert(key.clone(), Arc::new(Notify::new()));
                        None
                    }
                }
            };
            if let Some(n) = wait {
                // register before re-checking the cache, so a lookup finishing in between can't be missed
                let notified = n.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.cache.lock().await.contains_key(&key) {
                    continue;
                }
                notified.await;
                continue;
            }
            let found = self.lookup(imdb, &title, year.as_deref()).await;
            self.cache.lock().await.insert(key.clone(), found.clone());
            if let Some(n) = self.pending.lock().await.remove(&key) {
                n.notify_waiters();
            }
            self.save.notify_one();
            return found;
        }
    }

    async fn lookup(&self, imdb: Option<&str>, title: &str, year: Option<&str>) -> Option<Meta> {
        let mut hit: Option<Value> = None;
        if let Some(id) = imdb.filter(|s| !s.is_empty()) {
            hit = self.suggest(id).await.into_iter().find(|d| d["id"] == id && d["i"]["imageUrl"].is_string());
        }
        if hit.is_none() && !title.is_empty() {
            let q = match year {
                Some(y) => format!("{title} {y}"),
                None => title.to_string(),
            };
            let results: Vec<Value> = self
                .suggest(&q)
                .await
                .into_iter()
                .filter(|d| d["id"].as_str().is_some_and(|s| s.starts_with("tt")) && d["i"]["imageUrl"].is_string())
                .collect();
            hit = results
                .iter()
                .find(|d| year.is_some_and(|y| d["y"].to_string() == y))
                .or(results.first())
                .cloned();
        }
        let hit = hit?;
        let image = hit["i"]["imageUrl"].as_str()?;
        Some(Meta {
            imdb: hit["id"].as_str()?.to_string(),
            title: hit["l"].as_str().map(str::to_string),
            year: hit["y"].as_i64(),
            stars: hit["s"].as_str().map(str::to_string),
            poster: resize(image, 400),
            poster_large: resize(image, 1280),
        })
    }

    async fn suggest(&self, query: &str) -> Vec<Value> {
        let q = normalize(query);
        let Some(first) = q.chars().next() else { return vec![] };
        let url = format!(
            "https://v2.sg.media-imdb.com/suggestion/{}/{}.json",
            urlencoding(&first.to_string()),
            urlencoding(&q)
        );
        let Ok(res) = self.http.get(url).send().await else { return vec![] };
        if !res.status().is_success() {
            return vec![];
        }
        res.json::<Value>().await.ok().and_then(|v| v["d"].as_array().cloned()).unwrap_or_default()
    }
}

fn normalize(s: &str) -> String {
    let folded: String = s
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'ă' | 'â' | 'á' | 'à' | 'ä' => 'a',
            'î' | 'í' | 'ì' => 'i',
            'ș' | 'ş' => 's',
            'ț' | 'ţ' => 't',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'ó' | 'ò' | 'ö' | 'ô' => 'o',
            'ú' | 'ù' | 'ü' => 'u',
            c if c.is_ascii_alphanumeric() => c,
            _ => ' ',
        })
        .collect();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn urlencoding(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn resize(url: &str, width: u32) -> String {
    static V1: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\._V1_.*?\.(jpe?g|png)$").unwrap());
    V1.replace(url, format!("._V1_QL75_UX{width}_.$1").as_str()).into_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn resizes_imdb_urls() {
        assert_eq!(
            super::resize("https://m.media-amazon.com/images/M/abc@._V1_.jpg", 400),
            "https://m.media-amazon.com/images/M/abc@._V1_QL75_UX400_.jpg"
        );
        assert_eq!(super::normalize("Blind Date 1987"), "blind date 1987");
    }
}
