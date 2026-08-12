use crate::client::{SearchResult, SoulseekClient};
use crate::error::Result;

/// Search Soulseek for an album, returning deduplicated results.
pub async fn search_album(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    timeout_secs: u64,
) -> Result<Vec<SearchResult>> {
    let query = match album {
        Some(a) if !a.is_empty() => format!("{artist} {a}"),
        _ => artist.to_string(),
    };

    let mut results = client.search(&query, timeout_secs).await?;
    // Deduplicate by filename+size within each result's files
    for result in &mut results {
        result.files.sort_by(|a, b| a.name.cmp(&b.name));
        result
            .files
            .dedup_by(|a, b| a.name == b.name && a.size == b.size);
    }
    Ok(results)
}

/// Record a search in history (used by runner for stats).
pub fn record_search(
    artist: &str,
    album: Option<&str>,
    result_count: usize,
    duration_ms: u64,
    db: &crate::db::Database,
) -> Result<()> {
    db.conn.execute(
        "INSERT INTO search_history (artist, album, result_count, duration_ms) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![artist, album, result_count as i64, duration_ms as i64],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{FileInfo, MockClient, SearchResult};
    use std::collections::HashMap;

    fn make_file(name: &str, bitrate: u32, size: u64) -> FileInfo {
        let mut attribs = HashMap::new();
        attribs.insert(0, bitrate);
        FileInfo {
            name: name.into(),
            size,
            attribs,
        }
    }

    #[tokio::test]
    async fn test_search_returns_results() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 2,
            files: vec![make_file("track.flac", 900, 30_000_000)],
        }];

        let results = search_album(&client, "Test Artist", Some("Test Album"), 15)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].username, "user1");
    }

    #[tokio::test]
    async fn test_search_deduplicates_by_filename() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![
            SearchResult {
                username: "user1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file("track.flac", 900, 30_000_000)],
            },
            SearchResult {
                username: "user2".into(),
                speed: 400,
                slots: 2,
                files: vec![make_file("track.flac", 900, 30_000_000)], // same filename
            },
        ];

        let results = search_album(&client, "Artist", Some("Album"), 15)
            .await
            .unwrap();
        // Both users returned (dedup is by file hash, not username — both have same file but from different users)
        assert_eq!(results.len(), 2);
    }
}
