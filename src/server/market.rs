//! Public App Store market data: any app's listing, ratings and reviews, a
//! keyword's top results, and Apple's own search suggestions.
//!
//! These read Apple's public endpoints (the iTunes Search API, the customer
//! reviews RSS feed and the App Store search-hints service), not App Store
//! Connect, so they need no API key and work for other developers' apps. They
//! replace the iOS half of the old `mcp-appstore` Node server.

use std::collections::{BTreeMap, HashMap};

use rmcp::{
    handler::server::wrapper::Parameters, model::*, schemars, tool, tool_router,
    ErrorData as McpError,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::AppStoreServer;
use crate::error::AscError;

const ITUNES_BASE: &str = "https://itunes.apple.com";
const HINTS_BASE: &str = "https://search.itunes.apple.com";
/// Apple's RSS review feed serves at most 10 pages of 50.
const MAX_REVIEW_PAGES: u32 = 10;

/// HTTP client and base URLs for the public endpoints. Bases are fields so
/// tests can point them at a mock server.
#[derive(Debug, Clone)]
pub struct MarketClient {
    http: reqwest::Client,
    itunes_base: String,
    hints_base: String,
}

impl Default for MarketClient {
    fn default() -> Self {
        Self::with_bases(ITUNES_BASE, HINTS_BASE)
    }
}

impl MarketClient {
    pub fn with_bases(itunes_base: &str, hints_base: &str) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("asc-mcp/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("reqwest client builds");
        Self {
            http,
            itunes_base: itunes_base.trim_end_matches('/').to_string(),
            hints_base: hints_base.trim_end_matches('/').to_string(),
        }
    }

    async fn get(
        &self,
        url: &str,
        query: &[(&str, String)],
        headers: &[(&str, String)],
    ) -> Result<String, AscError> {
        let mut req = self.http.get(url).query(query);
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(AscError::Api {
                status: status.as_u16(),
                errors: Vec::new(),
                raw: Some(text.chars().take(300).collect()),
            });
        }
        Ok(text)
    }

    async fn get_json(&self, url: &str, query: &[(&str, String)]) -> Result<Value, AscError> {
        let text = self.get(url, query, &[]).await?;
        serde_json::from_str(&text)
            .map_err(|e| AscError::InvalidRequest(format!("Apple returned non-JSON: {e}")))
    }

    async fn search(&self, term: &str, country: &str, limit: u32) -> Result<Vec<Value>, AscError> {
        let v = self
            .get_json(
                &format!("{}/search", self.itunes_base),
                &[
                    ("term", term.to_string()),
                    ("entity", "software".into()),
                    ("limit", limit.to_string()),
                    ("country", country.to_string()),
                ],
            )
            .await?;
        Ok(results(&v).iter().map(map_app).collect())
    }

    async fn lookup(&self, app: &str, country: &str) -> Result<Value, AscError> {
        let key = if app.chars().all(|c| c.is_ascii_digit()) {
            "id"
        } else {
            "bundleId"
        };
        let v = self
            .get_json(
                &format!("{}/lookup", self.itunes_base),
                &[(key, app.to_string()), ("country", country.to_string())],
            )
            .await?;
        results(&v).first().map(map_app).ok_or_else(|| {
            AscError::InvalidRequest(format!(
                "no app '{app}' on the {} App Store",
                country.to_uppercase()
            ))
        })
    }

    async fn developer_apps(
        &self,
        developer_id: &str,
        country: &str,
        limit: u32,
    ) -> Result<Vec<Value>, AscError> {
        let v = self
            .get_json(
                &format!("{}/lookup", self.itunes_base),
                &[
                    ("id", developer_id.to_string()),
                    ("entity", "software".into()),
                    ("limit", limit.to_string()),
                    ("country", country.to_string()),
                ],
            )
            .await?;
        Ok(results(&v)
            .iter()
            .filter(|r| r["wrapperType"] == "software")
            .map(map_app)
            .collect())
    }

    async fn reviews(
        &self,
        app_id: &str,
        country: &str,
        page: u32,
        sort: &str,
    ) -> Result<Vec<Value>, AscError> {
        let url = format!(
            "{}/{}/rss/customerreviews/page={page}/id={app_id}/sortby={sort}/json",
            self.itunes_base, country
        );
        let v = self.get_json(&url, &[]).await?;
        // A single review comes back as an object, not a one-element array.
        let entries = match &v["feed"]["entry"] {
            Value::Array(a) => a.clone(),
            Value::Object(_) => vec![v["feed"]["entry"].clone()],
            _ => Vec::new(),
        };
        Ok(entries
            .iter()
            .filter(|e| e.get("im:rating").is_some())
            .map(map_review)
            .collect())
    }

    async fn hints(&self, term: &str, storefront: u32) -> Result<Vec<String>, AscError> {
        let text = self
            .get(
                &format!("{}/WebObjects/MZSearchHints.woa/wa/hints", self.hints_base),
                &[
                    ("clientApplication", "Software".into()),
                    ("term", term.to_string()),
                ],
                &[("X-Apple-Store-Front", format!("{storefront}-1,29"))],
            )
            .await?;
        Ok(parse_hint_terms(&text))
    }

    /// Resolve an app given as a numeric ID or a bundle ID to its numeric ID.
    async fn numeric_id(&self, app: &str, country: &str) -> Result<String, AscError> {
        if app.chars().all(|c| c.is_ascii_digit()) {
            return Ok(app.to_string());
        }
        let details = self.lookup(app, country).await?;
        Ok(details["id"].to_string())
    }
}

fn results(v: &Value) -> Vec<Value> {
    v["results"].as_array().cloned().unwrap_or_default()
}

/// The fields an ASO question needs, under readable names.
fn map_app(r: &Value) -> Value {
    json!({
        "id": r["trackId"],
        "bundleId": r["bundleId"],
        "name": r["trackName"],
        "url": r["trackViewUrl"],
        "developer": r["artistName"],
        "developerId": r["artistId"],
        "seller": r["sellerName"],
        "genre": r["primaryGenreName"],
        "genres": r["genres"],
        "rating": r["averageUserRating"],
        "ratingCount": r["userRatingCount"],
        "price": r["price"],
        "formattedPrice": r["formattedPrice"],
        "currency": r["currency"],
        "version": r["version"],
        "released": r["releaseDate"],
        "updated": r["currentVersionReleaseDate"],
        "releaseNotes": r["releaseNotes"],
        "description": r["description"],
        "contentRating": r["contentAdvisoryRating"],
        "minimumOsVersion": r["minimumOsVersion"],
        "sizeBytes": r["fileSizeBytes"],
        "languages": r["languageCodesISO2A"],
        "icon": r["artworkUrl512"],
        "screenshotUrls": r["screenshotUrls"],
        "ipadScreenshotUrls": r["ipadScreenshotUrls"],
    })
}

fn label(v: &Value) -> Value {
    v["label"].clone()
}

fn map_review(e: &Value) -> Value {
    json!({
        "id": label(&e["id"]),
        "author": label(&e["author"]["name"]),
        "rating": label(&e["im:rating"]).as_str().and_then(|s| s.parse::<u8>().ok()),
        "version": label(&e["im:version"]),
        "title": label(&e["title"]),
        "text": label(&e["content"]),
        "updated": label(&e["updated"]),
    })
}

/// Pull the suggested terms out of the hints plist, in Apple's order (most
/// popular first). The plist is flat enough that a scan beats a parser.
fn parse_hint_terms(plist: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = plist;
    while let Some(i) = rest.find("<key>term</key>") {
        rest = &rest[i + "<key>term</key>".len()..];
        let Some(start) = rest.find("<string>") else {
            break;
        };
        let after = &rest[start + "<string>".len()..];
        let Some(end) = after.find("</string>") else {
            break;
        };
        out.push(unescape_xml(&after[..end]));
        rest = &after[end..];
    }
    out
}

fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// App Store storefront IDs for the `X-Apple-Store-Front` header.
fn storefront(country: &str) -> Option<u32> {
    Some(match country.to_ascii_lowercase().as_str() {
        "us" => 143441,
        "fr" => 143442,
        "de" => 143443,
        "gb" => 143444,
        "at" => 143445,
        "be" => 143446,
        "fi" => 143447,
        "gr" => 143448,
        "ie" => 143449,
        "it" => 143450,
        "lu" => 143451,
        "nl" => 143452,
        "pt" => 143453,
        "es" => 143454,
        "ca" => 143455,
        "se" => 143456,
        "no" => 143457,
        "dk" => 143458,
        "ch" => 143459,
        "au" => 143460,
        "nz" => 143461,
        "jp" => 143462,
        "hk" => 143463,
        "sg" => 143464,
        "cn" => 143465,
        "kr" => 143466,
        "in" => 143467,
        "mx" => 143468,
        "ru" => 143469,
        "tw" => 143470,
        "br" => 143503,
        _ => return None,
    })
}

fn country_or_default(country: &Option<String>) -> String {
    country
        .as_deref()
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "us".into())
}

// ---- Analysis ----------------------------------------------------------------

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "you", "this", "that", "with", "app", "but", "not", "are", "was", "have",
    "its", "it's", "can", "all", "just", "they", "would", "very", "when", "what", "has", "get",
    "from", "out", "one", "use", "there", "your", "will", "more", "been", "like", "only", "also",
    "even", "about", "please", "i'm", "don't", "really", "much", "had", "some", "any", "now",
    "how", "which", "than", "them", "were", "could", "make", "after", "every", "into", "time",
];

/// Ratings distribution, average, per-version averages and the words that
/// recur in low and high reviews.
fn analyze_reviews(reviews: &[Value]) -> Value {
    let mut dist: BTreeMap<u8, usize> = BTreeMap::new();
    let mut by_version: BTreeMap<String, (usize, u32)> = BTreeMap::new();
    let mut low_words: HashMap<String, usize> = HashMap::new();
    let mut high_words: HashMap<String, usize> = HashMap::new();
    let mut sum = 0u32;
    let mut n = 0usize;
    for r in reviews {
        let Some(rating) = r["rating"].as_u64().map(|x| x as u8) else {
            continue;
        };
        n += 1;
        sum += rating as u32;
        *dist.entry(rating).or_default() += 1;
        let version = r["version"].as_str().unwrap_or("unknown").to_string();
        let e = by_version.entry(version).or_default();
        e.0 += 1;
        e.1 += rating as u32;
        let bucket = match rating {
            1 | 2 => Some(&mut low_words),
            4 | 5 => Some(&mut high_words),
            _ => None,
        };
        if let Some(bucket) = bucket {
            let text = format!(
                "{} {}",
                r["title"].as_str().unwrap_or(""),
                r["text"].as_str().unwrap_or("")
            )
            .to_lowercase();
            let mut seen = std::collections::HashSet::new();
            for w in text
                .split(|c: char| !(c.is_alphanumeric() || c == '\''))
                .filter(|w| w.chars().count() >= 3 && !STOPWORDS.contains(w))
            {
                if seen.insert(w.to_string()) {
                    *bucket.entry(w.to_string()).or_default() += 1;
                }
            }
        }
    }
    let top = |m: HashMap<String, usize>| -> Vec<Value> {
        let mut v: Vec<(String, usize)> = m.into_iter().filter(|(_, c)| *c >= 2).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.into_iter()
            .take(20)
            .map(|(w, c)| json!({ "word": w, "reviews": c }))
            .collect()
    };
    let mut versions: Vec<Value> = by_version
        .into_iter()
        .map(|(v, (count, total))| {
            json!({ "version": v, "reviews": count, "average": round2(total as f64 / count as f64) })
        })
        .collect();
    versions.sort_by(|a, b| b["reviews"].as_u64().cmp(&a["reviews"].as_u64()));
    json!({
        "reviewsAnalyzed": n,
        "average": if n > 0 { json!(round2(sum as f64 / n as f64)) } else { Value::Null },
        "distribution": dist.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_, _>>(),
        "byVersion": versions,
        "wordsInLowReviews": top(low_words),
        "wordsInHighReviews": top(high_words),
    })
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// How contested a keyword is, measured from its real top results.
fn analyze_keyword(keyword: &str, apps: &[Value]) -> Value {
    let kw = keyword.to_lowercase();
    let kw_words: Vec<&str> = kw.split_whitespace().collect();
    let mut developers: BTreeMap<String, usize> = BTreeMap::new();
    let mut counts: Vec<u64> = Vec::new();
    let mut ratings: Vec<f64> = Vec::new();
    let mut title_exact = 0usize;
    let mut title_all_words = 0usize;
    let mut paid = 0usize;
    for a in apps {
        let name = a["name"].as_str().unwrap_or("").to_lowercase();
        if name.contains(&kw) {
            title_exact += 1;
        }
        if kw_words.iter().all(|w| name.contains(w)) {
            title_all_words += 1;
        }
        if a["price"].as_f64().unwrap_or(0.0) > 0.0 {
            paid += 1;
        }
        *developers
            .entry(a["developer"].as_str().unwrap_or("?").to_string())
            .or_default() += 1;
        counts.push(a["ratingCount"].as_u64().unwrap_or(0));
        if let Some(r) = a["rating"].as_f64() {
            ratings.push(r);
        }
    }
    let mut sorted = counts.clone();
    sorted.sort_unstable();
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0);
    let top: Vec<Value> = apps
        .iter()
        .enumerate()
        .map(|(i, a)| {
            json!({
                "rank": i + 1,
                "id": a["id"],
                "name": a["name"],
                "developer": a["developer"],
                "rating": a["rating"],
                "ratingCount": a["ratingCount"],
                "price": a["price"],
                "updated": a["updated"],
            })
        })
        .collect();
    let mut repeat_devs: Vec<Value> = developers
        .into_iter()
        .filter(|(_, c)| *c > 1)
        .map(|(d, c)| json!({ "developer": d, "apps": c }))
        .collect();
    repeat_devs.sort_by(|a, b| b["apps"].as_u64().cmp(&a["apps"].as_u64()));
    json!({
        "keyword": keyword,
        "resultsAnalyzed": apps.len(),
        "medianRatingCount": median,
        "minRatingCountInTop10": counts.iter().take(10).min(),
        "averageRating": if ratings.is_empty() { Value::Null } else {
            json!(round2(ratings.iter().sum::<f64>() / ratings.len() as f64))
        },
        "titlesContainingKeyword": title_exact,
        "titlesContainingEveryWord": title_all_words,
        "paidApps": paid,
        "developersWithSeveralApps": repeat_devs,
        "topApps": top,
        "note": "Measured from the live iTunes Search API, whose ranking approximates but is not \
    identical to the App Store app's. Rating counts are a proxy for incumbents' strength; Apple \
    publishes no search volume. Use search_suggestions for demand.",
    })
}

// ---- Tools -------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StoreSearchArgs {
    /// Search term, e.g. "habit tracker".
    pub term: String,
    /// Two-letter storefront country (default "us").
    #[serde(default)]
    pub country: Option<String>,
    /// Results to return (1–200, default 25).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StoreAppArgs {
    /// The app's numeric App Store ID (e.g. "389801252") or bundle ID.
    pub app: String,
    /// Two-letter storefront country (default "us").
    #[serde(default)]
    pub country: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeveloperAppsArgs {
    /// The developer's numeric artist ID (the `developerId` field of get_store_app).
    pub developer_id: String,
    /// Two-letter storefront country (default "us").
    #[serde(default)]
    pub country: Option<String>,
    /// Maximum apps (default 50, maximum 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SimilarAppsArgs {
    /// The app's numeric App Store ID or bundle ID.
    pub app: String,
    /// Two-letter storefront country (default "us").
    #[serde(default)]
    pub country: Option<String>,
    /// Maximum apps (default 20, maximum 100).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StoreReviewsArgs {
    /// The app's numeric App Store ID or bundle ID. Works for any app.
    pub app: String,
    /// Two-letter storefront country (default "us"). Reviews are per storefront.
    #[serde(default)]
    pub country: Option<String>,
    /// "recent" (default) or "helpful".
    #[serde(default)]
    pub sort: Option<String>,
    /// Pages of 50 to fetch (1–10, default 1).
    #[serde(default)]
    pub pages: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KeywordArgs {
    /// The keyword or phrase, e.g. "meditation timer".
    pub keyword: String,
    /// Two-letter storefront country (default "us").
    #[serde(default)]
    pub country: Option<String>,
    /// Top results to analyze (1–100, default 25).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SuggestionsArgs {
    /// The start of a search, e.g. "medit". Apple returns what users type next.
    pub term: String,
    /// Two-letter storefront country (default "us"). Supported: us ca gb au nz
    /// ie fr de at ch be nl lu it es pt se no dk fi gr jp cn hk tw kr sg in mx
    /// br ru.
    #[serde(default)]
    pub country: Option<String>,
}

#[tool_router(router = market_router, vis = "pub(crate)")]
impl AppStoreServer {
    #[tool(
        description = "Search the public App Store as a user would (any developer's apps, no API \
key needed). Returns each app's ID, name, developer, rating, rating count, price, genre and \
last update. Ranking comes from the iTunes Search API and approximates the App Store app's."
    )]
    async fn search_store_apps(
        &self,
        Parameters(args): Parameters<StoreSearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let country = country_or_default(&args.country);
        let apps = self
            .market
            .search(&args.term, &country, args.limit.unwrap_or(25).clamp(1, 200))
            .await
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(json!({ "country": country, "results": apps }))
    }

    #[tool(
        description = "Get any app's public App Store listing by numeric ID or bundle ID: name, \
description, release notes, rating and rating count, price, genre, screenshots, languages, \
size and dates. No API key needed; works for competitors."
    )]
    async fn get_store_app(
        &self,
        Parameters(args): Parameters<StoreAppArgs>,
    ) -> Result<CallToolResult, McpError> {
        let country = country_or_default(&args.country);
        let app = self
            .market
            .lookup(&args.app, &country)
            .await
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(app)
    }

    #[tool(
        description = "List a developer's public App Store apps by numeric developer (artist) ID."
    )]
    async fn list_developer_store_apps(
        &self,
        Parameters(args): Parameters<DeveloperAppsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let country = country_or_default(&args.country);
        let apps = self
            .market
            .developer_apps(
                &args.developer_id,
                &country,
                args.limit.unwrap_or(50).clamp(1, 200),
            )
            .await
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(json!({ "country": country, "apps": apps }))
    }

    #[tool(
        description = "List apps competing with an app: the top public App Store results for its \
primary genre, excluding the app itself. A rough competitor set, not Apple's \"You might also \
like\" list."
    )]
    async fn list_similar_store_apps(
        &self,
        Parameters(args): Parameters<SimilarAppsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let country = country_or_default(&args.country);
        let limit = args.limit.unwrap_or(20).clamp(1, 100);
        let details = self
            .market
            .lookup(&args.app, &country)
            .await
            .map_err(AppStoreServer::map_err)?;
        let genre = details["genre"].as_str().unwrap_or_default().to_string();
        if genre.is_empty() {
            return self.ok_json(json!({ "genre": null, "apps": [] }));
        }
        let apps: Vec<Value> = self
            .market
            .search(&genre, &country, (limit + 1).min(200))
            .await
            .map_err(AppStoreServer::map_err)?
            .into_iter()
            .filter(|a| a["id"] != details["id"])
            .take(limit as usize)
            .collect();
        self.ok_json(json!({ "genre": genre, "apps": apps }))
    }

    #[tool(
        description = "Fetch any app's public App Store customer reviews for one storefront \
(rating, title, text, version, date), most recent or most helpful first, up to 500. For your \
own apps, list_customer_reviews reads App Store Connect and can respond."
    )]
    async fn list_store_reviews(
        &self,
        Parameters(args): Parameters<StoreReviewsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let country = country_or_default(&args.country);
        let reviews = self
            .fetch_reviews(&args.app, &country, args.sort.as_deref(), args.pages)
            .await
            .map_err(AppStoreServer::map_err)?;
        self.ok_json(json!({ "country": country, "count": reviews.len(), "reviews": reviews }))
    }

    #[tool(
        description = "Analyze any app's public reviews in one storefront: rating distribution, \
average, average by app version, and the words that recur in 1–2 star versus 4–5 star \
reviews. Reads up to 500 reviews."
    )]
    async fn analyze_store_reviews(
        &self,
        Parameters(args): Parameters<StoreReviewsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let country = country_or_default(&args.country);
        let reviews = self
            .fetch_reviews(
                &args.app,
                &country,
                args.sort.as_deref(),
                args.pages.or(Some(4)),
            )
            .await
            .map_err(AppStoreServer::map_err)?;
        let mut v = analyze_reviews(&reviews);
        v["country"] = json!(country);
        self.ok_json(v)
    }

    #[tool(
        description = "Measure how contested an App Store keyword is from its live top results: \
median and minimum rating counts of the apps ranking for it, average rating, how many titles \
contain it, paid share, developers holding several slots, and the ranked apps. Pair with \
search_suggestions for demand."
    )]
    async fn analyze_store_keyword(
        &self,
        Parameters(args): Parameters<KeywordArgs>,
    ) -> Result<CallToolResult, McpError> {
        let country = country_or_default(&args.country);
        let apps = self
            .market
            .search(
                &args.keyword,
                &country,
                args.limit.unwrap_or(25).clamp(1, 100),
            )
            .await
            .map_err(AppStoreServer::map_err)?;
        let mut v = analyze_keyword(&args.keyword, &apps);
        v["country"] = json!(country);
        self.ok_json(v)
    }

    #[tool(
        description = "Get the App Store's own search suggestions for a partial term, in Apple's \
order (most searched first): the autocomplete users see. A real demand signal for keywords; \
an empty list means few people search it."
    )]
    async fn search_suggestions(
        &self,
        Parameters(args): Parameters<SuggestionsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let country = country_or_default(&args.country);
        let front = storefront(&country).ok_or_else(|| {
            AppStoreServer::map_err(AscError::InvalidRequest(format!(
                "no storefront ID known for '{country}'"
            )))
        })?;
        let terms = self
            .market
            .hints(&args.term, front)
            .await
            .map_err(AppStoreServer::map_err)?;
        let ranked: Vec<Value> = terms
            .iter()
            .enumerate()
            .map(|(i, t)| json!({ "rank": i + 1, "term": t }))
            .collect();
        self.ok_json(json!({ "term": args.term, "country": country, "suggestions": ranked }))
    }
}

impl AppStoreServer {
    async fn fetch_reviews(
        &self,
        app: &str,
        country: &str,
        sort: Option<&str>,
        pages: Option<u32>,
    ) -> Result<Vec<Value>, AscError> {
        let sort = match sort.unwrap_or("recent") {
            "helpful" | "mostHelpful" => "mostHelpful",
            _ => "mostRecent",
        };
        let id = self.market.numeric_id(app, country).await?;
        let mut all = Vec::new();
        for page in 1..=pages.unwrap_or(1).clamp(1, MAX_REVIEW_PAGES) {
            let batch = self.market.reviews(&id, country, page, sort).await?;
            if batch.is_empty() {
                break;
            }
            all.extend(batch);
        }
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_terms_come_out_in_order_and_unescaped() {
        let plist = r#"<plist><dict><key>hints</key><array>
            <dict><key>term</key><string>meditation</string><key>url</key><string>x</string></dict>
            <dict><key>term</key><string>sleep &amp; meditation</string></dict>
        </array></dict></plist>"#;
        assert_eq!(
            parse_hint_terms(plist),
            ["meditation", "sleep & meditation"]
        );
        assert!(parse_hint_terms("<plist></plist>").is_empty());
    }

    #[test]
    fn review_analysis_counts_and_buckets() {
        let reviews = vec![
            json!({ "rating": 1, "version": "2.0", "title": "Crashes", "text": "crashes on launch" }),
            json!({ "rating": 2, "version": "2.0", "title": "Bad", "text": "crashes constantly" }),
            json!({ "rating": 5, "version": "1.9", "title": "Love it", "text": "simple and calm" }),
        ];
        let a = analyze_reviews(&reviews);
        assert_eq!(a["reviewsAnalyzed"], 3);
        assert_eq!(a["average"], 2.67);
        assert_eq!(a["distribution"]["1"], 1);
        assert_eq!(a["wordsInLowReviews"][0]["word"], "crashes");
        assert_eq!(a["byVersion"][0]["version"], "2.0");
    }

    #[test]
    fn keyword_analysis_measures_titles_and_incumbents() {
        let apps = vec![
            json!({ "name": "Habit Tracker", "developer": "A", "ratingCount": 100, "rating": 4.5, "price": 0.0 }),
            json!({ "name": "Daily Habits", "developer": "A", "ratingCount": 10, "rating": 4.0, "price": 2.99 }),
            json!({ "name": "Streaks", "developer": "B", "ratingCount": 1000, "rating": 4.8, "price": 0.0 }),
        ];
        let k = analyze_keyword("habit tracker", &apps);
        assert_eq!(k["titlesContainingKeyword"], 1);
        assert_eq!(k["medianRatingCount"], 100);
        assert_eq!(k["paidApps"], 1);
        assert_eq!(k["developersWithSeveralApps"][0]["developer"], "A");
    }

    #[tokio::test]
    async fn reviews_resolve_a_bundle_id_and_stop_at_an_empty_page() {
        use crate::testing::{result_text, test_server_with_market};
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/lookup"))
            .and(query_param("bundleId", "com.example.app"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [{ "trackId": 42, "trackName": "Example" }]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/us/rss/customerreviews/page=1/id=42/sortby=mostRecent/json",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "feed": { "entry": {
                    "id": { "label": "r1" }, "author": { "name": { "label": "Ann" } },
                    "im:rating": { "label": "4" }, "im:version": { "label": "1.0" },
                    "title": { "label": "Nice" }, "content": { "label": "Works" },
                    "updated": { "label": "2026-10-01" }
                }}})),
            )
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/us/rss/customerreviews/page=2/id=42/sortby=mostRecent/json",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "feed": {} })))
            .expect(1)
            .mount(&mock)
            .await;

        let result = test_server_with_market(&mock.uri())
            .list_store_reviews(Parameters(StoreReviewsArgs {
                app: "com.example.app".into(),
                country: None,
                sort: None,
                pages: Some(5),
            }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["reviews"][0]["rating"], 4);
    }

    #[test]
    fn storefronts_cover_the_main_markets() {
        assert_eq!(storefront("US"), Some(143441));
        assert_eq!(storefront("ca"), Some(143455));
        assert_eq!(storefront("zz"), None);
    }
}
