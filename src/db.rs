use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, Result, params};

use crate::api_requester::ApiType;

/// Cover art cache entries expire after this long; the art is re-probed afterwards so
/// covers added to CAA later still get picked up.
pub const COVER_ART_CACHE_TTL_SECS: i64 = 6 * 60 * 60;

/// Cached inline file_ids are re-minted after this long. Telegram photo file_ids
/// stay valid indefinitely, but the TTL bounds the damage if one ever goes stale.
pub const INLINE_FILEID_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// MusicBrainz genre entries expire after this long. Genres are effectively immutable;
/// the long TTL just bounds the table while still re-validating monthly.
pub const MB_GENRE_CACHE_TTL_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Clone, Debug)]
pub struct User {
    pub tg_user_id: u64,
    pub account_username: String,
    api_type: String,
    pub profile_shown: bool,
    pub cover_shown: bool,
    /// Whether inline (via-inline-query) statuses include the album art photo.
    /// Separate from `cover_shown`: inline media edits can only render art by
    /// URL (Telegram fetches it server-side), which not every resolved art
    /// url survives — so inline defaults to text-only.
    pub inline_cover_shown: bool,
}

impl User {
    pub fn new(
        tg_user_id: u64,
        account_username: String,
        api_type: &ApiType,
        profile_shown: bool,
        cover_shown: bool,
        inline_cover_shown: bool,
    ) -> User {
        User {
            tg_user_id,
            account_username,
            api_type: api_type.to_string(),
            profile_shown,
            cover_shown,
            inline_cover_shown,
        }
    }

    pub fn api_type(&self) -> ApiType {
        self.api_type.parse().unwrap_or(ApiType::Lastfm)
    }
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn new() -> Db {
        let conn = Connection::open("users.sqlite").unwrap();
        let _ = conn.execute(
            "CREATE TABLE IF NOT EXISTS users (
            tg_user_id              INTEGER PRIMARY KEY,
            account_username        TEXT NOT NULL,
            api_type                TEXT NOT NULL,
            profile_shown           INTEGER NOT NULL DEFAULT 0,
            cover_shown             INTEGER NOT NULL DEFAULT 0,
            inline_cover_shown      INTEGER NOT NULL DEFAULT 0
            )",
            (),
        );
        // Older databases predate the inline cover preference; add it if missing
        // (a duplicate-column error on fresh databases is fine to ignore).
        let _ = conn.execute(
            "ALTER TABLE users ADD COLUMN inline_cover_shown INTEGER NOT NULL DEFAULT 0",
            (),
        );
        let _ = conn.execute(
            "CREATE TABLE IF NOT EXISTS cover_art_cache (
            url             TEXT PRIMARY KEY,
            resolved        TEXT,
            fetched_at      INTEGER NOT NULL
            )",
            (),
        );
        let _ = conn.execute(
            "CREATE TABLE IF NOT EXISTS mb_genre_cache (
            path            TEXT PRIMARY KEY,
            genres          TEXT NOT NULL,
            fetched_at      INTEGER NOT NULL
            )",
            (),
        );
        let _ = conn.execute(
            "CREATE TABLE IF NOT EXISTS inline_fileid_cache (
            key             TEXT PRIMARY KEY,
            file_id         TEXT NOT NULL,
            fetched_at      INTEGER NOT NULL
            )",
            (),
        );

        Db { conn }
    }

    pub fn fetch_user(&self, tg_user_id: u64) -> Option<User> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM users WHERE tg_user_id = ?1 LIMIT 1")
            .unwrap();

        stmt.query_map([tg_user_id as i64], |row| {
            Ok(User {
                tg_user_id: row.get::<_, i64>(0)? as u64,
                account_username: row.get(1)?,
                api_type: row.get(2)?,
                profile_shown: row.get(3)?,
                cover_shown: row.get(4)?,
                inline_cover_shown: row.get(5)?,
            })
        })
        .unwrap()
        .next()
        .map(|x| x.unwrap())
    }

    pub fn upsert_user(&self, user: &User) -> Result<usize> {
        self.conn.execute("INSERT INTO users (tg_user_id, account_username, api_type, profile_shown, cover_shown, inline_cover_shown) VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (tg_user_id) DO UPDATE SET account_username = ?2, api_type = ?3, profile_shown = ?4, cover_shown = ?5, inline_cover_shown = ?6",
         params![user.tg_user_id as i64, user.account_username, user.api_type, user.profile_shown, user.cover_shown, user.inline_cover_shown])
    }

    pub fn delete_user(&self, tg_user_id: u64) -> Result<usize> {
        self.conn.execute(
            "DELETE FROM users WHERE tg_user_id = ?1",
            [tg_user_id as i64],
        )
    }

    /// Look up a fresh cover art cache entry. `Some(Some(url))` is a cached resolved url,
    /// `Some(None)` a cached known-miss, and `None` means no fresh entry — probe again.
    pub fn get_cover_art(&self, url: &str) -> Option<Option<String>> {
        let cutoff = now_secs() - COVER_ART_CACHE_TTL_SECS;
        let mut stmt = self
            .conn
            .prepare("SELECT resolved FROM cover_art_cache WHERE url = ?1 AND fetched_at >= ?2 LIMIT 1")
            .unwrap();

        stmt.query(params![url, cutoff])
            .unwrap()
            .next()
            .ok()
            .flatten()
            .map(|row| row.get::<_, Option<String>>(0).ok().flatten())
    }

    pub fn store_cover_art(&self, url: &str, resolved: Option<&str>) -> Result<usize> {
        self.conn.execute(
            "INSERT INTO cover_art_cache (url, resolved, fetched_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (url) DO UPDATE SET resolved = ?2, fetched_at = ?3",
            params![url, resolved, now_secs()],
        )
    }

    pub fn clear_cover_art_cache(&self) -> Result<usize> {
        self.conn.execute("DELETE FROM cover_art_cache", ())
    }

    /// Cached Telegram file_id for a cover-art cache key. File_ids can't be
    /// minted without sending the media to a chat, so the minted result is
    /// persisted here — one dump-chat message per unique cover.
    pub fn get_inline_file_id(&self, key: &str) -> Option<String> {
        let cutoff = now_secs() - INLINE_FILEID_TTL_SECS;
        let mut stmt = self
            .conn
            .prepare("SELECT file_id FROM inline_fileid_cache WHERE key = ?1 AND fetched_at >= ?2 LIMIT 1")
            .unwrap();

        stmt.query(params![key, cutoff])
            .unwrap()
            .next()
            .ok()
            .flatten()
            .and_then(|row| row.get::<_, String>(0).ok())
    }

    pub fn store_inline_file_id(&self, key: &str, file_id: &str) -> Result<usize> {
        self.conn.execute(
            "INSERT INTO inline_fileid_cache (key, file_id, fetched_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (key) DO UPDATE SET file_id = ?2, fetched_at = ?3",
            params![key, file_id, now_secs()],
        )
    }

    /// Look up cached MusicBrainz genres/tags for an entity path
    /// (`release-group/<mbid>` etc.). `Some(vec)` (possibly empty = known to
    /// carry nothing) is a fresh hit; `None` means fetch from MB.
    pub fn get_mb_genres(&self, path: &str) -> Option<Vec<String>> {
        let cutoff = now_secs() - MB_GENRE_CACHE_TTL_SECS;
        let mut stmt = self
            .conn
            .prepare("SELECT genres FROM mb_genre_cache WHERE path = ?1 AND fetched_at >= ?2 LIMIT 1")
            .unwrap();

        stmt.query(params![path, cutoff])
            .unwrap()
            .next()
            .ok()
            .flatten()
            .and_then(|row| row.get::<_, String>(0).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    pub fn store_mb_genres(&self, path: &str, genres: &[String]) -> Result<usize> {
        let json = serde_json::to_string(genres).unwrap_or_else(|_| "[]".to_string());
        self.conn.execute(
            "INSERT INTO mb_genre_cache (path, genres, fetched_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (path) DO UPDATE SET genres = ?2, fetched_at = ?3",
            params![path, json, now_secs()],
        )
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub static DB: LazyLock<Mutex<Db>> = LazyLock::new(|| Mutex::new(Db::new()));
