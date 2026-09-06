use std::sync::LazyLock;
use std::time::Duration;

use reqwest::Url;
use serde_json::json;

use crate::{api_requester, config, db};

// Navidrome's native API authenticates via a JWT in the `X-ND-Authorization` header
// (the standard `Authorization` header is ignored by its auth middleware).
static X_ND_AUTHORIZATION: reqwest::header::HeaderName =
    reqwest::header::HeaderName::from_static("x-nd-authorization");

// Navidrome cover art source.
//
// The bot's tracks already carry a release-group mbid (ListenBrainz
// `additional_info.release_group_mbid`), and Navidrome albums are keyed on the
// MusicBrainz Album ID tag (release-group mbid). So: mbid -> album id via the
// native API, then the cover bytes via the Subsonic `getCoverArt` endpoint.
// Covers are served from Navidrome's local art cache, avoiding the (slow,
// often cold) Cover Art Archive round trip. Only users listed in
// `config::NAVIDROME_USERS` (matched by scrobbling username) use this source;
// everyone else keeps the CAA/Last.fm path.

static CLIENT_ND: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("LastFM Robot (Telegram bot)")
        .build()
        .unwrap()
});

// Navidrome JWT login token, in-memory only (never persisted). Navidrome tokens
// are valid for 48h by default; a 1h TTL keeps us fresh without re-logging in on
// every cover resolve.
static TOKEN_CACHE: LazyLock<moka::future::Cache<String, String>> = LazyLock::new(|| {
    moka::future::Cache::builder()
        .max_capacity(1)
        .time_to_live(Duration::from_secs(60 * 60))
        .build()
});

// Fixed salt so the getCoverArt URL is stable and cacheable (the subsonic token
// is md5(password + salt); it never expires, only the password matters).
const SUBSONIC_SALT: &str = "lastfmrobot-nd";

/// Whether Navidrome should be the cover art source for this scrobbling username.
pub fn enabled_for(username: &str) -> bool {
    !config::NAVIDROME_URL.is_empty()
        && config::NAVIDROME_USERS
            .iter()
            .any(|u| u.eq_ignore_ascii_case(username))
}

async fn login() -> Option<String> {
    if let Some(token) = TOKEN_CACHE.get("token").await {
        return Some(token);
    }

    let resp = match CLIENT_ND
        .post(format!("{}/auth/login", config::NAVIDROME_URL))
        .json(&json!({
            "username": config::NAVIDROME_USERNAME,
            "password": config::NAVIDROME_PASSWORD,
        }))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            log::warn!("navidrome: login request failed: {e}");
            return None;
        }
    };
    if !resp.status().is_success() {
        log::warn!("navidrome: login returned {}", resp.status());
        return None;
    }

    let token = match resp.json::<serde_json::Value>().await {
        Ok(json) => match json.get("token").and_then(|t| t.as_str()) {
            Some(token) => token.to_string(),
            None => {
                log::warn!("navidrome: login response missing token");
                return None;
            }
        },
        Err(e) => {
            log::warn!("navidrome: login json parse failed: {e}");
            return None;
        }
    };
    TOKEN_CACHE.insert("token".to_string(), token.clone()).await;
    Some(token)
}

enum Lookup {
    Found(String),
    NoAlbum,
    Unreachable,
}

/// Genres for a recording mbid from Navidrome's file tags (`/api/song` filtered
/// on `mbz_track_id` → `genres[].name`, falling back to the single `genre`
/// string). Keyed by recording mbid, so it serves any user, not just gated
/// ones. Hits (even genre-less songs) are cached in users.sqlite; a missing
/// song or unreachable server stays uncached and retries next status.
pub async fn fetch_track_genres(recording_mbid: &str) -> Option<Vec<String>> {
    let key = format!("nd:track:{recording_mbid}");
    if let Some(cached) = db::DB.lock().unwrap().get_mb_genres(&key) {
        return Some(cached);
    }
    let Some(token) = login().await else {
        return None;
    };

    let filters = format!("{{\"mbz_track_id\":\"{recording_mbid}\"}}");
    let mut url = match Url::parse(&format!("{}/api/song", config::NAVIDROME_URL)) {
        Ok(url) => url,
        Err(_) => return None,
    };
    url.query_pairs_mut()
        .append_pair("_filters", &filters)
        .append_pair("_end", "1");

    let resp = match CLIENT_ND
        .get(url)
        .header(X_ND_AUTHORIZATION.clone(), format!("Bearer {token}"))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            log::warn!("navidrome: song lookup ({recording_mbid}) failed: {e}");
            return None;
        }
    };
    if !resp.status().is_success() {
        log::warn!(
            "navidrome: song lookup ({recording_mbid}) returned {}",
            resp.status()
        );
        return None;
    }
    let json: serde_json::Value = match resp.json().await {
        Ok(json) => json,
        Err(e) => {
            log::warn!("navidrome: song lookup ({recording_mbid}) json parse failed: {e}");
            return None;
        }
    };

    let song = json.as_array().and_then(|a| a.first());
    let mut genres: Vec<String> = song
        .and_then(|s| s.get("genres"))
        .and_then(|g| g.as_array())
        .into_iter()
        .flatten()
        .filter_map(|g| g["name"].as_str().map(str::to_string))
        .filter(|g| !g.is_empty())
        .collect();
    if genres.is_empty()
        && let Some(genre) = song
            .and_then(|s| s.get("genre"))
            .and_then(|g| g.as_str())
            .map(str::trim)
            .filter(|g| !g.is_empty())
    {
        genres.push(genre.to_string());
    }
    // Song found (even genre-less) → cache; no song → leave uncached so a
    // later library rescan gets picked up.
    if song.is_some() {
        db::DB
            .lock()
            .unwrap()
            .store_mb_genres(&key, &genres)
            .ok();
        Some(genres)
    } else {
        log::debug!("navidrome: no song for mbz_track_id={recording_mbid}");
        None
    }
}

// Native API `GET /api/album` with a field filter returns the album(s) whose
// MusicBrainz id matches. Navidrome maps:
//   `mbz_album_id`            <- MusicBrainz Album Id tag  (a *release* mbid)
//   `mbz_release_group_id`    <- MusicBrainz Release Group Id tag
// ListenBrainz hands out both, so the caller picks the field matching the mbid
// it holds. `_end=1` keeps the first row only; album ids are persistent
// (PID-derived), so stable across restarts.
async fn lookup_album_id(filter_field: &str, mbid: &str) -> Lookup {
    let Some(token) = login().await else {
        return Lookup::Unreachable;
    };

    let filters = format!("{{\"{filter_field}\":\"{mbid}\"}}");
    let mut url = match Url::parse(&format!("{}/api/album", config::NAVIDROME_URL)) {
        Ok(url) => url,
        Err(_) => return Lookup::Unreachable,
    };
    url.query_pairs_mut()
        .append_pair("_filters", &filters)
        .append_pair("_end", "1");

    let resp = match CLIENT_ND
        .get(url)
        .header(X_ND_AUTHORIZATION.clone(), format!("Bearer {token}"))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            log::warn!("navidrome: album lookup ({filter_field}={mbid}) failed: {e}");
            return Lookup::Unreachable;
        }
    };
    if !resp.status().is_success() {
        log::warn!(
            "navidrome: album lookup ({filter_field}={mbid}) returned {}",
            resp.status()
        );
        return Lookup::Unreachable;
    }

    let json = match resp.json::<serde_json::Value>().await {
        Ok(json) => json,
        Err(e) => {
            log::warn!("navidrome: album lookup ({filter_field}) json parse failed: {e}");
            return Lookup::Unreachable;
        }
    };

    match json
        .as_array()
        .and_then(|a| a.first())
        .and_then(|a| a.get("id"))
        .and_then(|id| id.as_str())
    {
        Some(id) => Lookup::Found(id.to_string()),
        None => {
            log::debug!("navidrome: no album for {filter_field}={mbid}");
            Lookup::NoAlbum
        }
    }
}

// Subsonic `getCoverArt` with token auth — the `t`/`s` params carry the auth so
// the url is self-contained and fetchable without extra headers.
fn cover_url(album_id: &str) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(format!("{}{}", config::NAVIDROME_PASSWORD, SUBSONIC_SALT));
    let token = format!("{:x}", hasher.finalize());
    let mut url = Url::parse(&format!("{}/rest/getCoverArt", config::NAVIDROME_URL))
        .expect("NAVIDROME_URL must be a valid URL");
    url.query_pairs_mut()
        .append_pair("id", album_id)
        .append_pair("u", config::NAVIDROME_USERNAME)
        .append_pair("t", &token)
        .append_pair("s", SUBSONIC_SALT)
        .append_pair("v", "1.16.1")
        .append_pair("c", "lastfmrobot");
    url.to_string()
}

/// Resolve the Subsonic getCoverArt URL for a Navidrome album matched by the given
/// MusicBrainz id (release or release-group mbid, per `filter_field`), or None when
/// Navidrome has no matching album (or is unreachable). Results are cached in
/// users.sqlite keyed `nd:<field>:<mbid>` (same 6h TTL as the CAA cache); only a
/// definitive "no album" / "no art" is negative-cached, an unreachable server is left
/// uncached so the next status re-tries.
async fn resolve_cover_art_inner(filter_field: &str, mbid: &str) -> Option<String> {
    let key = format!("nd:{filter_field}:{mbid}");
    match db::DB.lock().unwrap().get_cover_art(&key) {
        Some(Some(resolved)) => return Some(resolved),
        Some(None) => return None,
        None => {}
    }

    let album_id = match lookup_album_id(filter_field, mbid).await {
        Lookup::Found(id) => id,
        Lookup::NoAlbum => {
            db::DB.lock().unwrap().store_cover_art(&key, None).ok();
            log::debug!("navidrome: no album for {filter_field}={mbid}, negative-cached");
            return None;
        }
        Lookup::Unreachable => {
            log::warn!("navidrome: unreachable during {filter_field}={mbid} resolve");
            return None;
        }
    };

    let url = cover_url(&album_id);

    // Fetch the cover and check it is not Navidrome's placeholder. Navidrome serves
    // the placeholder byte-identical (never resized), so a SHA match is a definitive
    // "no art" and falls through to CAA/Last.fm. The fetched bytes are warmed into
    // the shared bytes cache, so the caller's cover_art_bytes hits memory.
    let Ok(resp) = CLIENT_ND.get(&url).send().await else {
        log::warn!("navidrome: getCoverArt request failed for {filter_field}={mbid}");
        return Some(url); // don't treat a fetch failure as "no art"
    };
    let Ok(bytes) = resp.bytes().await else {
        log::warn!("navidrome: getCoverArt body failed for {filter_field}={mbid}");
        return Some(url);
    };

    if is_placeholder(&bytes) {
        db::DB.lock().unwrap().store_cover_art(&key, None).ok();
        log::debug!("navidrome: {filter_field}={mbid} served the placeholder, negative-cached");
        return None;
    }

    crate::api_requester::cache_cover_art_bytes(&url, bytes);
    db::DB.lock().unwrap().store_cover_art(&key, Some(&url)).ok();
    log::debug!("navidrome: resolved {filter_field}={mbid} -> {url}");
    Some(url)
}

/// Resolve a cover by release-group mbid (Navidrome `mbz_release_group_id`). Used
/// for tracks, where ListenBrainz provides `additional_info.release_group_mbid`.
pub async fn resolve_cover_art_release_group(rg_mbid: &str) -> Option<String> {
    resolve_cover_art_inner("mbz_release_group_id", rg_mbid).await
}

/// Resolve a cover by release mbid (Navidrome `mbz_album_id`). Used for albums,
/// where ListenBrainz stats only carry release-level mbids.
pub async fn resolve_cover_art_release(release_mbid: &str) -> Option<String> {
    resolve_cover_art_inner("mbz_album_id", release_mbid).await
}

/// Whether these bytes are Navidrome's album placeholder (i.e. the album has no art).
fn is_placeholder(bytes: &[u8]) -> bool {
    if config::NAVIDROME_PLACEHOLDER_SHA256.is_empty() {
        return false;
    }
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    format!("{digest:x}") == config::NAVIDROME_PLACEHOLDER_SHA256
}

/// Resolve cover art for one track: Navidrome first when the user is gated in
/// and the track carries a release-group mbid, otherwise the plain CAA/Last.fm
/// url. This is the single entry point the status paths use.
/// Resolve cover art for one track: Navidrome first when the user is gated in and
/// the track carries a MusicBrainz id, otherwise the plain CAA/Last.fm url. This
/// is the single entry point the status paths use.
///
/// ListenBrainz only reliably includes the release-group mbid in
/// `additional_info.release_group_mbid` when the submitting client sent it; some
/// clients omit it while `mbid_mapping.release_mbid` is still present. Both are
/// tried: the release-group mbid maps to Navidrome `mbz_release_group_id`, the
/// release mbid to `mbz_album_id`.
pub async fn resolve_cover_art_or_fallback(
    username: &str,
    rg_mbid: Option<&str>,
    release_mbid: Option<&str>,
    caa_url: Option<&str>,
) -> Option<String> {
    if enabled_for(username) {
        if let Some(rg_mbid) = rg_mbid
            && let Some(url) = resolve_cover_art_release_group(rg_mbid).await
        {
            return Some(url);
        }
        if let Some(release_mbid) = release_mbid
            && let Some(url) = resolve_cover_art_release(release_mbid).await
        {
            return Some(url);
        }
    }
    log::debug!(
        "navidrome: skipped/falling back (gated={}, rg={:?}, release={:?})",
        enabled_for(username),
        rg_mbid.map(|m| &m[..8.min(m.len())]),
        release_mbid.map(|m| &m[..8.min(m.len())])
    );
    api_requester::resolve_cover_art_url(caa_url).await
}
