use reqwest::Client;
use serde::Serialize;

use crate::error::Result;

/// JSON payload posted to Apprise webhook URLs.
/// (Built with `serde` instead of `serde_json::json!` — serde_json is not a
/// direct dependency of this crate; the wire payload is identical.)
#[derive(Serialize)]
struct NotificationPayload {
    title: String,
    message: String,
    r#type: String,
}

/// Send success notification to all configured Apprise URLs.
pub async fn notify_success(
    urls: &[String],
    artist: &str,
    album: &str,
    track_count: usize,
) -> Result<()> {
    if urls.is_empty() {
        return Ok(());
    }

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| Client::new());
    let body = NotificationPayload {
        title: "Seakarr — Download Complete".into(),
        message: format!("Downloaded \"{artist} — {album}\" ({track_count} tracks)"),
        r#type: "success".into(),
    };

    for url in urls {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            continue;
        }

        match client.post(trimmed).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                tracing::warn!("Apprise notification to {url} returned {}", resp.status());
            }
            Err(e) => {
                tracing::warn!("Failed to send Apprise notification to {url}: {e}");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_notify_sends_payload() {
        // Start a mock HTTP server
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/notify"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let urls = vec![format!("{}/notify", mock_server.uri())];
        let result = notify_success(&urls, "Test Artist", "Test Album", 3).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_empty_urls_is_noop() {
        let result = notify_success(&[], "Artist", "Album", 1).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_multiple_urls() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(2) // Two URLs, two POSTs
            .mount(&mock_server)
            .await;

        let urls = vec![
            format!("{}/webhook1", mock_server.uri()),
            format!("{}/webhook2", mock_server.uri()),
        ];
        let result = notify_success(&urls, "Artist", "Album", 5).await;
        assert!(result.is_ok());
    }
}
