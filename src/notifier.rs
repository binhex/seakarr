use reqwest::Client;
use serde::Serialize;

use crate::error::Result;

/// JSON payload posted to each configured notification webhook URL.
///
/// Built with `serde` rather than `serde_json::json!`, so the payload shape
/// lives in one derived struct instead of a macro invocation.
#[derive(Serialize)]
struct NotificationPayload {
    title: String,
    message: String,
    r#type: String,
}

/// POST the success payload to every configured webhook URL.
///
/// Only `http`/`https` URLs can be delivered; a URL with any other scheme is
/// skipped by `reqwest` and logged as a warning, and a failed delivery never
/// fails the run.
///
/// `destination` is the album's already-rendered final path, including the
/// `(kept in staging)` marker when the album deliberately stayed in staging, so
/// an alert says where the album actually landed.
pub async fn notify_success(
    urls: &[String],
    artist: &str,
    album: &str,
    track_count: usize,
    destination: &str,
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
        message: format!(
            "Downloaded \"{artist} — {album}\" ({track_count} tracks) to {destination}"
        ),
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
                tracing::warn!("Notification to {url} returned {}", resp.status());
            }
            Err(e) => {
                tracing::warn!("Failed to send notification to {url}: {e}");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, method, path};
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
        let result = notify_success(
            &urls,
            "Test Artist",
            "Test Album",
            3,
            "/media/Music/Test Artist/Test Album",
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_posts_the_documented_payload_shape() {
        // The README documents the body as {title, message, type}; a rename or a
        // switch to form encoding would break every configured webhook silently.
        // The destination the operator asked for rides in `message`, so this also
        // pins that the album folder reaches the payload.
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/notify"))
            .and(body_json(serde_json::json!({
                "title": "Seakarr — Download Complete",
                "message": "Downloaded \"Test Artist — Test Album\" (2 tracks) to /media/Music/Test Artist/Test Album",
                "type": "success",
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock_server)
            .await;

        notify_success(
            &[format!("{}/notify", mock_server.uri())],
            "Test Artist",
            "Test Album",
            2,
            "/media/Music/Test Artist/Test Album",
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_notify_accepts_an_undeliverable_scheme_without_failing_the_run() {
        // An Apprise-style scheme URL cannot be delivered because the payload is a
        // plain HTTP POST. The README promises the run still succeeds and that
        // each failure is logged, so assert both: the outcome alone would pass
        // even if such a URL were skipped with no log line at all.
        let capture = crate::test_support::LogCapture::start();
        let result = notify_success(
            &[
                "ntfy://my-topic".to_string(),
                "discord://id/token".to_string(),
            ],
            "Artist",
            "Album",
            1,
            "/media/Music/Test Artist/Test Album",
        )
        .await;
        assert!(
            result.is_ok(),
            "an undeliverable notification URL must not fail the run"
        );

        let logs = capture.text();
        let ntfy = logs
            .lines()
            .find(|line| line.contains("ntfy://my-topic"))
            .unwrap_or_else(|| panic!("the undeliverable URL was not logged, got:\n{logs}"));
        assert!(
            ntfy.contains(" WARN "),
            "the failure must be a warning, got: {ntfy}"
        );
        // Every configured URL is reported, and at WARN: a change that logged the
        // later ones at a lower level would otherwise keep this test green while
        // the operator's log lost the warning the README promises.
        let discord = logs
            .lines()
            .find(|line| line.contains("discord://id/token"))
            .unwrap_or_else(|| panic!("the second URL was not logged, got:\n{logs}"));
        assert!(
            discord.contains(" WARN "),
            "every URL must be reported at WARN, got: {discord}"
        );
    }

    #[tokio::test]
    async fn test_notify_empty_urls_is_noop() {
        let result = notify_success(
            &[],
            "Artist",
            "Album",
            1,
            "/media/Music/Test Artist/Test Album",
        )
        .await;
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
        let result = notify_success(
            &urls,
            "Artist",
            "Album",
            5,
            "/media/Music/Test Artist/Test Album",
        )
        .await;
        assert!(result.is_ok());
    }
}
