use std::{error::Error, sync::LazyLock, time::Duration};

use chrono::DateTime;
use moka::future::Cache;

use crate::{api_requester::Track, config, config::KoitoInstance};

// Koito as a now-playing + recents source.
//
// A mapped user (see `config::KOITO_INSTANCES`, keyed by Telegram uid) reads
// from their own Koito instance first; ListenBrainz stays as fallback for when
// Koito is down or comes back empty. Auth is `Authorization: Token <api_key>`
// on every call, which Koito also accepts while its login gate is off — so
// this keeps working if the gate is ever enabled.
//
// Response shapes (Koito web API, `/apis/web/v1`):
// - `now-playing` carries the *full* track (mbids, listen_count, image,
//   album_id) — no hydration needed.
// - `listens` items only carry `{time, track:{id,title,artists,image}}`, so
//   each recent is hydrated via `GET /track/{id}` (duration, recording mbid,
//   playcount) plus `GET /album/{id}` (album title, release mbid).
// - Images are instance-relative (`/image/<uuid>/640x640.webp`); they get the
//   instance base URL prefixed and double as cover art with no CAA round trip.
// - `track.listen_count` is the user's playcount for that track.

type BoxError = Box<dyn Error + Send + Sync>;

// Same 10s budget as the ListenBrainz client — a slow Koito must not stall a
// status past the point where the LB fallback would already have answered.
static CLIENT_HTTPS: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .https_only(true)
        .user_agent("LastFM Robot (Telegram bot)")
        .build()
        .unwrap()
});

// Plain HTTP for docker-internal instances (e.g. http://koito:4110 on a shared
// compose network), where TLS terminates at the reverse proxy or not at all.
static CLIENT_HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("LastFM Robot (Telegram bot)")
        .build()
        .unwrap()
});

fn client_for(url: &str) -> &'static reqwest::Client {
    if url.starts_with("http://") {
        &CLIENT_HTTP
    } else {
        &CLIENT_HTTPS
    }
}

// album_id -> (title, release_mbid, image_url). Album metadata is effectively
// immutable, so a long TTL is safe; the track itself (whose listen_count moves)
// is always fetched fresh.
static ALBUM_CACHE: LazyLock<Cache<i32, (String, Option<String>, Option<String>)>> =
    LazyLock::new(|| {
        Cache::builder()
            .max_capacity(500)
            .time_to_live(Duration::from_secs(6 * 60 * 60))
            .build()
    });

pub fn instance_for_uid(uid: u64) -> Option<&'static KoitoInstance> {
    config::KOITO_INSTANCES.iter().find(|i| i.uid == uid)
}

pub fn has_instance(uid: u64) -> bool {
    instance_for_uid(uid).is_some()
}

async fn get_json(url: &str, inst: &KoitoInstance) -> Result<serde_json::Value, BoxError> {
    let json = client_for(inst.url)
        .get(url)
        .header("Authorization", format!("Token {}", inst.api_key))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    if json
        .get("error")
        .and_then(|e| e.as_str())
        .is_some_and(|e| !e.is_empty())
    {
        return Err(Box::from(json["error"].as_str().unwrap_or("Koito error")));
    }
    Ok(json)
}

fn artist_names(track: &serde_json::Value) -> String {
    track["artists"]
        .as_array()
        .map(|artists| {
            artists
                .iter()
                .filter_map(|a| a["name"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// Largest usable Koito image, made absolute. `large` (640px) is preferred —
/// same ballpark as the 500px the CAA path prefers.
fn image_url(base_url: &str, img: &serde_json::Value) -> Option<String> {
    let rel = ["large", "xl", "medium", "small", "xs"]
        .iter()
        .filter_map(|s| img.get(s)?.as_str())
        .find(|s| !s.is_empty())?;
    if rel.starts_with("http") {
        Some(rel.to_string())
    } else {
        Some(format!("{base_url}{rel}"))
    }
}

fn parse_time(s: &str) -> Option<u64> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp().max(0) as u64)
}

async fn album_info(
    inst: &KoitoInstance,
    album_id: i32,
) -> (Option<String>, Option<String>, Option<String>) {
    if album_id <= 0 {
        return (None, None, None);
    }
    if let Some(hit) = ALBUM_CACHE.get(&album_id).await {
        return (Some(hit.0), hit.1, hit.2);
    }
    let Ok(json) = get_json(&format!("{}/apis/web/v1/album/{album_id}", inst.url), inst).await
    else {
        return (None, None, None);
    };
    let title = json["title"].as_str().map(str::to_string);
    let release_mbid = json["musicbrainz_id"].as_str().map(str::to_string);
    let image = image_url(inst.url, &json["image"]);
    if let Some(title) = title.clone() {
        ALBUM_CACHE
            .insert(album_id, (title, release_mbid.clone(), image.clone()))
            .await;
    }
    (title, release_mbid, image)
}

async fn track_from_full(
    inst: &KoitoInstance,
    full: &serde_json::Value,
    date: Option<u64>,
    now_playing: bool,
) -> Track {
    let album_id = full["album_id"].as_i64().unwrap_or_default() as i32;
    let (album, release_mbid, album_image) = album_info(inst, album_id).await;
    let mut art = image_url(inst.url, &full["image"]);
    if art.is_none() {
        art = album_image;
    }
    Track {
        name: full["title"].as_str().unwrap_or_default().to_string(),
        album,
        artist: artist_names(full),
        album_art_url: art,
        date,
        duration: full["duration"].as_i64().unwrap_or_default().max(0) as u64,
        listeners: 0,
        playcount: 0,
        user_playcount: full["listen_count"].as_i64().unwrap_or_default().max(0) as u64,
        user_loved: false,
        now_playing,
        tags: None,
        recording_mbid: full["musicbrainz_id"].as_str().map(str::to_string),
        release_mbid,
        // The web API exposes the recording mbid plus the release mbid (via the
        // album); there is no release-group mbid anywhere in these responses.
        release_group_mbid: None,
    }
}

/// Now-playing plus recent listens as `Track`s, mirroring the ListenBrainz
/// fetch shape (NP first, then recents). Empty/error lets the caller fall back
/// to ListenBrainz.
pub async fn fetch_recent_tracks(
    inst: &'static KoitoInstance,
    actual_limit: usize,
) -> Result<Vec<Track>, BoxError> {
    let np_json = match get_json(&format!("{}/apis/web/v1/now-playing", inst.url), inst).await
    {
        Ok(json) => json,
        Err(e) => {
            log::warn!("koito: now-playing failed for {}: {e}", inst.url);
            return Err(e);
        }
    };
    let mut np: Option<Track> = None;
    if np_json["currently_playing"].as_bool() == Some(true) {
        let track = track_from_full(inst, &np_json["track"], None, true).await;
        if !track.name.is_empty() {
            np = Some(track);
        }
    }
    if np.is_some() && actual_limit == 1 {
        return Ok(np.into_iter().collect());
    }

    let listens = match get_json(
        &format!("{}/apis/web/v1/listens?limit=3&period=all_time", inst.url),
        inst,
    )
    .await
    {
        Ok(json) => json,
        Err(e) => {
            log::warn!("koito: listens failed for {}: {e}", inst.url);
            return Err(e);
        }
    };
    let items = listens["items"].as_array().cloned().unwrap_or_default();

    // Hydrate concurrently; track ids repeat, so the album cache usually covers.
    let mut handles = Vec::with_capacity(items.len());
    for item in items {
        handles.push(tokio::spawn(async move {
            let id = item["track"]["id"].as_i64().unwrap_or_default();
            if id <= 0 {
                return Err(Box::from("Koito listen without track id") as BoxError);
            }
            let full =
                get_json(&format!("{}/apis/web/v1/track/{id}", inst.url), inst).await?;
            let date = item["time"].as_str().and_then(parse_time);
            let track = track_from_full(inst, &full, date, false).await;
            if track.name.is_empty() {
                return Err(Box::from("Koito track without title") as BoxError);
            }
            Ok(track)
        }));
    }

    let mut tracks = Vec::new();
    if let Some(np) = np {
        tracks.push(np);
    }
    for handle in handles {
        if let Ok(Ok(track)) = handle.await {
            tracks.push(track);
        }
    }
    if tracks.is_empty() {
        log::debug!("koito: no usable tracks for {} (NP + hydrations all missed)", inst.url);
    }
    Ok(tracks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn artists_join_with_comma() {
        let t = json!({"artists": [{"id": 1, "name": "SadTurs"}, {"id": 2, "name": "Axell"}]});
        assert_eq!(artist_names(&t), "SadTurs, Axell");
    }

    #[test]
    fn artists_missing_is_empty() {
        assert_eq!(artist_names(&json!({})), "");
    }

    #[test]
    fn image_prefers_large_and_prefixes_base() {
        let img = json!({"small": "/image/x/128x128.webp", "large": "/image/x/640x640.webp"});
        assert_eq!(
            image_url("https://koito.bestfast.eu.org", &img).as_deref(),
            Some("https://koito.bestfast.eu.org/image/x/640x640.webp")
        );
    }

    #[test]
    fn image_passes_absolute_through() {
        let img = json!({"large": "https://example.com/a.png"});
        assert_eq!(
            image_url("https://koito.bestfast.eu.org", &img).as_deref(),
            Some("https://example.com/a.png")
        );
    }

    #[test]
    fn image_empty_is_none() {
        let img = json!({"xs": "", "small": "", "medium": "", "large": "", "xl": ""});
        assert_eq!(image_url("https://koito.bestfast.eu.org", &img), None);
    }

    #[test]
    fn time_parses_rfc3339() {
        assert_eq!(parse_time("2026-09-06T18:34:45Z"), Some(1788719685));
        assert_eq!(parse_time("not a time"), None);
    }
}
