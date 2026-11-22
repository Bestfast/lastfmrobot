use std::{error::Error, sync::LazyLock, time::Duration};

use chrono::{DateTime, Utc};
use moka::future::Cache;

use crate::{
    api_requester::{Track, caa_front_url_largest},
    config,
    config::TealUser,
};

// teal.fm as a now-playing + recents source (AT Protocol).
//
// Plays live in the user's own repo, so this needs no auth and no API key:
// the current track is the single `self` record of `fm.teal.actor.status`
// (with an `expiry`), the history is the `fm.teal.feed.play` collection read
// newest-first via `listRecords?reverse=true`. Handle -> DID comes from the
// AppView, DID -> PDS endpoint from plc.directory, then plain
// `com.atproto.repo.*` calls straight against the PDS.
//
// Records are mbid-first, which is the point of this source: `releaseName` +
// `releaseMbId` carry the exact album, so Navidrome/CAA resolve the right
// cover and the status prints the real album instead of a last.fm/LB guess.
// teal.fm serves no art of its own, so the track carries a Cover Art Archive
// probe url derived from its release mbid - exactly what the ListenBrainz
// parser hands the Navidrome/CAA chain. teal.fm has no play counts either;
// those stay on the user's Koito (see koito::playcount_for).

type BoxError = Box<dyn Error + Send + Sync>;

// Same 10s budget as the ListenBrainz/Koito clients: a slow teal.fm must not
// stall a status past the point where the fallbacks would already have answered.
static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .https_only(true)
        .user_agent("LastFM Robot (Telegram bot)")
        .build()
        .unwrap()
});

// handle -> (did, pds_endpoint). Identity only changes on a PDS migration, so
// a long TTL is safe and saves two round trips per status.
static IDENTITY_CACHE: LazyLock<Cache<String, (String, String)>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(50)
        .time_to_live(Duration::from_secs(6 * 60 * 60))
        .build()
});

// release mbid -> release-group mbid. Releases know their group, plays don't,
// and the canonical album art lives on the release group. Failed lookups are
// deliberately not cached so a MusicBrainz blip retries on the next status.
static RG_CACHE: LazyLock<Cache<String, String>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(500)
        .time_to_live(Duration::from_secs(24 * 60 * 60))
        .build()
});

fn caa_release_group_url(rg_mbid: &str) -> String {
    format!(
        "https://coverartarchive.org/release-group/{rg_mbid}/front-{}",
        crate::api_requester::CAA_SIZES[0]
    )
}

async fn release_group_mbid(release_mbid: &str) -> Option<String> {
    if let Some(hit) = RG_CACHE.get(release_mbid).await {
        return Some(hit);
    }
    let url = format!(
        "https://musicbrainz.org/ws/2/release/{release_mbid}?inc=release-groups&fmt=json"
    );
    let rg = match get_json(&url).await {
        Ok(json) => json["release-group"]["id"].as_str().map(str::to_string),
        Err(e) => {
            log::warn!("teal: musicbrainz release lookup failed for {release_mbid}: {e}");
            return None;
        }
    };
    if let Some(rg) = rg.clone() {
        RG_CACHE.insert(release_mbid.to_string(), rg).await;
    }
    rg
}

/// Fill in the release group and point the cover at its art. teal.fm tags the
/// release, but the release group is the album: several pressings of one album
/// carry different release-level art, while the group has the canonical cover.
async fn enrich_release_group(track: &mut Track) {
    let Some(release_mbid) = track.release_mbid.clone() else {
        return;
    };
    let Some(rg) = release_group_mbid(&release_mbid).await else {
        return;
    };
    track.album_art_url = Some(caa_release_group_url(&rg));
    track.release_group_mbid = Some(rg);
}

pub fn user_for_uid(uid: u64) -> Option<&'static TealUser> {
    config::TEAL_USERS.iter().find(|u| u.uid == uid)
}

pub fn has_user(uid: u64) -> bool {
    user_for_uid(uid).is_some()
}

/// `mbid:xxxxxxxx-...` -> `xxxxxxxx-...`. teal.fm prefixes every mbid it
/// writes; the bot's mbid consumers (Navidrome, CAA) want the bare uuid.
fn parse_mbid(value: &serde_json::Value) -> Option<String> {
    let raw = value.as_str()?.trim();
    let bare = raw
        .strip_prefix("mbid:")
        .or_else(|| raw.strip_prefix("MBID:"))
        .unwrap_or(raw)
        .trim();
    if bare.is_empty() {
        None
    } else {
        Some(bare.to_ascii_lowercase())
    }
}

fn artist_names(item: &serde_json::Value) -> String {
    item["artists"]
        .as_array()
        .map(|artists| {
            artists
                .iter()
                .filter_map(|a| a["artistName"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn parse_time(s: Option<&str>) -> Option<u64> {
    DateTime::parse_from_rfc3339(s?)
        .ok()
        .map(|d| d.timestamp().max(0) as u64)
}

fn parse_item(item: &serde_json::Value, now_playing: bool) -> Option<Track> {
    let name = item["trackName"].as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    let artist = artist_names(item);
    let album = item["releaseName"]
        .as_str()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string);
    let release_mbid = parse_mbid(&item["releaseMbId"]);
    // teal.fm has no art endpoint: hand the chain the CAA probe url for the
    // exact release, so Navidrome can answer first and CAA afterwards.
    let album_art_url = release_mbid.as_deref().map(caa_front_url_largest);
    Some(Track {
        name: name.to_string(),
        album,
        artist,
        album_art_url,
        date: parse_time(item["playedTime"].as_str()),
        duration: item["duration"].as_u64().unwrap_or_default(),
        listeners: 0,
        playcount: 0,
        // Play counts stay on Koito - see koito::playcount_for.
        user_playcount: 0,
        user_loved: false,
        now_playing,
        tags: None,
        recording_mbid: parse_mbid(&item["recordingMbId"]),
        release_mbid,
        release_group_mbid: None,
    })
}

async fn identity(handle: &str) -> Result<(String, String), BoxError> {
    if let Some(hit) = IDENTITY_CACHE.get(handle).await {
        return Ok(hit);
    }
    let json: serde_json::Value = CLIENT
        .get(format!(
            "https://public.api.bsky.app/xrpc/com.atproto.identity.resolveHandle?handle={handle}"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let did = json["did"]
        .as_str()
        .ok_or("teal: handle did not resolve to a DID")?
        .to_string();

    let doc: serde_json::Value = CLIENT
        .get(format!("https://plc.directory/{did}"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let pds = doc["service"]
        .as_array()
        .and_then(|services| {
            services
                .iter()
                .find(|s| s["id"].as_str() == Some("#atproto_pds"))
                .and_then(|s| s["serviceEndpoint"].as_str())
        })
        .ok_or("teal: DID document has no atproto PDS")?
        .trim_end_matches('/')
        .to_string();

    IDENTITY_CACHE
        .insert(handle.to_string(), (did.clone(), pds.clone()))
        .await;
    Ok((did, pds))
}

async fn get_json(url: &str) -> Result<serde_json::Value, BoxError> {
    Ok(CLIENT
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Now-playing (from `fm.teal.actor.status`, only while it hasn't expired) plus
/// the newest plays, mirroring the ListenBrainz/Koito fetch shape. Errors and
/// empties let the caller fall back to the next source.
pub async fn fetch_recent_tracks(
    handle: &str,
    actual_limit: usize,
) -> Result<Vec<Track>, BoxError> {
    let mut tracks = fetch_plays(handle, actual_limit).await?;
    // Only the first track ever feeds the cover render, so resolving its
    // release group costs one extra (cached) MusicBrainz call, not four.
    if let Some(first) = tracks.first_mut() {
        enrich_release_group(first).await;
    }
    Ok(tracks)
}

async fn fetch_plays(handle: &str, actual_limit: usize) -> Result<Vec<Track>, BoxError> {
    // A DID is `did:plc:<base32>` - ':' is legal in a query value, no escaping.
    let (repo, pds) = identity(handle).await?;

    let mut tracks: Vec<Track> = Vec::new();
    let mut np_date: Option<u64> = None;

    let status_url = format!(
        "{pds}/xrpc/com.atproto.repo.getRecord?repo={repo}&collection=fm.teal.actor.status&rkey=self"
    );
    match get_json(&status_url).await {
        Ok(json) => {
            let value = &json["value"];
            let expiry = parse_time(value["expiry"].as_str());
            let live = expiry.is_some_and(|e| e >= Utc::now().timestamp().max(0) as u64);
            if live && let Some(track) = parse_item(&value["item"], true) {
                np_date = track.date;
                tracks.push(track);
            }
        }
        // No status record is normal (nothing playing); a real transport error
        // only fails the NP part, recents can still answer.
        Err(e) => log::debug!("teal: status lookup failed for {handle}: {e}"),
    }

    if tracks.len() >= actual_limit {
        return Ok(tracks);
    }

    // Default `listRecords` order is newest-first (reverse=true would give the
    // oldest of the window), which is what the NP + recents shape needs.
    let plays_url = format!(
        "{pds}/xrpc/com.atproto.repo.listRecords?repo={repo}&collection=fm.teal.feed.play&limit=3"
    );
    let json = match get_json(&plays_url).await {
        Ok(json) => json,
        Err(e) => {
            if tracks.is_empty() {
                return Err(e);
            }
            log::warn!("teal: recent plays failed for {handle}: {e}");
            return Ok(tracks);
        }
    };

    for record in json["records"].as_array().into_iter().flatten() {
        let Some(track) = parse_item(&record["value"], false) else {
            continue;
        };
        // The status item is the same play as the newest record while playing.
        if np_date.is_some_and(|d| d == track.date.unwrap_or_default()) {
            continue;
        }
        tracks.push(track);
    }

    if tracks.is_empty() {
        log::debug!("teal: no usable plays for {handle}");
    }
    Ok(tracks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mbids_lose_the_prefix_and_lowercase() {
        assert_eq!(
            parse_mbid(&json!("mbid:1E7336B8-625D-4811-8200-4742F46CF99F")).as_deref(),
            Some("1e7336b8-625d-4811-8200-4742f46cf99f")
        );
        assert_eq!(
            parse_mbid(&json!("4716d919-444b-42c6-a538-ea65dd66945b")).as_deref(),
            Some("4716d919-444b-42c6-a538-ea65dd66945b")
        );
        assert_eq!(parse_mbid(&json!("")), None);
        assert_eq!(parse_mbid(&json!("mbid:")), None);
        assert_eq!(parse_mbid(&json!(null)), None);
    }

    #[test]
    fn artists_join_with_comma() {
        let item = json!({"artists": [{"artistName": "PSYQUI"}, {"artistName": "mami"}]});
        assert_eq!(artist_names(&item), "PSYQUI, mami");
        assert_eq!(artist_names(&json!({})), "");
    }

    #[test]
    fn item_maps_to_track_without_cover_or_playcount() {
        let item = json!({
            "trackName": "Underdog",
            "artists": [{"artistName": "Eve", "artistMbId": "mbid:66bdd1c9-d1c5-40b7-a487-5061fffbd87d"}],
            "releaseName": "Underdog",
            "releaseMbId": "mbid:20f7f039-51b0-488a-8696-641a0a28fa14",
            "recordingMbId": "mbid:1e7336b8-625d-4811-8200-4742f46cf99f",
            "duration": 194,
            "playedTime": "2026-11-22T00:58:45.000Z"
        });
        let track = parse_item(&item, true).unwrap();
        assert_eq!(track.name, "Underdog");
        assert_eq!(track.artist, "Eve");
        assert_eq!(track.album.as_deref(), Some("Underdog"));
        assert_eq!(track.duration, 194);
        assert_eq!(track.date, Some(1795309125));
        assert_eq!(track.album_art_url.as_deref(),
            Some("https://coverartarchive.org/release/20f7f039-51b0-488a-8696-641a0a28fa14/front-500"));
        assert_eq!(track.user_playcount, 0);
        assert!(track.now_playing);
        assert_eq!(
            track.release_mbid.as_deref(),
            Some("20f7f039-51b0-488a-8696-641a0a28fa14")
        );
        assert_eq!(
            track.recording_mbid.as_deref(),
            Some("1e7336b8-625d-4811-8200-4742f46cf99f")
        );
        assert_eq!(track.release_group_mbid, None);
    }

    #[test]
    fn item_without_title_is_dropped() {
        assert!(parse_item(&json!({"artists": []}), false).is_none());
        assert!(parse_item(&json!({"trackName": "  "}), false).is_none());
    }

    #[test]
    fn missing_release_name_means_no_album() {
        let track = parse_item(&json!({"trackName": "x", "releaseName": ""}), false).unwrap();
        assert_eq!(track.album, None);
    }
}
