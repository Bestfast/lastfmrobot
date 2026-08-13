use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, Result, params};

use crate::api_requester::ApiType;

/// Cover art cache entries expire after this long; the art is re-probed afterwards so
/// covers added to CAA later still get picked up.
pub const COVER_ART_CACHE_TTL_SECS: i64 = 6 * 60 * 60;

#[derive(Clone, Debug)]
pub struct User {
    pub tg_user_id: u64,
    pub account_username: String,
    api_type: String,
    pub profile_shown: bool,
    pub cover_shown: bool,
}

impl User {
    pub fn new(
        tg_user_id: u64,
        account_username: String,
        api_type: &ApiType,
        profile_shown: bool,
        cover_shown: bool,
    ) -> User {
        User {
            tg_user_id,
            account_username,
            api_type: api_type.to_string(),
            profile_shown,
            cover_shown,
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
            cover_shown             INTEGER NOT NULL DEFAULT 0
            )",
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
            })
        })
        .unwrap()
        .next()
        .map(|x| x.unwrap())
    }

    pub fn upsert_user(&self, user: &User) -> Result<usize> {
        self.conn.execute("INSERT INTO users (tg_user_id, account_username, api_type, profile_shown, cover_shown) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (tg_user_id) DO UPDATE SET account_username = ?2, api_type = ?3, profile_shown = ?4, cover_shown = ?5",
         params![user.tg_user_id as i64, user.account_username, user.api_type, user.profile_shown, user.cover_shown])
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
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub static DB: LazyLock<Mutex<Db>> = LazyLock::new(|| Mutex::new(Db::new()));
