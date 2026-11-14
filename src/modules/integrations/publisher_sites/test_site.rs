//! An in-memory publisher site for the probe and check tests.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use url::Url;

use super::fetch::{FetchError, Page, PostAnswer, SiteFetcher};

/// Address -> page. Unknown addresses are 404s; every address asked is
/// logged.
#[derive(Default)]
pub struct FakeSite {
    pub pages: HashMap<String, String>,
    pub asked: Mutex<Vec<String>>,
    pub blocked: bool,
    /// Path prefixes robots.txt forbids.
    pub forbidden: Vec<String>,
    /// Site maps robots.txt declares.
    pub sitemaps: Vec<String>,
}

impl FakeSite {
    pub fn page(mut self, url: &str, html: &str) -> Self {
        self.pages.insert(url.to_owned(), html.to_owned());
        self
    }
}

#[async_trait]
impl SiteFetcher for FakeSite {
    async fn get(&self, url: &Url) -> Result<Page, FetchError> {
        self.asked.lock().unwrap().push(url.to_string());
        if self.blocked {
            return Err(FetchError::Blocked("HTTP 403".into()));
        }
        if self
            .forbidden
            .iter()
            .any(|p| url.path().starts_with(p.as_str()))
        {
            return Err(FetchError::Disallowed);
        }
        self.pages
            .get(url.as_str())
            .map(|html| Page {
                url: url.clone(),
                html: html.clone(),
            })
            .ok_or(FetchError::Status(404))
    }

    async fn post_form(&self, _: &Url, _: &[(String, String)]) -> Result<PostAnswer, FetchError> {
        Err(FetchError::Status(405))
    }

    async fn sitemaps(&self, _: &Url) -> Vec<Url> {
        self.sitemaps
            .iter()
            .filter_map(|s| Url::parse(s).ok())
            .collect()
    }
}

/// A book page as publishers write them: JSON-LD `Book` and an `<h1>`.
pub fn book_page(isbn: &str, title: &str) -> String {
    format!(
        r#"<html><head><script type="application/ld+json">{{"@type":"Book","name":"{title}","isbn":"{isbn}","author":{{"name":"Anne Auteur"}}}}</script></head><body><h1>{title}</h1></body></html>"#
    )
}

/// A menu and a few links, so a result page weighs what a real one does.
pub fn padding() -> String {
    format!("<nav>{}</nav>", r#"<a href="/rayon">Rayon</a>"#.repeat(80))
}
