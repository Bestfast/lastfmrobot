use std::{error::Error, future::Future, sync::LazyLock, time::Duration};

use http::Extensions;
use http_cache_reqwest::{Cache, CacheMode, CacheOptions, HttpCache, MokaManager};
use reqwest::{Request, Response, StatusCode, Url, header::HeaderValue};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware, Middleware, Next};
use serde_json::Value;
use strum_macros::{Display, EnumString, IntoStaticStr};

use crate::{config, consts};

#[derive(Debug)]
pub struct Track {
    pub name: String,
    pub album: Option<String>,
    pub artist: String,
    pub album_art_url: Option<String>,
    pub date: Option<u64>,
    pub duration: u64,
    pub listeners: u64,
    pub playcount: u64,
    pub user_playcount: u64,
    pub user_loved: bool,
    pub now_playing: bool,
    pub tags: Option<Vec<String>>,
    pub recording_mbid: Option<String>,
    pub release_mbid: Option<String>,
    pub release_group_mbid: Option<String>,
}

#[derive(Debug)]
pub struct Album {
    pub name: String,
    pub artist: String,
    pub album_art_url: Option<String>,
    pub playcount: u64,
    pub listeners: u64,
    pub user_playcount: u64,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug)]
pub struct Artist {
    pub name: String,
    pub playcount: u64,
    pub listeners: u64,
    pub user_playcount: u64,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug)]
pub struct ScrobbleUser {
    pub username: String,
    pub playcount: u64,
    pub artist_count: u64,
    pub album_count: u64,
    pub track_count: u64,
    pub profile_pic_url: Option<String>,
    pub registered_date: Option<u64>,
}

#[derive(Debug, PartialEq, EnumString, Display, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ApiType {
    Lastfm,
    Librefm,
    Listenbrainz,
}

#[derive(Debug, PartialEq, EnumString, Display, IntoStaticStr)]
pub enum TimePeriod {
    #[strum(serialize = "1 week")]
    OneWeek,
    #[strum(serialize = "1 month")]
    OneMonth,
    #[strum(serialize = "3 months")]
    ThreeMonths,
    #[strum(serialize = "6 months")]
    SixMonths,
    #[strum(serialize = "1 year")]
    OneYear,
    #[strum(serialize = "All time")]
    AllTime,
}

#[derive(Debug, PartialEq, EnumString, Display, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum EntryType {
    Artist,
    Album,
    Track,
}

struct ForceCacheMiddleware {}

#[async_trait::async_trait]
impl Middleware for ForceCacheMiddleware {
    async fn handle(
        &self,
        mut req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<Response> {
        let no_cache = req
            .headers()
            .get("cache-control")
            .map(|h| h.to_str().unwrap_or_default())
            .unwrap_or_default()
            .contains("no-cache");

        if !no_cache {
            req.headers_mut().append(
                "cache-control",
                HeaderValue::from_str("max-stale=300").unwrap(),
            );
        }

        let mut resp = next.run(req, extensions).await?;

        if !no_cache {
            resp.headers_mut().insert(
                "cache-control",
                HeaderValue::from_str("max-age=300, public, immutable").unwrap(),
            );
        }
        Ok(resp)
    }
}

struct Response200Middleware {}
#[async_trait::async_trait]
impl Middleware for Response200Middleware {
    async fn handle(
        &self,
        req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<Response> {
        let resp = next.run(req, extensions).await?;
        if resp.status().is_success() {
            Ok(resp)
        } else {
            let display_msg = match resp.status() {
                StatusCode::NOT_FOUND => consts::USER_NOT_FOUND,
                StatusCode::FORBIDDEN => consts::PRIVATE_PROFILE,
                _ => resp.status().canonical_reason().unwrap_or(consts::ERR_MSG),
            };

            return Err(reqwest_middleware::Error::Middleware(anyhow::anyhow!(
                display_msg
            )));
        }
    }
}

pub static CLIENT: LazyLock<ClientWithMiddleware> = LazyLock::new(|| {
    ClientBuilder::new(
        reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(25))
            .https_only(true)
            .user_agent("LastFM Robot (Telegram bot)")
            .build()
            .unwrap(),
    )
    .with(Response200Middleware {})
    .with(ForceCacheMiddleware {})
    .with(Cache(HttpCache {
        mode: CacheMode::Default,
        manager: MokaManager::new(
            moka::future::Cache::builder()
                .max_capacity(100)
                .time_to_live(Duration::from_secs(300))
                .build(),
        ),
        options: http_cache_reqwest::HttpCacheOptions {
            cache_options: CacheOptions {
                shared: false,
                immutable_min_time_to_live: Duration::from_secs(300),
                ignore_cargo_cult: true,
                ..Default::default()
            }
            .into(),
            ..Default::default()
        },
    }))
    .build()
});

pub static CLIENT_NOCACHE: LazyLock<ClientWithMiddleware> = LazyLock::new(|| {
    ClientBuilder::new(
        reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(25))
            .https_only(true)
            .build()
            .unwrap(),
    )
    .with(Response200Middleware {})
    .build()
});

// Probing needs the *raw* status code (not Response200Middleware's error) so a clean 404
// ("definitively no art") can be told apart from a timeout/5xx ("transient, don't cache").
static CLIENT_PROBE: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .https_only(true)
        .user_agent("LastFM Robot (Telegram bot)")
        .build()
        .unwrap()
});

// MusicBrainz requires a descriptive User-Agent (with contact info) or it answers 403.
// Responses are cached like the other clients; genres are effectively immutable.
static CLIENT_MB: LazyLock<ClientWithMiddleware> = LazyLock::new(|| {
    ClientBuilder::new(
        reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(15))
            .https_only(true)
            .user_agent("lastfmrobot/0.2 (+https://github.com/Bestfast/lastfmrobot)")
            .build()
            .unwrap(),
    )
    .with(Response200Middleware {})
    .with(ForceCacheMiddleware {})
    .with(Cache(HttpCache {
        mode: CacheMode::Default,
        manager: MokaManager::new(
            moka::future::Cache::builder()
                .max_capacity(500)
                .time_to_live(Duration::from_secs(6 * 60 * 60))
                .build(),
        ),
        options: http_cache_reqwest::HttpCacheOptions {
            cache_options: CacheOptions {
                shared: false,
                immutable_min_time_to_live: Duration::from_secs(6 * 60 * 60),
                ignore_cargo_cult: true,
                ..Default::default()
            }
            .into(),
            ..Default::default()
        },
    }))
    .build()
});

/// MusicBrainz genres for a track, tried release-group → recording → release (release
/// groups carry genres most often). ListenBrainz tracks carry these mbids, which makes
/// this the natural tag source; Last.fm's track-level toptags no longer exist. MB is
/// rate-limited, so failures (429/503) fall through to None and the caller falls back.
pub async fn fetch_mb_genres(
    recording_mbid: Option<&str>,
    release_group_mbid: Option<&str>,
    release_mbid: Option<&str>,
) -> Option<Vec<String>> {
    for mbid in [
        release_group_mbid.map(|m| format!("release-group/{m}")),
        recording_mbid.map(|m| format!("recording/{m}")),
        release_mbid.map(|m| format!("release/{m}")),
    ]
    .into_iter()
    .flatten()
    {
        let url = format!("https://musicbrainz.org/ws/2/{mbid}?inc=genres+tags&fmt=json");
        let Ok(response) = CLIENT_MB.get(&url).send().await else {
            continue;
        };
        let Ok(json) = response.json::<serde_json::Value>().await else {
            continue;
        };

        let genres: Vec<String> = json["genres"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|g| g["name"].as_str().map(str::to_string))
            .collect();
        if !genres.is_empty() {
            return Some(genres);
        }

        let tags: Vec<String> = json["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect();
        if !tags.is_empty() {
            return Some(tags);
        }
    }

    None
}

type BoxError = Box<dyn Error + Send + Sync>;

// ListenBrainz goes down fairly often and its stats endpoints lag behind fresh listens.
// When a ListenBrainz request fails or comes back with nothing usable, retry against
// Last.fm, assuming the user has the same username on both platforms. Both arguments are
// futures and so are lazy — the Last.fm request is only issued once it's awaited.
//
// Callers pass `None` as the Last.fm limit rather than forwarding their own: the limits
// they choose are sized for ListenBrainz (which caps out far lower), so reusing them
// would truncate the fallback for no reason.
async fn or_lastfm<T>(
    listenbrainz: impl Future<Output = Result<T, BoxError>>,
    lastfm: impl Future<Output = Result<T, BoxError>>,
    is_usable: impl FnOnce(&T) -> bool,
) -> Result<T, BoxError> {
    match listenbrainz.await {
        Ok(value) if is_usable(&value) => Ok(value),
        Ok(_) => lastfm.await,
        // Surface the ListenBrainz error if the fallback fails too — that's the service
        // the user actually configured, so its error is the one worth reporting.
        Err(lb_err) => lastfm.await.map_err(|_| lb_err),
    }
}

// Cover Art Archive serves the original upload at `/front` plus fixed thumbnails at
// `/front-250`, `/front-500` and `/front-1200` — those are the only sizes that exist.
// We prefer 500 (plenty sharp for Telegram, a fraction of the download) and step down to
// 250; only if neither exists do we fall back to 1200. The original `/front` is avoided
// on purpose: originals are unbounded — lossless scans of several MB are common, and
// Telegram rejects photos sent by URL above 10MB. CAA answers 307 for every size without
// checking, so a missing thumbnail only surfaces as a 404 from archive.org once the
// redirect is followed — hence the step-down list rather than a single URL.
pub const CAA_SIZES: [u16; 3] = [500, 250, 1200];

pub fn caa_front_url(mbid: &str, size: u16) -> String {
    format!("https://coverartarchive.org/release/{mbid}/front-{size}")
}

/// URL for the largest cover art size we're willing to serve.
pub fn caa_front_url_largest(mbid: &str) -> String {
    caa_front_url(mbid, CAA_SIZES[0])
}

/// Rewrite a CAA front URL to a different size, leaving non-CAA URLs untouched.
pub fn caa_front_url_with_size(url: &str, size: u16) -> Option<String> {
    let (base, current) = url.rsplit_once("/front-")?;
    current.parse::<u16>().ok()?;
    Some(format!("{base}/front-{size}"))
}

// Last.fm serves every image at a fixed set of sizes under `/i/u/<size>/<hash>`, and the
// size its api hands back (`300x300` for the largest entry of an `image` array) is not
// always one that was actually generated — the cdn answers those with a 404 html page,
// the same thing Telegram chokes on. So Last.fm urls get a step-down list too, preferring
// `500x500` and going smaller before the largest (`770x0`) as a last resort. Every size
// stays far below Telegram's 10MB limit for photos by url.
pub const LASTFM_IMAGE_SIZES: [&str; 6] = ["500x500", "300x300", "174s", "64s", "34s", "770x0"];

/// Rewrite a Last.fm image URL to a different size, leaving other URLs untouched.
pub fn lastfm_image_url_with_size(url: &str, size: &str) -> Option<String> {
    let (base, path) = url.split_once("/i/u/")?;
    let (current, file) = path.split_once('/')?;
    if current.is_empty() || file.contains('/') {
        return None;
    }
    Some(format!("{base}/i/u/{size}/{file}"))
}

/// The urls worth trying for one cover art image, largest first. Hosts we know no size
/// variants for yield the single url they came with.
pub fn cover_art_candidates(url: &str) -> Vec<String> {
    if caa_front_url_with_size(url, CAA_SIZES[0]).is_some() {
        CAA_SIZES
            .iter()
            .filter_map(|&size| caa_front_url_with_size(url, size))
            .collect()
    } else if lastfm_image_url_with_size(url, LASTFM_IMAGE_SIZES[0]).is_some() {
        LASTFM_IMAGE_SIZES
            .iter()
            .filter_map(|&size| lastfm_image_url_with_size(url, size))
            .collect()
    } else {
        vec![url.to_string()]
    }
}

// Confirming a cover art url costs a round trip per candidate. The confirmation is kept
// in a persistent cache (users.sqlite, 6h TTL, see db.rs) so repeated statuses — and
// restarts — skip the probing entirely. Entries are keyed on the *canonical* image
// (CAA release mbid or the Last.fm image hash), so a hit works no matter which size
// variant the API happened to hand back.
fn cover_art_cache_key(url: &str) -> String {
    if let Some(mbid) = url
        .split_once("coverartarchive.org/release/")
        .and_then(|(_, rest)| rest.split('/').next())
    {
        return format!("caa:{mbid}");
    }
    if let Some(file) = lastfm_image_url_with_size(url, "x").and_then(|u| {
        u.rsplit('/').next().map(str::to_string)
    }) {
        return format!("lf:{file}");
    }
    url.to_string()
}

enum ProbeOutcome {
    /// A candidate actually serves an image.
    Image(String),
    /// The host answered definitively that this size has no art (404, or 2xx error page).
    NoArt,
    /// Network error/timeout/5xx - transient, must not be treated as "no art".
    Unknown,
}

async fn probe_one(candidate: &str) -> ProbeOutcome {
    let Ok(response) = CLIENT_PROBE.head(candidate).send().await else {
        return ProbeOutcome::Unknown;
    };
    let status = response.status();
    let is_image = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| content_type.starts_with("image/"));

    if status.is_success() && is_image {
        ProbeOutcome::Image(candidate.to_string())
    } else if status.is_server_error() {
        ProbeOutcome::Unknown
    } else {
        // 4xx, or a 2xx that isn't an image (CAA answers missing art with a 404 html page
        // after its redirect; some hosts answer 200 with an error page) - both definitive.
        ProbeOutcome::NoArt
    }
}

/// Probe every candidate size at once - a miss then costs one round trip instead of one
/// per size - and return the first (most preferred) size that actually serves an image.
/// `NoArt` means every candidate answered definitively; `Unknown` means at least one
/// candidate failed transiently, in which case the caller must NOT cache a negative.
async fn probe_cover_art(url: &str) -> ProbeOutcome {
    let candidates = cover_art_candidates(url);

    let mut handles = Vec::with_capacity(candidates.len());
    for candidate in &candidates {
        let candidate = candidate.clone();
        handles.push(tokio::task::spawn(async move {
            (candidate.clone(), probe_one(&candidate).await)
        }));
    }

    let mut best: Option<(usize, String)> = None;
    let mut saw_unknown = false;
    for (index, handle) in handles.into_iter().enumerate() {
        if let Ok((_, outcome)) = handle.await {
            match outcome {
                ProbeOutcome::Image(found) => {
                    if best.is_none() || index < best.as_ref().unwrap().0 {
                        best = Some((index, found));
                    }
                }
                ProbeOutcome::Unknown => saw_unknown = true,
                ProbeOutcome::NoArt => {}
            }
        } else {
            saw_unknown = true;
        }
    }

    match best {
        Some((_, found)) => ProbeOutcome::Image(found),
        None if saw_unknown => ProbeOutcome::Unknown,
        None => ProbeOutcome::NoArt,
    }
}

/// First artist of a multi-artist credit ("A, B, C" -> "A"), else the whole name.
pub fn leading_artist(artist: &str) -> &str {
    artist
        .split(',')
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(artist)
}

/// Resolve cover art for a track from Last.fm's track/album art (the primary url is
/// resolved by the caller). Last.fm often only serves an album's image under the *leading*
/// artist of a multi-artist credit, so the album lookup retries with that artist when the
/// full credit returns nothing usable.
pub async fn resolve_cover_art_fallback(
    username: &str,
    api_type: &ApiType,
    track: &str,
    artist: &str,
    album: Option<&str>,
) -> Option<String> {
    if api_type == &ApiType::Librefm {
        return None;
    }

    // track.getInfo often fails (or comes back art-less) for multi-artist credits, so it
    // must not gate the album lookup — the album path below is what usually finds art.
    let track_info = fetch_lastfm_track(
        Some(username.to_string()),
        artist.to_string(),
        track.to_string(),
    )
    .await
    .ok();
    let mut candidate = track_info.as_ref().and_then(|t| t.album_art_url.clone());

    if candidate.is_none() && let Some(album) = album {
        if let Ok(album_info) = fetch_lastfm_album(username, artist, album).await {
            candidate = album_info.album_art_url;
        }
        if candidate.is_none() {
            let first = leading_artist(artist);
            if first != artist {
                candidate = fetch_lastfm_album(username, first, album)
                    .await
                    .ok()
                    .and_then(|a| a.album_art_url);
            }
        }
    }

    resolve_cover_art_url(candidate.as_deref()).await
}

/// Resolve Cover Art Archive art from the release's JSON metadata instead of HEAD-probing
/// the (flaky) archive.org redirect chain. The JSON is served straight by CAA and lists
/// the front thumbnail URLs directly.
async fn resolve_caa_json(mbid: &str) -> ProbeOutcome {
    let url = format!("https://coverartarchive.org/release/{mbid}");
    let Ok(response) = CLIENT_PROBE.get(&url).send().await else {
        return ProbeOutcome::Unknown;
    };
    let status = response.status();
    if status.is_server_error() {
        return ProbeOutcome::Unknown;
    }
    if !status.is_success() {
        // 404 etc - definitively no art.
        return ProbeOutcome::NoArt;
    }
    let Ok(json) = response.json::<serde_json::Value>().await else {
        return ProbeOutcome::Unknown;
    };

    let front = json["images"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|img| img["front"].as_bool() == Some(true));

    if let Some(front) = front {
        for size in ["500", "250", "1200"] {
            if let Some(u) = front["thumbnails"][size].as_str() {
                return ProbeOutcome::Image(u.to_string());
            }
        }
        if let Some(u) = front["image"].as_str() {
            return ProbeOutcome::Image(u.to_string());
        }
    }
    ProbeOutcome::NoArt
}

/// Confirm a cover art url really serves an image, returning the preferred size that does.
///
/// ListenBrainz gives us a release mbid even when the Cover Art Archive holds no art for
/// it, and CAA answers those with a 404 *html* page. Handing that to Telegram fails the
/// send with "wrong type of the web page content", so the url has to be checked here
/// rather than trusted. Returning None lets the caller fall back to the placeholder.
/// Results are cached in users.sqlite for 6 hours.
pub async fn resolve_cover_art_url(url: Option<&str>) -> Option<String> {
    let url = url?;
    let key = cover_art_cache_key(url);

    let cached = crate::db::DB.lock().unwrap().get_cover_art(&key);
    match cached {
        Some(Some(resolved)) => return Some(resolved),
        Some(None) => return None,
        None => {}
    }

    let outcome = if let Some(mbid) = url
        .split_once("coverartarchive.org/release/")
        .and_then(|(_, rest)| rest.split('/').next())
    {
        resolve_caa_json(mbid).await
    } else {
        probe_cover_art(url).await
    };

    match outcome {
        ProbeOutcome::Image(resolved) => {
            crate::db::DB
                .lock()
                .unwrap()
                .store_cover_art(&key, Some(&resolved))
                .ok();
            Some(resolved)
        }
        // Only a definitive miss gets negative-cached; a transient failure (timeout/5xx)
        // is left uncached so the next status re-probes instead of hiding art for 6h.
        ProbeOutcome::NoArt => {
            crate::db::DB
                .lock()
                .unwrap()
                .store_cover_art(&key, None)
                .ok();
            None
        }
        ProbeOutcome::Unknown => None,
    }
}

// The image bytes behind a resolved cover url, cached in memory. Keyed on the resolved
// url, 6h TTL. Warming these in the background lets a cover click upload bytes straight
// to Telegram instead of making Telegram fetch the image from the (often slow) CDN.
static COVER_ART_BYTES_CACHE: LazyLock<moka::future::Cache<String, bytes::Bytes>> =
    LazyLock::new(|| {
        moka::future::Cache::builder()
            .max_capacity(256)
            .time_to_live(Duration::from_secs(6 * 60 * 60))
            .build()
    });

/// The image bytes for a cover art url, or None when they can't be fetched. The caller is
/// expected to pass a *resolved* url; a cache hit returns instantly, a miss downloads
/// once and warms the cache. Oversized responses are rejected.
pub async fn cover_art_bytes(url: Option<&str>) -> Option<bytes::Bytes> {
    let url = url?;
    if let Some(bytes) = COVER_ART_BYTES_CACHE.get(url).await {
        return Some(bytes);
    }

    let Ok(response) = CLIENT_NOCACHE.get(url).send().await else {
        return None;
    };
    let bytes = response.bytes().await.ok()?;
    if bytes.is_empty() || bytes.len() > 10 * 1024 * 1024 {
        return None;
    }
    COVER_ART_BYTES_CACHE.insert(url.to_string(), bytes.clone()).await;
    Some(bytes)
}

fn get_base_url(api_type: &ApiType) -> &'static str {
    match api_type {
        ApiType::Lastfm => "https://ws.audioscrobbler.com/2.0/",
        ApiType::Librefm => "https://libre.fm/2.0/",
        ApiType::Listenbrainz => "https://api.listenbrainz.org/1/",
    }
}

fn get_biggest_lastfm_image(json_value: &serde_json::Value) -> Option<String> {
    // Sizes come smallest first, so the last one that actually carries a url is the
    // biggest. Last.fm sometimes leaves the largest sizes blank, hence the rev().
    let url = json_value["image"]
        .as_array()
        .and_then(|images| {
            images
                .iter()
                .rev()
                .filter_map(|image| image["#text"].as_str())
                .find(|text| !text.is_empty())
                .map(|text| text.to_string())
        })
        .unwrap_or_default();

    if url.is_empty() || url.contains("2a96cbd8b46e442fc41c2b86b821562f") {
        None
    } else {
        Some(url)
    }
}

pub async fn fetch_lastfm_track(
    username: Option<String>,
    artist: String,
    track: String,
) -> Result<Track, Box<dyn Error + Send + Sync>> {
    let base_url = get_base_url(&ApiType::Lastfm);
    let url = Url::parse_with_params(
        base_url,
        &[
            ("method", "track.getInfo"),
            ("track", track.as_str()),
            ("artist", artist.as_str()),
            ("user", username.unwrap_or_default().as_str()),
            ("api_key", config::LASTFM_API_KEY),
            ("format", "json"),
        ],
    );

    let t0 = std::time::Instant::now();
    let response = CLIENT.get(url?).send().await?;

    let json = response.json::<serde_json::Value>().await?;
    log::debug!("api: track.getInfo took {:?}", t0.elapsed());
    let track_json = json["track"].as_object();
    if track_json.is_none() {
        return Err(Box::from("Track not found."));
    }
    let track_json = track_json.unwrap();
    let name = track_json["name"].as_str().unwrap_or_default().to_string();
    let album_obj = track_json.get("album");
    let album = if let Some(album_obj) = album_obj {
        let x = album_obj["title"].as_str().unwrap_or_default();
        (!x.is_empty()).then_some(x.to_string())
    } else {
        None
    };
    let album_art_url = if let Some(album_obj) = album_obj {
        get_biggest_lastfm_image(album_obj)
    } else {
        None
    };
    let artist = track_json["artist"]["name"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let listeners = track_json["listeners"]
        .as_str()
        .unwrap_or_default()
        .parse::<u64>()
        .unwrap_or_default();
    let playcount = track_json["playcount"]
        .as_str()
        .unwrap_or_default()
        .parse::<u64>()
        .unwrap_or_default();
    let duration = track_json["duration"]
        .as_str()
        .unwrap_or_default()
        .parse::<u64>()
        .unwrap_or_default();
    let user_playcount_obj = track_json.get("userplaycount");
    let user_playcount = if let Some(user_playcount_obj) = user_playcount_obj {
        user_playcount_obj
            .as_str()
            .unwrap_or_default()
            .parse::<u64>()
            .unwrap_or_default()
    } else {
        0
    };

    let user_loved = track_json
        .get("userloved")
        .map(|x| x.as_str().unwrap_or_default() == "1")
        .unwrap_or_default();
    let tags = track_json["toptags"].get("tag").map(|x| {
        x.as_array()
            .into_iter()
            .flatten()
            .map(|x| x["name"].as_str().unwrap_or_default().to_string())
            .filter(|x| !x.is_empty())
            .collect::<Vec<_>>()
    });

    Ok(Track {
        name,
        album,
        artist,
        listeners,
        playcount,
        user_playcount,
        user_loved,
        duration,
        album_art_url,
        date: None,
        now_playing: false,
        tags,
        recording_mbid: None,
        release_mbid: None,
        release_group_mbid: None,
    })
}

pub async fn fetch_listenbrainz_track_playcount(
    username: &str,
    _artist: &str,
    _track: &str,
    recording_mbid: Option<&str>,
) -> Result<u64, Box<dyn Error + Send + Sync>> {
    // The stats endpoint only covers the user's top-1000 recordings, so any track outside
    // that window reports 0. Count the real listens instead: page through /listens filtered
    // by recording mbid. Tracks under 100 plays cost a single request.
    let Some(recording_mbid) = recording_mbid else {
        return Ok(0);
    };

    let t0 = std::time::Instant::now();
    let mut total: u64 = 0;
    let mut max_ts: Option<i64> = None;

    loop {
        let base = format!(
            "https://api.listenbrainz.org/1/user/{username}/listens?count=100&recording_mbid={recording_mbid}"
        );
        let url = match max_ts {
            Some(ts) => format!("{base}&max_ts={ts}"),
            None => base,
        };
        let response = CLIENT.get(&url).send().await?;
        let json = response.json::<serde_json::Value>().await?;
        let listens = json["payload"]["listens"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let n = listens.len();
        total += n as u64;

        if n < 100 {
            break;
        }
        // Page backwards past the oldest listen in this page.
        max_ts = listens
            .last()
            .and_then(|l| l["listened_at"].as_i64())
            .map(|t| t - 1);
    }

    log::debug!("api: LB playcount took {:?} (count={total})", t0.elapsed());
    Ok(total)
}

pub async fn fetch_lastfm_album(
    username: &str,
    artist: &str,
    album: &str,
) -> Result<Album, Box<dyn Error + Send + Sync>> {
    let base_url = get_base_url(&ApiType::Lastfm);
    let url = Url::parse_with_params(
        base_url,
        &[
            ("method", "album.getInfo"),
            ("album", album),
            ("artist", artist),
            ("user", username),
            ("api_key", config::LASTFM_API_KEY),
            ("format", "json"),
        ],
    );
    let t0 = std::time::Instant::now();
    let response = CLIENT.get(url?).send().await?;

    let json = response.json::<serde_json::Value>().await?;
    log::debug!("api: album.getInfo took {:?}", t0.elapsed());
    let album_json = json["album"].as_object();
    if album_json.is_none() {
        return Err(Box::from("Album not found."));
    }
    let album_json = album_json.unwrap();
    let name = album_json["name"].as_str().unwrap_or_default().to_string();
    let artist = album_json["artist"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let listeners = album_json["listeners"]
        .as_str()
        .unwrap_or_default()
        .parse::<u64>()
        .unwrap_or_default();
    let playcount = album_json["playcount"]
        .as_str()
        .unwrap_or_default()
        .parse::<u64>()
        .unwrap_or_default();
    let user_playcount_obj = album_json.get("userplaycount");
    let user_playcount = if let Some(user_playcount_obj) = user_playcount_obj {
        user_playcount_obj
            .as_str()
            .unwrap_or_default()
            .parse::<u64>()
            .unwrap_or_default()
    } else {
        0
    };
    let tags = album_json["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| x["name"].as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();

    Ok(Album {
        name,
        artist,
        listeners,
        playcount,
        user_playcount,
        album_art_url: get_biggest_lastfm_image(&json["album"]),
        tags: Some(tags),
    })
}


pub fn parse_listenbrainz_tracks(
    json_arr: &Value,
) -> Result<Vec<Track>, Box<dyn Error + Send + Sync>> {
    parse_listenbrainz_tracks_np(json_arr, false)
}
pub fn parse_listenbrainz_tracks_np(
    json_arr: &Value,
    now_playing: bool,
) -> Result<Vec<Track>, Box<dyn Error + Send + Sync>> {
    let tracks = json_arr
        .as_array()
        .into_iter()
        .flatten()
        .map(|track_json| {
            let track_metadata = if let Some(m) = track_json.get("track_metadata") {
                m
            } else {
                track_json
            };

            let artist = track_metadata["artist_name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let album = track_metadata["release_name"]
                .as_str()
                .map(|s| s.to_string());
            let name = track_metadata["track_name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let album_art_url = track_metadata["mbid_mapping"]["caa_release_mbid"]
                .as_str()
                .or_else(|| track_metadata["mbid_mapping"]["release_mbid"].as_str())
                .or_else(|| track_metadata["additional_info"]["release_mbid"].as_str())
                .or_else(|| track_metadata["release_mbid"].as_str())
                .map(caa_front_url_largest);
            let recording_mbid = track_metadata["mbid_mapping"]["recording_mbid"]
                .as_str()
                .or_else(|| track_metadata["additional_info"]["recording_mbid"].as_str())
                .or_else(|| track_metadata["recording_mbid"].as_str())
                .map(|s| s.to_string());
            let release_mbid = track_metadata["mbid_mapping"]["release_mbid"]
                .as_str()
                .or_else(|| track_metadata["additional_info"]["release_mbid"].as_str())
                .or_else(|| track_metadata["release_mbid"].as_str())
                .map(|s| s.to_string());
            let release_group_mbid = track_metadata["additional_info"]["release_group_mbid"]
                .as_str()
                .map(|s| s.to_string());
            let user_playcount = track_metadata["listen_count"].as_u64().unwrap_or_default();
            let date = track_json["listened_at"].as_u64();

            Track {
                name,
                album,
                artist,
                album_art_url,
                date,
                user_loved: false,
                duration: 0,
                listeners: 0,
                playcount: 0,
                user_playcount,
                now_playing,
                tags: None,
                recording_mbid,
                release_mbid,
                release_group_mbid,
            }
        })
        .collect::<Vec<_>>();

    Ok(tracks)
}

pub fn parse_lastfm_tracks(json_arr: &Value) -> Result<Vec<Track>, Box<dyn Error + Send + Sync>> {
    let tracks = json_arr
        .as_array()
        .into_iter()
        .flatten()
        .map(|track_json| {
            let artist_obj = &track_json["artist"];
            let artist = if let Some(artist_name) = artist_obj.get("#text") {
                artist_name.as_str().unwrap_or_default()
            } else if let Some(artist_name) = artist_obj.get("name") {
                artist_name.as_str().unwrap_or_default()
            } else {
                ""
            };

            let album_obj = track_json["album"].as_object();

            let album = if let Some(album_obj) = album_obj {
                let x = album_obj["#text"].as_str().unwrap_or_default();
                (!x.is_empty()).then_some(x.to_string())
            } else {
                None
            };

            let name = track_json["name"].as_str().unwrap_or_default().to_string();
            let album_art_url = get_biggest_lastfm_image(track_json);
            let date = track_json["date"]["uts"]
                .as_str()
                .unwrap_or_default()
                .parse::<u64>()
                .ok();
            let user_loved = track_json["loved"].as_str().unwrap_or_default() == "1";
            let now_playing = track_json["@attr"]
                .get("nowplaying")
                .map(|x| x.as_str().unwrap_or_default())
                .unwrap_or_default()
                == "true";

            Track {
                name,
                album,
                artist: artist.into(),
                album_art_url,
                date,
                user_loved,
                duration: 0,
                listeners: 0,
                playcount: 0,
                user_playcount: 0,
                now_playing,
                tags: None,
                recording_mbid: None,
                release_mbid: None,
                release_group_mbid: None,
            }
        })
        .collect::<Vec<_>>();

    Ok(tracks)
}

async fn fetch_recent_tracks_listenbrainz(
    username: &str,
    cache_control: &str,
    actual_limit: usize,
) -> Result<Vec<Track>, BoxError> {
    let base_url = get_base_url(&ApiType::Listenbrainz);

    let url = format!("{base_url}user/{username}/playing-now");
    let response = CLIENT
        .get(&url)
        .header("cache-control", cache_control)
        .send()
        .await?;

    let json = response.json::<serde_json::Value>().await?;

    let mut all_tracks = parse_listenbrainz_tracks_np(&json["payload"]["listens"], true)?;

    if !all_tracks.is_empty() && actual_limit == 1 {
        return Ok(all_tracks);
    }

    let url = format!("{base_url}user/{username}/listens?count=3");
    let response = CLIENT
        .get(&url)
        .header("cache-control", cache_control)
        .send()
        .await?;
    let json = response.json::<serde_json::Value>().await?;

    let tracks = parse_listenbrainz_tracks(&json["payload"]["listens"])?;

    all_tracks.extend(tracks);
    Ok(all_tracks)
}

async fn fetch_recent_tracks_lastfm(
    username: &str,
    api_type: &ApiType,
    cache_control: &str,
) -> Result<Vec<Track>, BoxError> {
    let url = Url::parse_with_params(
        get_base_url(api_type),
        &[
            ("method", "user.getrecenttracks"),
            ("user", username),
            ("extended", "1"),
            ("limit", "3"),
            ("api_key", config::LASTFM_API_KEY),
            ("format", "json"),
        ],
    )?;

    let response = CLIENT
        .get(url)
        .header("cache-control", cache_control)
        .send()
        .await?;

    let json = response.json::<serde_json::Value>().await?;
    let err = json["error"]
        .as_object()
        .map(|x| x["#text"].as_str().unwrap_or_default());
    if let Some(err) = err
        && !err.is_empty()
    {
        return Err(Box::from(err));
    }

    parse_lastfm_tracks(&json["recenttracks"]["track"])
}

// Get recent tracks for a given user
pub async fn fetch_recent_tracks(
    username: &str,
    api_type: &ApiType,
    prefer_cached: bool,
    actual_limit: usize,
) -> Result<Vec<Track>, BoxError> {
    let cache_control = if prefer_cached {
        "max-stale=300"
    } else {
        "no-cache, must-revalidate"
    };

    match api_type {
        ApiType::Listenbrainz => or_lastfm(
            fetch_recent_tracks_listenbrainz(username, cache_control, actual_limit),
            fetch_recent_tracks_lastfm(username, &ApiType::Lastfm, cache_control),
            |tracks| !tracks.is_empty(),
        )
        .await,

        ApiType::Librefm | ApiType::Lastfm => {
            fetch_recent_tracks_lastfm(username, api_type, cache_control).await
        }
    }
}

async fn fetch_loved_tracks_listenbrainz(username: &str) -> Result<Vec<Track>, BoxError> {
    let base_url = get_base_url(&ApiType::Listenbrainz);
    let url = format!("{base_url}user/{username}/get-feedback?metadata=true&count=5");

    let response = CLIENT.get(&url).send().await?;
    let json = response.json::<serde_json::Value>().await?;

    parse_listenbrainz_tracks(&json["feedback"])
}

async fn fetch_loved_tracks_lastfm(
    username: &str,
    api_type: &ApiType,
) -> Result<Vec<Track>, BoxError> {
    let url = Url::parse_with_params(
        get_base_url(api_type),
        &[
            ("method", "user.getlovedtracks"),
            ("user", username),
            ("limit", "5"),
            ("api_key", config::LASTFM_API_KEY),
            ("format", "json"),
        ],
    )?;

    let response = CLIENT.get(url).send().await?;
    let json = response.json::<serde_json::Value>().await?;

    parse_lastfm_tracks(&json["lovedtracks"]["track"])
}

// Get loved tracks for a given user
pub async fn fetch_loved_tracks(
    username: &str,
    api_type: &ApiType,
) -> Result<Vec<Track>, BoxError> {
    match api_type {
        ApiType::Listenbrainz => or_lastfm(
            fetch_loved_tracks_listenbrainz(username),
            fetch_loved_tracks_lastfm(username, &ApiType::Lastfm),
            |tracks| !tracks.is_empty(),
        )
        .await,

        ApiType::Librefm | ApiType::Lastfm => fetch_loved_tracks_lastfm(username, api_type).await,
    }
}

fn time_period_to_api_string<'a>(duration: &'a TimePeriod, api_type: &'a ApiType) -> &'a str {
    match api_type {
        ApiType::Lastfm | ApiType::Librefm => match duration {
            TimePeriod::OneWeek => "7day",
            TimePeriod::OneMonth => "1month",
            TimePeriod::ThreeMonths => "3month",
            TimePeriod::SixMonths => "6month",
            TimePeriod::OneYear => "12month",
            TimePeriod::AllTime => "overall",
        },
        ApiType::Listenbrainz => match duration {
            TimePeriod::OneWeek => "week",
            TimePeriod::OneMonth => "month",
            TimePeriod::ThreeMonths => "quarter",
            TimePeriod::SixMonths => "half_yearly",
            TimePeriod::OneYear => "year",
            TimePeriod::AllTime => "all_time",
        },
    }
}

async fn fetch_albums_listenbrainz(
    username: &str,
    duration: &TimePeriod,
    limit: Option<usize>,
) -> Result<Vec<Album>, BoxError> {
    let base_url = get_base_url(&ApiType::Listenbrainz);
    let duration_str = time_period_to_api_string(duration, &ApiType::Listenbrainz);

    let url = format!(
        "{}stats/user/{}/releases?range={}&count={}",
        base_url,
        username,
        duration_str,
        limit.unwrap_or(100)
    );
    let response = CLIENT.get(&url).send().await?;

    let json = response.json::<serde_json::Value>().await?;

    let albums = json["payload"]["releases"]
        .as_array()
        .ok_or("Invalid JSON format: 'payload.releases' is not an array")
        .into_iter()
        .flatten()
        .map(|album_json| {
            let artist = album_json["artist_name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let name = album_json["release_name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            // `caa_release_mbid` is the release CAA actually holds art for; it
            // differs from `release_mbid` when art lives on another release in
            // the group, so prefer it and only fall back to the plain mbid.
            let album_art_url = album_json["caa_release_mbid"]
                .as_str()
                .or_else(|| album_json["release_mbid"].as_str())
                .map(caa_front_url_largest);
            let user_playcount = album_json["listen_count"].as_u64().unwrap_or_default();

            Album {
                name,
                artist,
                album_art_url,
                listeners: 0,
                playcount: 0,
                user_playcount,
                tags: None,
            }
        })
        .collect::<Vec<_>>();

    Ok(albums)
}

async fn fetch_albums_lastfm(
    username: &str,
    duration: &TimePeriod,
    api_type: &ApiType,
    limit: Option<usize>,
) -> Result<Vec<Album>, BoxError> {
    let url = Url::parse_with_params(
        get_base_url(api_type),
        &[
            ("method", "user.gettopalbums"),
            ("period", time_period_to_api_string(duration, api_type)),
            ("user", username),
            ("limit", &limit.unwrap_or(200).to_string()),
            ("api_key", config::LASTFM_API_KEY),
            ("format", "json"),
        ],
    )?;
    let response = CLIENT.get(url).send().await?;
    let json = response.json::<serde_json::Value>().await?;

    let albums = json["topalbums"]["album"]
        .as_array()
        .ok_or("Invalid JSON format: 'topalbums.album' is not an array")
        .into_iter()
        .flatten()
        .map(|album_json| {
            let artist = album_json["artist"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let name = album_json["name"].as_str().unwrap_or_default().to_string();
            let album_art_url = get_biggest_lastfm_image(album_json);
            let user_playcount = album_json["playcount"]
                .as_str()
                .unwrap_or_default()
                .parse::<u64>()
                .unwrap_or_default();

            Album {
                name,
                artist,
                album_art_url,
                listeners: 0,
                playcount: 0,
                user_playcount,
                tags: None,
            }
        })
        .collect::<Vec<_>>();

    Ok(albums)
}

// Get albums for a given user
pub async fn fetch_albums(
    username: &str,
    duration: &TimePeriod,
    api_type: &ApiType,
    limit: Option<usize>,
) -> Result<Vec<Album>, BoxError> {
    match api_type {
        ApiType::Listenbrainz => or_lastfm(
            fetch_albums_listenbrainz(username, duration, limit),
            fetch_albums_lastfm(username, duration, &ApiType::Lastfm, None),
            |albums| !albums.is_empty(),
        )
        .await,

        ApiType::Librefm | ApiType::Lastfm => {
            fetch_albums_lastfm(username, duration, api_type, limit).await
        }
    }
}

async fn fetch_artists_listenbrainz(
    username: &str,
    duration: &TimePeriod,
    limit: Option<usize>,
) -> Result<Vec<Artist>, BoxError> {
    let url = format!(
        "{}stats/user/{}/artists?range={}&count={}",
        get_base_url(&ApiType::Listenbrainz),
        username,
        time_period_to_api_string(duration, &ApiType::Listenbrainz),
        limit.unwrap_or(100)
    );
    let response = CLIENT.get(&url).send().await?;

    let json = response.json::<serde_json::Value>().await?;

    let artists = json["payload"]["artists"]
        .as_array()
        .ok_or("Invalid JSON format: 'payload.artists' is not an array")
        .into_iter()
        .flatten()
        .map(|artists_json| {
            let name = artists_json["artist_name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let user_playcount = artists_json["listen_count"].as_u64().unwrap_or_default();

            Artist {
                name,
                listeners: 0,
                playcount: 0,
                user_playcount,
                tags: None,
            }
        })
        .collect::<Vec<_>>();
    Ok(artists)
}

async fn fetch_artists_lastfm(
    username: &str,
    duration: &TimePeriod,
    api_type: &ApiType,
    limit: Option<usize>,
) -> Result<Vec<Artist>, BoxError> {
    let url = Url::parse_with_params(
        get_base_url(api_type),
        &[
            ("method", "user.gettopartists"),
            ("period", time_period_to_api_string(duration, api_type)),
            ("user", username),
            ("limit", &limit.unwrap_or(200).to_string()),
            ("api_key", config::LASTFM_API_KEY),
            ("format", "json"),
        ],
    )?;
    let response = CLIENT.get(url).send().await?;
    let json = response.json::<serde_json::Value>().await?;

    let artists = json["topartists"]["artist"]
        .as_array()
        .ok_or("Invalid JSON format: 'topartists.artist' is not an array")
        .into_iter()
        .flatten()
        .map(|artist_json| {
            let name = artist_json["name"].as_str().unwrap_or_default().to_string();
            let user_playcount = artist_json["playcount"]
                .as_str()
                .unwrap_or_default()
                .parse::<u64>()
                .unwrap_or_default();

            Artist {
                name,
                listeners: 0,
                playcount: 0,
                user_playcount,
                tags: None,
            }
        })
        .collect::<Vec<_>>();

    Ok(artists)
}

// Get artists for a given user
pub async fn fetch_artists(
    username: &str,
    duration: &TimePeriod,
    api_type: &ApiType,
    limit: Option<usize>,
) -> Result<Vec<Artist>, BoxError> {
    match api_type {
        ApiType::Listenbrainz => or_lastfm(
            fetch_artists_listenbrainz(username, duration, limit),
            fetch_artists_lastfm(username, duration, &ApiType::Lastfm, None),
            |artists| !artists.is_empty(),
        )
        .await,

        ApiType::Librefm | ApiType::Lastfm => {
            fetch_artists_lastfm(username, duration, api_type, limit).await
        }
    }
}

async fn fetch_tracks_listenbrainz(
    username: &str,
    duration: &TimePeriod,
    limit: Option<usize>,
) -> Result<Vec<Track>, BoxError> {
    let url = format!(
        "{}stats/user/{}/recordings?range={}&count={}",
        get_base_url(&ApiType::Listenbrainz),
        username,
        time_period_to_api_string(duration, &ApiType::Listenbrainz),
        limit.unwrap_or(100)
    );
    let response = CLIENT.get(&url).send().await?;

    let json = response.json::<serde_json::Value>().await?;

    parse_listenbrainz_tracks(&json["payload"]["recordings"])
}

async fn fetch_tracks_lastfm(
    username: &str,
    duration: &TimePeriod,
    api_type: &ApiType,
    limit: Option<usize>,
) -> Result<Vec<Track>, BoxError> {
    let url = Url::parse_with_params(
        get_base_url(api_type),
        &[
            ("method", "user.gettoptracks"),
            ("period", time_period_to_api_string(duration, api_type)),
            ("user", username),
            ("limit", &limit.unwrap_or(200).to_string()),
            ("api_key", config::LASTFM_API_KEY),
            ("format", "json"),
        ],
    )?;
    let response = CLIENT.get(url).send().await?;
    let json = response.json::<serde_json::Value>().await?;

    let tracks = json["toptracks"]["track"]
        .as_array()
        .ok_or("Invalid JSON format: 'toptracks.track' is not an array")
        .into_iter()
        .flatten()
        .map(|track_json| {
            let name = track_json["name"].as_str().unwrap_or_default().to_string();
            let user_playcount = track_json["playcount"]
                .as_str()
                .unwrap_or_default()
                .parse::<u64>()
                .unwrap_or_default();
            let artist = track_json["artist"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string();

            Track {
                name,
                album: None,
                artist,
                album_art_url: None,
                date: None,
                duration: 0,
                listeners: 0,
                playcount: 0,
                user_playcount,
                now_playing: false,
                user_loved: false,
                tags: None,
                recording_mbid: None,
                release_mbid: None,
                release_group_mbid: None,
            }
        })
        .collect::<Vec<_>>();

    Ok(tracks)
}

// Get tracks for a given user
pub async fn fetch_tracks(
    username: &str,
    duration: &TimePeriod,
    api_type: &ApiType,
    limit: Option<usize>,
) -> Result<Vec<Track>, BoxError> {
    match api_type {
        ApiType::Listenbrainz => or_lastfm(
            fetch_tracks_listenbrainz(username, duration, limit),
            fetch_tracks_lastfm(username, duration, &ApiType::Lastfm, None),
            |tracks| !tracks.is_empty(),
        )
        .await,

        ApiType::Librefm | ApiType::Lastfm => {
            fetch_tracks_lastfm(username, duration, api_type, limit).await
        }
    }
}

async fn fetch_user_info_listenbrainz(username: &str) -> Result<ScrobbleUser, BoxError> {
    let base_url = get_base_url(&ApiType::Listenbrainz);

    let url = format!("{base_url}user/{username}/listen-count");
    let response = CLIENT.get(&url).send().await?;
    let json = response.json::<serde_json::Value>().await?;
    let playcount = json["payload"]["count"].as_u64().unwrap_or_default();

    let url = format!("{base_url}stats/user/{username}/artists");
    let response = CLIENT.get(&url).send().await?;
    let json = response.json::<serde_json::Value>().await?;
    let artist_count = json["payload"]["total_artist_count"]
        .as_u64()
        .unwrap_or_default();

    let url = format!("{base_url}stats/user/{username}/releases");
    let response = CLIENT.get(&url).send().await?;
    let json = response.json::<serde_json::Value>().await?;
    let track_count = json["payload"]["total_release_count"]
        .as_u64()
        .unwrap_or_default();

    let url = format!("{base_url}stats/user/{username}/recordings");
    let response = CLIENT.get(&url).send().await?;
    let json = response.json::<serde_json::Value>().await?;
    let album_count = json["payload"]["total_recording_count"]
        .as_u64()
        .unwrap_or_default();

    Ok(ScrobbleUser {
        username: username.to_owned(),
        playcount,
        artist_count,
        track_count,
        album_count,
        profile_pic_url: None,
        registered_date: None,
    })
}

async fn fetch_user_info_lastfm(
    username: &str,
    api_type: &ApiType,
) -> Result<ScrobbleUser, BoxError> {
    let url = Url::parse_with_params(
        get_base_url(api_type),
        &[
            ("method", "user.getInfo"),
            ("user", username),
            ("api_key", config::LASTFM_API_KEY),
            ("format", "json"),
        ],
    )?;
    let response = CLIENT.get(url).send().await?;
    let json = response.json::<serde_json::Value>().await?;
    let user_json = &json["user"];
    let playcount = user_json["playcount"]
        .as_str()
        .unwrap_or_default()
        .parse::<u64>()
        .unwrap_or_default();
    let artist_count = user_json["artist_count"]
        .as_str()
        .unwrap_or_default()
        .parse::<u64>()
        .unwrap_or_default();
    let track_count = user_json["track_count"]
        .as_str()
        .unwrap_or_default()
        .parse::<u64>()
        .unwrap_or_default();
    let album_count = user_json["album_count"]
        .as_str()
        .unwrap_or_default()
        .parse::<u64>()
        .unwrap_or_default();
    let registered_date = if let Some(registered) = user_json["registered"].get("#text") {
        registered.as_u64()
    } else {
        None
    };
    let profile_pic_url = get_biggest_lastfm_image(user_json);

    Ok(ScrobbleUser {
        username: username.to_owned(),
        playcount,
        artist_count,
        track_count,
        album_count,
        profile_pic_url,
        registered_date,
    })
}

// Get info for a given user
pub async fn fetch_user_info(
    username: &str,
    api_type: &ApiType,
) -> Result<ScrobbleUser, BoxError> {
    match api_type {
        ApiType::Listenbrainz => or_lastfm(
            fetch_user_info_listenbrainz(username),
            fetch_user_info_lastfm(username, &ApiType::Lastfm),
            // A live ListenBrainz account always reports a listen count; an all-zero
            // profile means the stats endpoints returned nothing useful.
            |user| user.playcount > 0,
        )
        .await,

        ApiType::Librefm | ApiType::Lastfm => fetch_user_info_lastfm(username, api_type).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_url_uses_500() {
        assert_eq!(
            caa_front_url_largest("abc"),
            "https://coverartarchive.org/release/abc/front-500"
        );
    }

    #[test]
    fn sizes_prefer_500_first() {
        assert_eq!(CAA_SIZES[0], 500);
        assert_eq!(CAA_SIZES.len(), 3);
    }

    #[test]
    fn rewrites_size_of_caa_url() {
        let url = caa_front_url_largest("abc");
        assert_eq!(
            caa_front_url_with_size(&url, 250).as_deref(),
            Some("https://coverartarchive.org/release/abc/front-250")
        );
    }

    #[test]
    fn leaves_non_caa_urls_alone() {
        // Last.fm images have sizes of their own, swapped by their own helper.
        assert_eq!(
            caa_front_url_with_size("https://lastfm.freetls.fastly.net/i/u/300x300/abc.png", 250),
            None
        );
        // `/front` without a size suffix isn't a rewrite target either.
        assert_eq!(
            caa_front_url_with_size("https://coverartarchive.org/release/abc/front", 250),
            None
        );
        // A non-numeric suffix must not be treated as a size.
        assert_eq!(
            caa_front_url_with_size("https://coverartarchive.org/release/abc/front-large", 250),
            None
        );
    }

    #[test]
    fn candidates_prefer_500_for_caa() {
        let c = cover_art_candidates(&caa_front_url_largest("abc"));
        assert_eq!(
            c,
            vec![
                "https://coverartarchive.org/release/abc/front-500",
                "https://coverartarchive.org/release/abc/front-250",
                "https://coverartarchive.org/release/abc/front-1200",
            ]
        );
    }

    #[test]
    fn rewrites_size_of_lastfm_url() {
        assert_eq!(
            lastfm_image_url_with_size(
                "https://lastfm.freetls.fastly.net/i/u/300x300/abc.png",
                "770x0"
            )
            .as_deref(),
            Some("https://lastfm.freetls.fastly.net/i/u/770x0/abc.png")
        );
    }

    #[test]
    fn leaves_non_lastfm_urls_alone() {
        // A sizeless image url has nothing to swap...
        assert_eq!(
            lastfm_image_url_with_size("https://lastfm.freetls.fastly.net/i/u/abc.png", "770x0"),
            None
        );
        // ...and neither has a url from another host.
        assert_eq!(
            lastfm_image_url_with_size("https://coverartarchive.org/release/abc/front-1200", "34s"),
            None
        );
    }

    #[test]
    fn candidates_prefer_500_for_lastfm() {
        let c = cover_art_candidates("https://lastfm.freetls.fastly.net/i/u/300x300/abc.png");
        assert_eq!(c.len(), LASTFM_IMAGE_SIZES.len());
        assert_eq!(c[0], "https://lastfm.freetls.fastly.net/i/u/500x500/abc.png");
        assert_eq!(
            c.last().unwrap(),
            "https://lastfm.freetls.fastly.net/i/u/770x0/abc.png"
        );
    }

    #[test]
    fn candidates_for_unknown_host_are_just_the_url() {
        let url = "https://example.com/cover.png";
        assert_eq!(cover_art_candidates(url), vec![url]);
    }

    #[test]
    fn cache_key_is_canonical_for_caa() {
        assert_eq!(
            cover_art_cache_key("https://coverartarchive.org/release/abc/front-500"),
            "caa:abc"
        );
        assert_eq!(
            cover_art_cache_key("https://coverartarchive.org/release/abc/front-1200"),
            "caa:abc"
        );
    }

    #[test]
    fn cache_key_is_canonical_for_lastfm() {
        assert_eq!(
            cover_art_cache_key("https://lastfm.freetls.fastly.net/i/u/300x300/hash.png"),
            "lf:hash.png"
        );
        assert_eq!(
            cover_art_cache_key("https://lastfm.freetls.fastly.net/i/u/500x500/hash.png"),
            "lf:hash.png"
        );
    }

    #[test]
    fn cache_key_for_unknown_host_is_the_url() {
        let url = "https://example.com/cover.png";
        assert_eq!(cover_art_cache_key(url), url);
    }

    #[test]
    fn leading_artist_splits_multi_artist_credits() {
        assert_eq!(leading_artist("Ele A, NeS"), "Ele A");
        assert_eq!(leading_artist("Gmtn., kozato, Luze"), "Gmtn.");
        assert_eq!(leading_artist("Single Artist"), "Single Artist");
        assert_eq!(leading_artist("Massive New Krew"), "Massive New Krew");
    }

    // Network-gated: `cargo test -- --ignored`. These pin the exact behaviour that broke
    // the bot, so they are worth keeping even though they need the live CAA.
    #[tokio::test]
    #[ignore]
    async fn resolves_release_without_art_to_none() {
        // ListenBrainz reports these release mbids with no caa_release_mbid; CAA answers
        // with a 404 html page, which Telegram rejects as "wrong type of web page content".
        for mbid in [
            "b479acee-3cde-49af-83cc-2c8054d089e9",
            "d229f806-0639-4b8c-bce6-1d4f01824be3",
        ] {
            let url = caa_front_url_largest(mbid);
            assert_eq!(resolve_cover_art_url(Some(&url)).await, None, "{mbid}");
        }
    }

    #[tokio::test]
    #[ignore]
    async fn resolves_release_with_art_to_preferred_size() {
        let url = caa_front_url_largest("0d932a42-b3f5-419c-bc28-332d4a2b7f87");
        assert_eq!(resolve_cover_art_url(Some(&url)).await, Some(url));
    }

    #[tokio::test]
    #[ignore]
    async fn resolves_lastfm_url_whose_own_size_is_missing() {
        // Last.fm gives this album's cover out as 300x300, a size its cdn 404s; the art
        // itself is there at every other size.
        let hash = "2ae8513eff8778953057e10a00d45415.jpg";
        let url = format!("https://lastfm.freetls.fastly.net/i/u/300x300/{hash}");
        assert_eq!(
            resolve_cover_art_url(Some(&url)).await,
            Some(format!("https://lastfm.freetls.fastly.net/i/u/500x500/{hash}"))
        );
    }

    #[tokio::test]
    #[ignore]
    async fn resolves_none_to_none() {
        assert_eq!(resolve_cover_art_url(None).await, None);
    }
}
