use rusqlite::{params, Connection};
use std::path::Path;

use crate::config::DatabaseConfig;
use crate::error::{Result, SeakarrError};

pub struct Database {
    pub conn: Connection,
}

// ── Domain structs ──

#[derive(Debug, Clone)]
pub struct ProcessedAlbum {
    pub id: i64,
    pub artist: String,
    pub album: String,
    pub status: String,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub first_seen: String,
    pub last_tried: Option<String>,
}

#[derive(Debug, Clone)]
pub struct QueuedDownload {
    pub id: i64,
    pub artist: String,
    pub album: Option<String>,
    pub filename: String,
    pub username: String,
    pub size_bytes: i64,
    pub bitrate: Option<i32>,
    pub format: Option<String>,
    pub status: String,
    pub progress: f64,
    pub local_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PeerReputation {
    pub username: String,
    pub total_downloads: u32,
    pub successful: u32,
    pub avg_speed_kbps: f64,
}

/// Input data for enqueueing a download into the persistent queue.
#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub artist: String,
    pub album: Option<String>,
    pub filename: String,
    pub username: String,
    pub size_bytes: i64,
    pub bitrate: Option<i32>,
    pub format: Option<String>,
}

impl Database {
    pub fn open(db_dir: &Path, _db_config: &DatabaseConfig) -> Result<Self> {
        std::fs::create_dir_all(db_dir)
            .map_err(|e| SeakarrError::Config(format!("cannot create db dir {db_dir:?}: {e}")))?;
        let db_path = db_dir.join("seakarr.db");
        let conn = Connection::open(&db_path)?;
        let db = Database { conn };
        db.migrate()?;
        Ok(db)
    }

    /// Open an in-memory database for testing.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Database { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "DROP TABLE IF EXISTS browse_cache;

            CREATE TABLE IF NOT EXISTS processed_albums (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                artist      TEXT NOT NULL,
                album       TEXT NOT NULL,
                status      TEXT NOT NULL DEFAULT 'pending',
                attempts    INTEGER NOT NULL DEFAULT 0,
                last_error  TEXT,
                first_seen  TEXT NOT NULL DEFAULT (datetime('now')),
                last_tried  TEXT,
                UNIQUE(artist, album)
            );

            CREATE TABLE IF NOT EXISTS download_queue (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                artist      TEXT NOT NULL,
                album       TEXT,
                filename    TEXT NOT NULL,
                username    TEXT NOT NULL,
                size_bytes  INTEGER NOT NULL,
                bitrate     INTEGER,
                format      TEXT,
                status      TEXT NOT NULL DEFAULT 'queued',
                progress    REAL NOT NULL DEFAULT 0.0,
                local_path  TEXT,
                created_at  TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS peer_reputation (
                username        TEXT PRIMARY KEY,
                total_downloads INTEGER NOT NULL DEFAULT 0,
                successful      INTEGER NOT NULL DEFAULT 0,
                avg_speed_kbps  REAL NOT NULL DEFAULT 0.0,
                speed_samples   INTEGER NOT NULL DEFAULT 0,
                last_seen       TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS search_history (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                artist       TEXT NOT NULL,
                album        TEXT,
                result_count INTEGER NOT NULL DEFAULT 0,
                duration_ms  INTEGER,
                searched_at  TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS download_stats (
                id             INTEGER PRIMARY KEY AUTOINCREMENT,
                artist         TEXT NOT NULL,
                album          TEXT NOT NULL,
                username       TEXT NOT NULL,
                filename       TEXT NOT NULL,
                size_bytes     INTEGER NOT NULL,
                bitrate        INTEGER,
                format         TEXT,
                speed_kbps     REAL,
                duration_secs  REAL,
                retries        INTEGER NOT NULL DEFAULT 0,
                status         TEXT NOT NULL,
                downloaded_at  TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS batch_jobs (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                file_path   TEXT NOT NULL,
                total_lines INTEGER NOT NULL DEFAULT 0,
                completed   INTEGER NOT NULL DEFAULT 0,
                failed      INTEGER NOT NULL DEFAULT 0,
                status      TEXT NOT NULL DEFAULT 'running',
                created_at  TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS batch_job_lines (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                job_id       INTEGER NOT NULL REFERENCES batch_jobs(id),
                line_number  INTEGER NOT NULL,
                artist       TEXT NOT NULL,
                album        TEXT,
                status       TEXT NOT NULL DEFAULT 'pending',
                error        TEXT,
                processed_at TEXT
            );",
        )?;

        // Add speed_samples to peer_reputation for databases created before
        // this column existed (fresh DBs get it from CREATE TABLE above). The
        // duplicate-column error on an up-to-date DB is expected and ignored.
        let _ = self.conn.execute(
            "ALTER TABLE peer_reputation ADD COLUMN speed_samples INTEGER NOT NULL DEFAULT 0",
            [],
        );

        Ok(())
    }

    // ── Processed albums ──

    pub fn mark_album_processed(&self, artist: &str, album: &str, status: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO processed_albums (artist, album, status, attempts, last_tried)
             VALUES (?1, ?2, ?3, 1, datetime('now'))
             ON CONFLICT(artist, album) DO UPDATE SET
               status = excluded.status,
               attempts = attempts + 1,
               last_tried = datetime('now')",
            params![artist, album, status],
        )?;
        Ok(())
    }

    pub fn is_album_processed(&self, artist: &str, album: &str) -> Result<bool> {
        let count: i32 = self.conn.query_row(
            "SELECT COUNT(*) FROM processed_albums WHERE artist = ?1 AND album = ?2 AND status = 'success'",
            params![artist, album],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Return the current status of an album's processing run, or None when
    /// the album is not tracked in the database.
    pub fn get_album_status(&self, artist: &str, album: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT status FROM processed_albums WHERE artist = ?1 AND album = ?2")?;
        let mut rows = stmt.query_map(params![artist, album], |row| row.get::<_, String>(0))?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn get_processed_albums(&self) -> Result<Vec<ProcessedAlbum>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, artist, album, status, attempts, last_error, first_seen, last_tried
             FROM processed_albums ORDER BY artist, album",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ProcessedAlbum {
                id: row.get(0)?,
                artist: row.get(1)?,
                album: row.get(2)?,
                status: row.get(3)?,
                attempts: row.get(4)?,
                last_error: row.get(5)?,
                first_seen: row.get(6)?,
                last_tried: row.get(7)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    // ── Download queue ──

    pub fn enqueue_download(&self, req: &DownloadRequest) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO download_queue (artist, album, filename, username, size_bytes, bitrate, format)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                req.artist,
                req.album,
                req.filename,
                req.username,
                req.size_bytes,
                req.bitrate,
                req.format,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn get_queued_downloads(&self) -> Result<Vec<QueuedDownload>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, artist, album, filename, username, size_bytes, bitrate, format, status, progress, local_path
             FROM download_queue WHERE status = 'queued' ORDER BY id"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(QueuedDownload {
                id: row.get(0)?,
                artist: row.get(1)?,
                album: row.get(2)?,
                filename: row.get(3)?,
                username: row.get(4)?,
                size_bytes: row.get(5)?,
                bitrate: row.get(6)?,
                format: row.get(7)?,
                status: row.get(8)?,
                progress: row.get(9)?,
                local_path: row.get(10)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    // ── Peer reputation ──

    pub fn update_peer_reputation(
        &self,
        username: &str,
        speed_kbps: f64,
        success: bool,
    ) -> Result<()> {
        let username = username.to_lowercase();
        self.conn.execute(
            "INSERT INTO peer_reputation (username, total_downloads, successful, avg_speed_kbps, speed_samples, last_seen)
             VALUES (?1, 1, ?2, ?3, CASE WHEN ?2 = 1 AND ?3 > 0.0 THEN 1 ELSE 0 END, datetime('now'))
             ON CONFLICT(username) DO UPDATE SET
               total_downloads = total_downloads + 1,
               successful = successful + ?2,
               avg_speed_kbps = CASE WHEN ?2 = 1 AND ?3 > 0.0
                 THEN (avg_speed_kbps * speed_samples + ?3) / (speed_samples + 1)
                 ELSE avg_speed_kbps END,
               speed_samples = speed_samples + CASE WHEN ?2 = 1 AND ?3 > 0.0 THEN 1 ELSE 0 END,
               last_seen = datetime('now')",
            params![username, if success { 1 } else { 0 }, speed_kbps],
        )?;
        Ok(())
    }

    pub fn get_reputation_map(&self) -> Result<std::collections::HashMap<String, PeerReputation>> {
        let mut stmt = self.conn.prepare(
            "SELECT username, total_downloads, successful, avg_speed_kbps
             FROM peer_reputation",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                PeerReputation {
                    username: row.get(0)?,
                    total_downloads: row.get::<_, u32>(1)?,
                    successful: row.get::<_, u32>(2)?,
                    avg_speed_kbps: row.get(3)?,
                },
            ))
        })?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (name, rep) = row?;
            map.insert(name, rep);
        }
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    #[test]
    fn test_browse_cache_dropped_from_existing_db() {
        // Simulate an existing install whose DB already has the browse_cache
        // table from before this feature was removed.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE browse_cache (
                username   TEXT NOT NULL,
                path       TEXT NOT NULL,
                data_json  TEXT NOT NULL,
                cached_at  TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (username, path)
            );",
        )
        .unwrap();
        let db = Database { conn };
        db.migrate().unwrap();

        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='browse_cache'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 0,
            "browse_cache table should be dropped by migrate()"
        );
    }

    #[test]
    fn test_create_tables() {
        let db = test_db();
        db.migrate().unwrap();

        // Verify all 7 tables exist by querying sqlite_master
        let tables: Vec<String> = db
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"processed_albums".to_string()));
        assert!(tables.contains(&"download_queue".to_string()));
        assert!(tables.contains(&"peer_reputation".to_string()));
        assert!(tables.contains(&"search_history".to_string()));
        assert!(tables.contains(&"download_stats".to_string()));
        assert!(tables.contains(&"batch_jobs".to_string()));
        assert!(tables.contains(&"batch_job_lines".to_string()));
    }

    #[test]
    fn test_mark_album_processed() {
        let db = test_db();
        db.migrate().unwrap();

        db.mark_album_processed("Test Artist", "Test Album", "success")
            .unwrap();

        let albums = db.get_processed_albums().unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].artist, "Test Artist");
        assert_eq!(albums[0].album, "Test Album");
        assert_eq!(albums[0].status, "success");
    }

    #[test]
    fn test_album_already_processed() {
        let db = test_db();
        db.migrate().unwrap();

        db.mark_album_processed("Artist", "Album", "success")
            .unwrap();
        assert!(db.is_album_processed("Artist", "Album").unwrap());
        assert!(!db.is_album_processed("Artist", "Other").unwrap());
    }

    #[test]
    fn test_get_album_status() {
        let db = test_db();
        db.migrate().unwrap();

        db.mark_album_processed("Artist", "Album", "success")
            .unwrap();
        let status = db.get_album_status("Artist", "Album").unwrap();
        assert_eq!(status, Some("success".to_string()));

        // Untracked albums return None.
        let missing = db.get_album_status("Nobody", "Nothing").unwrap();
        assert_eq!(missing, None);
    }

    #[test]
    fn test_download_queue_persistence() {
        let db = test_db();
        db.migrate().unwrap();

        db.enqueue_download(&DownloadRequest {
            artist: "Artist".into(),
            album: Some("Album".into()),
            filename: "file.flac".into(),
            username: "user1".into(),
            size_bytes: 10_000_000,
            bitrate: Some(320),
            format: Some("flac".into()),
        })
        .unwrap();

        let queue = db.get_queued_downloads().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].filename, "file.flac");
        assert_eq!(queue[0].status, "queued");
    }

    #[test]
    fn test_peer_reputation_upsert() {
        let db = test_db();
        db.migrate().unwrap();

        db.update_peer_reputation("fastuser", 500.0, true).unwrap();
        db.update_peer_reputation("fastuser", 600.0, true).unwrap();

        let map = db.get_reputation_map().unwrap();
        assert_eq!(map.len(), 1);
        let peer = &map["fastuser"];
        assert_eq!(peer.username, "fastuser");
        // Avg speed should be updated: (500 + 600) / 2 = 550
        assert!((peer.avg_speed_kbps - 550.0).abs() < 1.0);
    }

    #[test]
    fn test_get_reputation_map_returns_indexed_peers() {
        let db = test_db();
        db.update_peer_reputation("alice", 500.0, true).unwrap();
        db.update_peer_reputation("alice", 700.0, true).unwrap();
        db.update_peer_reputation("bob", 300.0, false).unwrap();

        let map = db.get_reputation_map().unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map["alice"].total_downloads, 2);
        assert_eq!(map["alice"].successful, 2);
        assert_eq!(map["alice"].avg_speed_kbps, 600.0); // (500 + 700) / 2
        assert_eq!(map["bob"].total_downloads, 1);
        assert_eq!(map["bob"].successful, 0);
    }
}
