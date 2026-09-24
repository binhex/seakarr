use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::time::{timeout, Duration};

use indicatif::ProgressBar;

use crate::client::{DownloadHandle, DownloadStatus, FileInfo, SearchResult, SoulseekClient};
use crate::config::DownloadConfig;
use crate::discs;
use crate::error::{Result, SeakarrError};
use crate::filter;
use crate::formatting::format_speed;
use crate::progress::ProgressDisplay;

/// Sanitize a remote filename for local download: extract the basename
/// (the crate already strips directory components), reject path-traversal
/// patterns, and return a safe filename suitable for path construction.
pub(crate) fn safe_basename(remote_name: &str) -> Result<&str> {
    // Soulseek filenames may use either / or \\ as path separators
    // (many peers share from Windows machines).  Split on both and
    // take the last component.
    let basename = remote_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(remote_name);
    if basename.is_empty() || basename == "." || basename.contains("..") {
        return Err(SeakarrError::Download(format!(
            "unsafe or empty remote filename: {remote_name:?}"
        )));
    }
    Ok(basename)
}

/// Drain the status channel until the transfer terminates (Completed/Failed)
/// or the channel closes. Prevents `remove_dir_all` from racing the vendor
/// library's transfer thread after cancellation.
async fn drain_transfer(rx: &mut tokio::sync::mpsc::Receiver<DownloadStatus>, timeout_secs: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Some(DownloadStatus::Completed))
            | Ok(Some(DownloadStatus::Failed { .. }))
            | Ok(None) => return,
            _ => continue,
        }
    }
}

/// A single track's outcome, for peer-reputation recording.
#[derive(Debug, Default, Clone)]
pub struct TrackRecord {
    pub username: String,
    /// Effective transfer throughput in KiB/s (bytes downloaded ÷ transfer
    /// time, including retries, excluding queue wait); 0 when the track
    /// failed or produced no measurable speed sample.
    pub speed_kbps: f64,
    pub success: bool,
}

/// Per-track outcomes collected during an album download, consumed by the
/// runner to update peer reputation.
#[derive(Debug, Default, Clone)]
pub struct DownloadStats {
    pub tracks: Vec<TrackRecord>,
}

/// Download a single file from a specific user, monitoring speed.
///
/// Returns the destination path plus the effective transfer throughput in
/// KiB/s (bytes downloaded ÷ transfer time, excluding queue wait but
/// including retry delays). 0.0 is returned when no measurable transfer time
/// was observed (e.g. a zero-byte file or a transfer with no progress
/// samples), so the sample is excluded from the running average.
///
/// Retries the same peer up to `config.max_retries` times on failure,
/// waiting `config.retry_delay_secs` between attempts, before surfacing the
/// last error. A user-initiated cancellation (Ctrl+C) aborts at the next
/// safe point — between attempts or within the status polling loop — and
/// is never retried.
///
/// The progress bar is created lazily on the first `InProgress` status that
/// reports bytes — before that point no bar renders. Pass `None` for
/// `progress` to skip the bar entirely.
///
/// This entry point assumes the candidate has a free upload slot. Callers
/// that know the candidate's advertised slot count use
/// [`download_file_for_candidate`] so the queue policy can require a
/// position from candidates that have to wait.
#[allow(clippy::too_many_arguments)]
pub async fn download_file(
    client: &dyn SoulseekClient,
    file: &FileInfo,
    username: &str,
    dir: &Path,
    config: &DownloadConfig,
    filters: &crate::config::FilterConfig,
    progress: Option<&ProgressDisplay>,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<(PathBuf, f64)> {
    download_file_for_candidate(
        client, file, username, 1, dir, config, filters, progress, cancel,
    )
    .await
}

/// Download a single file from a specific candidate, applying the queue
/// policy while the peer holds it.
///
/// `peer_slots` is the candidate's advertised free upload slots from the
/// search result. With `max_queue_length > 0` it decides whether an attempt
/// must prove an in-bound queue position before its first progress event: a
/// zero-slot candidate is expected to queue and its position is enforced,
/// while a candidate with a free slot may start immediately. Waiting for the
/// queue head does not consume `timeout_secs`; `max_queue_time_secs` bounds
/// the total queue wait and `max_start_time_secs` bounds the wait from the
/// first observed position-1 state.
#[allow(clippy::too_many_arguments)]
pub async fn download_file_for_candidate(
    client: &dyn SoulseekClient,
    file: &FileInfo,
    username: &str,
    peer_slots: u8,
    dir: &Path,
    config: &DownloadConfig,
    filters: &crate::config::FilterConfig,
    progress: Option<&ProgressDisplay>,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<(PathBuf, f64)> {
    if config.max_queue_length == 0 && peer_slots == 0 {
        return Err(SeakarrError::QueueTimeout(format!(
            "{} from {username} requires an advertised free slot when max_queue_length=0",
            file.name
        )));
    }

    // Validate the remote name for traversal safety, but pass the FULL
    // share-relative path to the crate: the Soulseek QueueUpload wire
    // message must quote the path exactly as the peer shared it (e.g.
    // "Music\Artist\Album\01 - Track.flac"). Sending only the basename
    // makes every peer respond UploadDenied because it cannot find a
    // basename-only entry in its share list. The crate strips the path
    // itself when writing the local file, so the local destination is
    // still dir/<basename>.
    let basename = safe_basename(&file.name)?;

    // Retry the same peer up to max_retries times. On cancellation the
    // first attempt already aborts via the cancel flag inside download_once,
    // and the loop re-checks the flag between attempts so a SIGINT during
    // the delay window is honoured as soon as the sleep completes.
    //
    // NOTE: most failure types are retried, including non-transient ones
    // (e.g. "user declined", "could not connect"). This is intentional —
    // the retry_delay_secs penalty is the cost of a failed attempt, and
    // the candidate-list fallback provides the real diversity. Permanent
    // failures waste one delay window per retry, then fall back.
    //
    // The exceptions are failures the same peer+file cannot recover from.
    // A quality-verification rejection (SeakarrError::QualityRejected): the
    // file was downloaded and its bitrate/bitdepth found to be below the
    // configured minimum, and re-downloading it from the same peer cannot
    // change its quality. A below-floor speed abort (SeakarrError::
    // SlowDownload): the peer's measured rate stayed under
    // min_upload_speed_kbps past speed_check_wait_secs, and asking again only
    // puts the file back in that peer's queue — which is what produced the
    // "queued, started, retrying, queued" cycle on a starved peer. Retrying
    // either would only burn retry_delay_secs on a doomed attempt. Both
    // errors are returned immediately so download_album falls through to the
    // next ranked candidate, which may hold a better copy or serve faster.
    //
    // NOTE: with very low retry_delay_secs (< ~30 s), the vendor crate's
    // transfer thread may still be winding down (30 s socket read timeout)
    // when the retry opens a new connection to the same peer+file. Both
    // threads write to the same .part file (O_APPEND). The default 30 s
    // delay makes this negligible; lower values risk interleaved writes.
    //
    // The same window exists on the candidate-fallback path, where no retry
    // delay intervenes at all: a transfer the vendor thread finished just as
    // this attempt aborted can still rename its `.part` into a staging
    // directory `download_album` has already removed and another candidate now
    // owns. That race predates this loop — it applies to every non-retryable
    // abort (queue limits, quality rejection, below-floor speed) — and the
    // vendor's own read timeout is what bounds it.
    // Effective-transfer-time accumulator: the recorded throughput excludes
    // queue wait (a busy-but-fast peer is not demoted) but includes every
    // retry_delay_secs sleep and the final transfer's real duration, so a
    // peer that needs retries is demoted by the time it costs.
    let mut transfer_time = std::time::Duration::ZERO;
    let mut last_err: Option<SeakarrError> = None;
    for attempt in 0..=config.max_retries {
        if attempt > 0 {
            if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
                return Err(SeakarrError::Download("download cancelled by user".into()));
            }
            tracing::info!(
                "Retrying download of {basename} from {username} (attempt {attempt}/{})",
                config.max_retries
            );
            tokio::time::sleep(Duration::from_secs(config.retry_delay_secs)).await;
            transfer_time += Duration::from_secs(config.retry_delay_secs);
            // Re-check after the delay — a SIGINT during sleep must not
            // fall through to queueing another download.
            if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
                return Err(SeakarrError::Download("download cancelled by user".into()));
            }
        }
        match download_once(
            client, file, basename, username, peer_slots, dir, config, filters, progress, cancel,
        )
        .await
        {
            Ok((path, elapsed)) => {
                transfer_time += elapsed;
                // A transfer with no measurable duration (no InProgress sample,
                // e.g. a zero-byte file or a protocol edge) yields no speed
                // sample — record 0.0 so the DB's >0.0 guard excludes it,
                // rather than inflating the average via the 1ms floor.
                let throughput = if transfer_time.is_zero() {
                    0.0
                } else {
                    let bytes = std::fs::metadata(&path)
                        .map(|m| m.len())
                        .unwrap_or(file.size);
                    effective_throughput_kib_s(bytes, transfer_time)
                };
                return Ok((path, throughput));
            }
            Err(e) => {
                if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
                    return Err(e);
                }
                // Permanent failure (quality rejection, queue policy, or
                // below-floor speed): do not retry the same peer+file.
                if !is_retryable(&e) {
                    return Err(e);
                }
                last_err = Some(e);
            }
        }
    }
    // Unreachable in practice: the loop always executes at least once
    // (max_retries >= 0), so last_err is always set on this path.
    Err(last_err.unwrap())
}

/// Classify a download error for the per-peer retry loop. Three error kinds
/// are permanent: a quality rejection (the same file downloaded again from the
/// same peer has the same bitrate/bitdepth), a queue policy failure (the
/// peer's queue cannot improve by waiting again), and a below-floor speed
/// abort (the same peer+file cannot get faster, so re-requesting it only
/// re-enters that peer's queue). Every other failure (timeouts, refused
/// transfers, dropped connections) is treated as transient and retried.
fn is_retryable(error: &SeakarrError) -> bool {
    !matches!(
        error,
        SeakarrError::QualityRejected(_)
            | SeakarrError::QueueTimeout(_)
            | SeakarrError::SlowDownload(_)
    )
}

/// Update an exponential moving average (EMA) with a new sample.
///
/// Returns the updated EMA value. On the first call (`current` is `None`),
/// the new value is returned unsmoothed.
///
/// `alpha` controls responsiveness vs. smoothness:
/// - Higher alpha (e.g. 0.5) → faster response to real changes
/// - Lower alpha (e.g. 0.2) → smoother but slower to react
/// - Typical for speed display: 0.3 (responds within ~3-4 updates)
const SPEED_EMA_ALPHA: f64 = 0.3;

fn ema_update(current: Option<f64>, new_value: f64, alpha: f64) -> f64 {
    match current {
        Some(prev) => alpha * new_value + (1.0 - alpha) * prev,
        None => new_value,
    }
}

/// Effective transfer throughput: downloaded bytes over total wall-clock time,
/// in KiB/s. Elapsed is floored at 1 ms so a near-instant transfer can't
/// divide by zero.
fn effective_throughput_kib_s(bytes: u64, elapsed: std::time::Duration) -> f64 {
    let secs = elapsed.as_secs_f64().max(0.001);
    bytes as f64 / secs / 1024.0
}

/// How often the status channel is polled. Small enough that a cancellation
/// (Ctrl+C) is honoured promptly, large enough to keep the polling loop cheap
/// during a long queue wait.
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How long the queued notice waits for a queue position before it is emitted
/// without one.
///
/// The vendored client requests position telemetry immediately on enqueue and
/// the poll window is `STATUS_POLL_INTERVAL`, so a position normally arrives
/// within a few hundred milliseconds. The grace exists only so a peer that never
/// answers cannot leave the run silent — the defect this notice fixes.
const QUEUE_NOTICE_GRACE: Duration = Duration::from_secs(5);

/// How often a queued attempt re-asks its peer for a position.
///
/// The vendored crate's own interval is five minutes, which outlives the queue
/// cap this deployment runs (`max_queue_time_secs=300`), so a wait used to see
/// one position report and a bar frozen at it. Fresh positions also arm the
/// `max_start_time_secs` head deadline, which needs a position-1 report.
const QUEUE_POSITION_REFRESH: Duration = Duration::from_secs(30);

/// How long a transfer may take to start before it stops counting as immediate.
///
/// Deliberately the poll window rather than `QUEUE_NOTICE_GRACE`: a peer that
/// holds the transfer for seconds and never reports a position has not started
/// "immediately", and the grace is far too wide to claim otherwise — it exists
/// to bound how long the queued line waits, not to define immediacy.
const IMMEDIATE_START_WINDOW: Duration = STATUS_POLL_INTERVAL;

/// Which queue limit expired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueDeadlineKind {
    /// `max_queue_time_secs` — the total time the peer kept the file queued.
    TotalQueue,
    /// `max_start_time_secs` — the time since the peer first reported the
    /// file at queue position 1.
    QueueHead,
}

/// Build the queue deadline for a limit measured from `start`. Zero disables
/// the limit, so the deadline is `None` and the wait is unbounded.
fn enabled_deadline(start: tokio::time::Instant, seconds: u64) -> Option<tokio::time::Instant> {
    (seconds > 0)
        .then(|| start.checked_add(Duration::from_secs(seconds)))
        .flatten()
}

/// Pick the queue deadline that expires first, keeping its source so the
/// error can name the limit that was actually exceeded.
fn earliest_queue_deadline(
    total: Option<tokio::time::Instant>,
    head: Option<tokio::time::Instant>,
) -> Option<(tokio::time::Instant, QueueDeadlineKind)> {
    match (total, head) {
        (Some(total), Some(head)) if head < total => Some((head, QueueDeadlineKind::QueueHead)),
        (Some(total), _) => Some((total, QueueDeadlineKind::TotalQueue)),
        (None, Some(head)) => Some((head, QueueDeadlineKind::QueueHead)),
        (None, None) => None,
    }
}

/// The earlier of two optional deadlines; `None` means that limit is disabled.
fn earliest_deadline(
    first: Option<tokio::time::Instant>,
    second: Option<tokio::time::Instant>,
) -> Option<tokio::time::Instant> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (only, None) => only,
        (None, only) => only,
    }
}

/// Reject a positive peer-reported queue position above an active cap.
/// Cap-zero candidates have already proved a free slot before queueing and
/// are not retroactively rejected by later telemetry.
fn queue_position_rejection(position: u32, max_queue_length: u32) -> Option<String> {
    if max_queue_length > 0 && position > max_queue_length {
        Some(format!(
            "reported queue position {position}, exceeding max_queue_length={max_queue_length}"
        ))
    } else {
        None
    }
}

/// Ask the peer to cancel the transfer and wait for it to stop, so the
/// caller's staging cleanup cannot race a still-running transfer thread.
async fn cancel_and_drain(handle: &mut DownloadHandle) {
    let _ = handle.cancel_tx.send(()).await;
    drain_transfer(&mut handle.status_rx, 5).await;
}

fn cancellation_requested(cancel: Option<&Arc<AtomicBool>>) -> bool {
    cancel.is_some_and(|flag| flag.load(Ordering::SeqCst))
}

/// Clear the progress bar, cancel the in-flight transfer, and hand back the
/// error to return — the single exit path for every aborted attempt.
async fn stop_with_error(
    handle: &mut DownloadHandle,
    bar: &Option<ProgressBar>,
    error: SeakarrError,
) -> SeakarrError {
    if let Some(bar) = bar {
        bar.finish_and_clear();
    }
    cancel_and_drain(handle).await;
    error
}

/// Release a queue bar if one exists, counting the release through the display so
/// the "every attempt releases its bar" contract stays observable.
///
/// Every path that leaves the queue before the transfer starts calls this:
/// transfer start, position rejection, both fail-closed rejections, queue
/// deadline expiry, a stalled transfer that never started, channel close,
/// cancellation, a peer-side failure, and a completion that never sent
/// `InProgress`.
///
/// The two fail-closed rejections are defensive no-ops today: a queue bar exists
/// only once a positive in-bound position was observed, and that same block stores
/// the position, so their `queue.observed_position.is_none()` guard implies no bar.
/// They are kept so a future change that creates a bar earlier cannot leak one.
fn clear_queue_bar(progress: Option<&ProgressDisplay>, queue_bar: &mut Option<ProgressBar>) {
    if let (Some(display), Some(bar)) = (progress, queue_bar.take()) {
        display.clear_queue_bar(bar);
    }
}

/// End an attempt that received no status within `timeout_secs` of the last
/// accepted event.
///
/// Retryable, unlike a queue limit: a stall can be temporary, so the retry loop
/// (or the candidate fallback once retries are exhausted) is the existing answer
/// to it. Used from both arms that can observe the expiry: the loop top, when a
/// status already in the channel beat the poll timer, and the poll timeout.
async fn expire_stalled_transfer(
    handle: &mut DownloadHandle,
    bar: &Option<ProgressBar>,
    cancel: Option<&Arc<AtomicBool>>,
    basename: &str,
    timeout_secs: u64,
) -> SeakarrError {
    // A cancellation that landed during the final poll window outranks the
    // timeout, exactly as it does for both queue limits — and an attempt the
    // operator stopped must not be logged as a timeout.
    let error = if cancellation_requested(cancel) {
        SeakarrError::Download("download cancelled by user".into())
    } else {
        tracing::warn!("Download of {basename} timed out after {timeout_secs}s");
        SeakarrError::Download("download timed out".into())
    };
    stop_with_error(handle, bar, error).await
}

/// End a queued attempt whose queue deadline expired, naming the limit that
/// was exceeded. A cancellation that arrived in the meantime wins over the
/// expiry — Ctrl+C must always surface as a cancellation.
#[allow(clippy::too_many_arguments)]
async fn expire_queue_wait(
    handle: &mut DownloadHandle,
    bar: &Option<ProgressBar>,
    cancel: Option<&Arc<AtomicBool>>,
    kind: QueueDeadlineKind,
    basename: &str,
    username: &str,
    config: &DownloadConfig,
    observed_queue_position: Option<u32>,
) -> SeakarrError {
    let reason = match kind {
        QueueDeadlineKind::TotalQueue => format!(
            "{basename} from {username} exceeded max_queue_time_secs={} (last queue position: {})",
            config.max_queue_time_secs,
            observed_queue_position
                .map(|position| position.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
        ),
        QueueDeadlineKind::QueueHead => format!(
            "{basename} from {username} exceeded max_start_time_secs={} at queue position 1",
            config.max_start_time_secs,
        ),
    };
    let error = if cancellation_requested(cancel) {
        SeakarrError::Download("download cancelled by user".into())
    } else {
        tracing::warn!("Download queue timeout: {reason}");
        SeakarrError::QueueTimeout(reason)
    };
    stop_with_error(handle, bar, error).await
}

/// End a queued attempt rejected by the queue policy (a position the
/// configured `max_queue_length` does not allow, or a zero-slot candidate
/// that started without proving a position). A cancellation that arrived in
/// the meantime wins over the rejection.
async fn reject_queued_attempt(
    handle: &mut DownloadHandle,
    bar: &Option<ProgressBar>,
    cancel: Option<&Arc<AtomicBool>>,
    reason: String,
) -> SeakarrError {
    let error = if cancellation_requested(cancel) {
        SeakarrError::Download("download cancelled by user".into())
    } else {
        tracing::warn!("Download queue rejected: {reason}");
        SeakarrError::QueueTimeout(reason)
    };
    stop_with_error(handle, bar, error).await
}

/// Announce that a file has been queued.
///
/// Deferred rather than logged at enqueue time: the position is not known until
/// the peer answers, and one line carrying it is what the operator asked for. A
/// `None` position means the notice fired on the transfer-start or grace path.
fn emit_queue_notice(basename: &str, username: &str, position: Option<u32>) {
    match position {
        Some(position) => {
            tracing::info!("Download queued: {basename} from {username} - position {position}");
        }
        None => tracing::info!("Download queued: {basename} from {username}"),
    }
}

/// Announce that a queued file has started transferring, with the queue wait it
/// served.
///
/// The wait runs from enqueue to the peer's accept — that is when the queue
/// ended — while the line itself is emitted at the first byte-carrying progress
/// report, so the starved interval between the two is not counted as queueing.
/// The free-slot form is used when no position was ever reported and the
/// transfer began inside the immediacy window (`IMMEDIATE_START_WINDOW`), so it
/// never queued in any meaningful sense — the notice grace is a bound on how long
/// the queued line waits, not a definition of immediacy.
fn emit_download_started(
    basename: &str,
    username: &str,
    wait: Duration,
    last_position: Option<u32>,
) {
    if last_position.is_none() && wait <= IMMEDIATE_START_WINDOW {
        tracing::info!("Download started: {basename} from {username} immediately (free slot)");
        return;
    }
    // Reaching this form means either the wait exceeded the immediacy window, or a
    // position was observed — in which case the file genuinely queued and a
    // sub-second wait is still a wait. Reporting it as `0s` would read as "waited
    // nothing", so floor it at one second: `format_duration` works in whole
    // seconds.
    let reported = crate::formatting::format_duration(wait.max(Duration::from_secs(1)));
    match last_position {
        Some(position) => tracing::info!(
            "Download started: {basename} from {username} after {reported} queued (last position {position})"
        ),
        None => tracing::info!(
            "Download started: {basename} from {username} after {reported} queued"
        ),
    }
}

/// Queue-phase state of one attempt: everything the wait from enqueue to the
/// first byte depends on.
///
/// Extracted from `download_once` so each transition — the deferred notice, the
/// peer's position telemetry, the deadline selection and the queue-bar release —
/// is a small function of its own instead of another nesting level inside the
/// poll loop. Every method is synchronous and free of I/O, so the queue policy
/// can be tested without a client.
struct QueueWait {
    /// When the file was queued: the base for the total queue limit and for the
    /// wait reported in the started line.
    enqueued_at: tokio::time::Instant,
    /// When the deferred queued notice stops waiting for a position.
    notice_grace_deadline: tokio::time::Instant,
    /// Whether the notice has been emitted. Guards the grace, start and expiry
    /// paths so the line can never be duplicated.
    notice_emitted: bool,
    /// `max_queue_time_secs` deadline; `None` when the limit is disabled.
    total_deadline: Option<tokio::time::Instant>,
    /// First position-1 observation, which starts the `max_start_time_secs`
    /// clock only at that point.
    head_at: Option<tokio::time::Instant>,
    /// Last positive position the peer reported, for the timeout message.
    observed_position: Option<u32>,
    /// When the next on-demand position ask is due.
    next_position_request: tokio::time::Instant,
    /// Queue bar, created on the first position observation and released on
    /// every path out of the queue, so no attempt can leave a bar behind.
    bar: Option<ProgressBar>,
}

impl QueueWait {
    fn new(enqueued_at: tokio::time::Instant, config: &DownloadConfig) -> Self {
        Self {
            enqueued_at,
            notice_grace_deadline: enqueued_at + QUEUE_NOTICE_GRACE,
            notice_emitted: false,
            total_deadline: enabled_deadline(enqueued_at, config.max_queue_time_secs),
            head_at: None,
            observed_position: None,
            next_position_request: enqueued_at + QUEUE_POSITION_REFRESH,
            bar: None,
        }
    }

    /// Whether the notice grace has expired with no position, notice or start.
    fn notice_is_due(&self, now: tokio::time::Instant) -> bool {
        !self.notice_emitted && now >= self.notice_grace_deadline
    }

    /// Whether a position ask is due, advancing the deadline when it is.
    ///
    /// The same shape as `notice_is_due`: both are time-driven checks evaluated in
    /// the poll loop, so a busy status channel cannot starve them. Unlike the
    /// notice, this one is called after the loop's exit checks, so an attempt that
    /// is expiring this iteration is not asked at all, and it advances its
    /// deadline, so calling it twice at the same instant asks only once.
    fn position_request_is_due(&mut self, now: tokio::time::Instant) -> bool {
        if now < self.next_position_request {
            return false;
        }
        self.next_position_request = now + QUEUE_POSITION_REFRESH;
        true
    }

    /// Emit the queued notice without a position, once.
    ///
    /// Used where the wait ends without the peer ever reporting a position: the
    /// grace expiring, the transfer starting, a queue limit expiring, or a
    /// completion that skipped progress entirely.
    fn notice(&mut self, basename: &str, username: &str) {
        if !self.notice_emitted {
            emit_queue_notice(basename, username, None);
            self.notice_emitted = true;
        }
    }

    /// Record a reported queue position and emit or update what it drives.
    ///
    /// Returns the rejection reason when the position exceeds the configured
    /// `max_queue_length`, so the caller can end the attempt with it. A zero or
    /// absent position is ignored: neither proves a zero-slot peer is in bounds.
    fn observe(
        &mut self,
        position: Option<u32>,
        now: tokio::time::Instant,
        basename: &str,
        username: &str,
        config: &DownloadConfig,
        progress: Option<&ProgressDisplay>,
    ) -> Option<String> {
        let position = position.filter(|position| *position > 0)?;
        if let Some(detail) = queue_position_rejection(position, config.max_queue_length) {
            return Some(format!("{basename} from {username} {detail}"));
        }
        if !self.notice_emitted {
            emit_queue_notice(basename, username, Some(position));
            self.notice_emitted = true;
        } else if self.observed_position != Some(position) {
            tracing::debug!("Queue position for {basename} from {username}: {position}");
        }
        if let Some(display) = progress {
            let label = crate::progress::queue_label(basename, username, Some(position));
            match &self.bar {
                Some(existing) => display.update_queue_bar(existing, &label),
                None => self.bar = Some(display.create_queue_bar(&label)),
            }
        }
        self.observed_position = Some(position);
        if position == 1 && self.head_at.is_none() {
            self.head_at = Some(now);
        }
        None
    }

    /// The earliest enabled limit that bounds a wait for the transfer to start,
    /// with the limit that produced it.
    fn active_deadline(
        &self,
        config: &DownloadConfig,
    ) -> Option<(tokio::time::Instant, QueueDeadlineKind)> {
        let head_deadline = self
            .head_at
            .and_then(|head| enabled_deadline(head, config.max_start_time_secs));
        earliest_queue_deadline(self.total_deadline, head_deadline)
    }

    /// Release the queue bar, if this attempt created one.
    fn release_bar(&mut self, progress: Option<&ProgressDisplay>) {
        clear_queue_bar(progress, &mut self.bar);
    }

    /// The wait served since enqueue, as reported in the started line.
    fn wait(&self, now: tokio::time::Instant) -> std::time::Duration {
        now.duration_since(self.enqueued_at)
    }
}

/// Transfer-phase state of one attempt: the clock the inactivity and speed limits
/// are measured from, the bar that tracks the transfer, and the running speed
/// average that bar displays.
///
/// Like `QueueWait`, this exists so `download_once` stays a thin poll loop: each
/// transfer limit is applied by a small method that can be tested directly.
struct TransferProgress {
    /// Set by the first `InProgress` of any kind: the peer accepted the transfer,
    /// so the queue wait is over. This is the clock the recorded throughput is
    /// measured from (the accept-to-first-byte phase is transfer time, not queue
    /// time), and the base for the pre-start inactivity bound below.
    accepted_at: Option<tokio::time::Instant>,
    /// Set by the first progress sample that reports a positive
    /// `bytes_downloaded`. A fresh transfer's offset handshake reports zero
    /// bytes, so it cannot start the clock; a resumed transfer's handshake
    /// reports the surviving `.part` size (the vendor sends `part.written`),
    /// which does start it.
    started_at: Option<tokio::time::Instant>,
    /// `timeout_secs` deadline. Armed by every accepted `InProgress` — including
    /// the zero-byte offset handshake, which is what bounds a peer that accepts
    /// and then sends nothing — and reset by every progress sample. `None` when
    /// the limit is disabled.
    deadline: Option<tokio::time::Instant>,
    /// Whether a below-floor sample may end the attempt. Armed by the first
    /// sample *after* the one that started the transfer, so the zero-speed
    /// handshake echo that begins a resumed transfer is never judged against the
    /// floor (with `speed_check_wait_secs: 0` it would otherwise abort a healthy
    /// peer on the spot).
    floor_armed: bool,
    /// Last peer-reported total, so the bar can be snapped to 100% on
    /// completion (the final sample may lag the actual end).
    total_bytes: u64,
    /// EMA of the reported speed, so the displayed speed does not jump around.
    speed_ema: Option<f64>,
    /// Transfer bar, created on the first progress sample. Before the transfer
    /// starts no bar may render (a 0 B/[total] [0%] bar used to appear before
    /// the bridge even started).
    bar: Option<ProgressBar>,
}

impl TransferProgress {
    const fn new() -> Self {
        Self {
            accepted_at: None,
            started_at: None,
            deadline: None,
            floor_armed: false,
            total_bytes: 0,
            speed_ema: None,
            bar: None,
        }
    }

    const fn has_started(&self) -> bool {
        self.started_at.is_some()
    }

    /// When the peer accepted the transfer, if it has reported anything yet.
    const fn accepted_at(&self) -> Option<tokio::time::Instant> {
        self.accepted_at
    }

    /// Record that the peer accepted the transfer, and re-arm the inactivity
    /// deadline.
    ///
    /// Called for every `InProgress`. Before the byte-gated start this is the
    /// only limit that covers a peer which accepts and then goes silent: with
    /// both queue limits disabled, nothing else would ever end the attempt.
    fn note_accepted(&mut self, now: tokio::time::Instant, config: &DownloadConfig) {
        self.accepted_at.get_or_insert(now);
        self.deadline = enabled_deadline(now, config.timeout_secs);
    }

    /// Start the transfer clock and create the bar for a non-empty file.
    ///
    /// Zero-length transfers get no bar: indicatif renders `len == 0` as 100%
    /// (state.rs:282), which reproduces the bug this creation point exists to fix.
    /// A file small enough that the peer never reports progress (its reports are
    /// one per ~120 KB of reads) also completes without one — the queue bar is
    /// released by the completion path and nothing is left on the terminal. The
    /// peer reports one fixed total for a transfer, so a transfer that starts
    /// without a total never gains a bar later.
    fn start(
        &mut self,
        now: tokio::time::Instant,
        total_bytes: u64,
        basename: &str,
        progress: Option<&ProgressDisplay>,
    ) {
        self.started_at = Some(now);
        self.floor_armed = false;
        if total_bytes > 0 {
            if let Some(display) = progress {
                self.bar = Some(display.create_bar(basename, total_bytes));
            }
        }
    }

    /// Apply one progress sample: reset the inactivity deadline, enforce the
    /// speed floor on the smoothed rate once the wait period has passed, and
    /// update the bar.
    ///
    /// The sample that started the transfer is exempt from both the floor and
    /// the average (see `floor_armed`). The returned error ends the attempt: the
    /// transfer is cancelled and drained first, because `download_album` removes
    /// the staging directory immediately afterwards and must not race the
    /// transfer thread.
    #[allow(clippy::too_many_arguments)]
    async fn sample(
        &mut self,
        handle: &mut DownloadHandle,
        cancel: Option<&Arc<AtomicBool>>,
        now: tokio::time::Instant,
        config: &DownloadConfig,
        speed_bytes_per_sec: u64,
        bytes_downloaded: u64,
        total_bytes: u64,
    ) -> Result<()> {
        // Any accepted `InProgress` — including the zero-byte offset handshake —
        // arms and re-arms `timeout_secs`, so queue wait never counts against it.
        // As with `max_queue_length` and `max_start_time_secs`, `0` disables the
        // inactivity timeout instead of expiring immediately.
        self.deadline = enabled_deadline(now, config.timeout_secs);
        self.total_bytes = total_bytes;
        // Smooth the reported rate, then judge the smoothed value: the floor is
        // applied to the same average the bar displays, so a jittery ~120 KB
        // window cannot abandon an otherwise-fast peer once the average is
        // seeded. A peer that is slow from the start is still caught by its first
        // judged sample, whatever that value blends with by then.
        //
        // The sample that started the transfer is kept out of the average, not
        // just out of the verdict: a resumed transfer's handshake reports the
        // `.part` size with a zero speed, and seeding the average with that zero
        // would hold every later sample down (0.7^k of it each time), so the
        // first judged sample would be measured at a fraction of the peer's real
        // rate. The first judged sample therefore seeds the average unsmoothed,
        // which still rejects a peer that is slow from the start.
        let smoothed = if self.floor_armed {
            let smoothed = ema_update(self.speed_ema, speed_bytes_per_sec as f64, SPEED_EMA_ALPHA);
            self.speed_ema = Some(smoothed);
            smoothed
        } else {
            speed_bytes_per_sec as f64
        };
        // Speed check: only once the transfer has actually started transferring
        // (not just been accepted), and past the wait period. The wait is measured
        // against the caller's sample instant, which is the same `now` used for
        // the inactivity deadline above, so both limits read one clock.
        if self.floor_armed {
            if let Some(started_at) = self.started_at {
                if config.min_upload_speed_kbps > 0
                    && now.duration_since(started_at).as_secs() >= config.speed_check_wait_secs
                {
                    let speed_kbps = (smoothed / 1024.0) as u32;
                    if speed_kbps < config.min_upload_speed_kbps {
                        // A cancellation that landed while the poll was pending
                        // outranks the verdict, exactly as it does for both queue
                        // limits: the operator asked to stop, and the surviving
                        // error has to say so.
                        let error = if cancellation_requested(cancel) {
                            SeakarrError::Download("download cancelled by user".into())
                        } else {
                            SeakarrError::SlowDownload(format!(
                                "speed {speed_kbps} KB/s below minimum {} KB/s",
                                config.min_upload_speed_kbps
                            ))
                        };
                        return Err(stop_with_error(handle, &self.bar, error).await);
                    }
                }
            }
        }
        self.floor_armed = true;
        // Update progress bar if present with the same smoothed speed.
        if let Some(bar) = &self.bar {
            bar.set_position(bytes_downloaded);
            bar.set_prefix(format_speed(smoothed as u64));
        }
        Ok(())
    }

    /// Whether no status arrived within `timeout_secs` of the last accepted
    /// event — the last progress sample, or the peer's acceptance when the
    /// transfer has not produced a byte yet.
    fn is_inactive(&self, now: tokio::time::Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }

    /// Snap the bar to the peer's last reported total and remove it.
    ///
    /// `finish_and_clear` removes the bar from the terminal so the next track
    /// starts with a single bar (no "double progress bar" effect).
    fn complete(&self) {
        if let Some(bar) = &self.bar {
            bar.set_position(self.total_bytes);
            bar.finish_and_clear();
        }
    }

    /// Remove the bar after a failed attempt.
    fn clear(&self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
    }

    /// The transfer duration, or zero when the peer never reported anything.
    ///
    /// Measured from the accept, not from the first byte: the phase between the
    /// peer accepting and its first progress sample is transfer time, not queue
    /// wait, so it belongs in the recorded throughput. A completed transfer that
    /// sent no `InProgress` at all still records zero.
    fn elapsed(&self) -> std::time::Duration {
        self.accepted_at
            .map(|accepted_at| accepted_at.elapsed())
            .unwrap_or(std::time::Duration::ZERO)
    }
}

/// Single download attempt for `download_file_for_candidate` (no retry loop).
/// Queues the transfer and polls status until success, failure, timeout, or
/// cancel. Returns the destination path plus the transfer duration (the peer's
/// accept to completion). The `basename` parameter must be pre-validated by the
/// caller.
///
/// Queue wait and transfer inactivity are timed separately: `timeout_secs` is
/// armed by the peer's accept and by every later `InProgress`, and is not reset
/// by a paused status, while the queue wait is bounded by `max_queue_time_secs`
/// (from enqueue) and `max_start_time_secs` (from the first position-1
/// observation). Observed queue positions are validated against
/// `max_queue_length`, and a zero-slot candidate with a positive cap must prove
/// an in-bound position before its first byte-carrying progress event.
#[allow(clippy::too_many_arguments)]
async fn download_once(
    client: &dyn SoulseekClient,
    file: &FileInfo,
    basename: &str,
    username: &str,
    peer_slots: u8,
    dir: &Path,
    config: &DownloadConfig,
    filters: &crate::config::FilterConfig,
    progress: Option<&ProgressDisplay>,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<(PathBuf, std::time::Duration)> {
    let mut handle = match client.download(file, username, dir).await {
        Ok(h) => h,
        Err(e) => return Err(e),
    };
    let mut queue = QueueWait::new(tokio::time::Instant::now(), config);
    let mut transfer = TransferProgress::new();
    // A zero-slot candidate with a positive cap is expected to queue, so it
    // must prove an in-bound position before it may start transferring.
    let requires_queue_position = config.max_queue_length > 0 && peer_slots == 0;

    loop {
        // Honour cancellation (Ctrl+C / SIGINT) first: it outranks every
        // queue and transfer deadline. The caller (download_album) removes
        // the staging dir.
        if cancellation_requested(cancel) {
            queue.release_bar(progress);
            return Err(stop_with_error(
                &mut handle,
                &transfer.bar,
                SeakarrError::Download("download cancelled by user".into()),
            )
            .await);
        }
        let now = tokio::time::Instant::now();
        // The notice grace is evaluated here, at the top of the loop, and not
        // only when the status poll times out. A peer that answers position-0
        // more often than the poll window (each reply maps to
        // `Queued { queue_position: None }`) keeps the poll alive, so a check
        // inside the timeout arm can be starved and the queued line never
        // emitted — the silence this notice exists to remove.
        if queue.notice_is_due(now) {
            queue.notice(basename, username);
        }
        // While queued, the earliest of the total queue limit and the queue-head
        // limit bounds the wait. Once the transfer has started, only transfer
        // inactivity matters.
        let active_queue_deadline = queue.active_deadline(config);
        // While the transfer has not started, the queue limits bound the wait —
        // and so does `timeout_secs` measured from the peer's accept, armed by
        // `note_accepted`. With both queue limits disabled that accept clock is
        // the only bound a peer which accepts and then goes silent has.
        let active_deadline = if transfer.has_started() {
            transfer.deadline
        } else {
            earliest_deadline(
                active_queue_deadline.map(|(deadline, _)| deadline),
                transfer.deadline,
            )
        };
        // The accept clock can expire with no status pending, so it is checked
        // here as well as in the poll-timeout arm: a status that was already in
        // the channel beats the poll timer, and without this the expiry would be
        // delayed by another poll window.
        if transfer.is_inactive(now) {
            queue.release_bar(progress);
            return Err(expire_stalled_transfer(
                &mut handle,
                &transfer.bar,
                cancel,
                basename,
                config.timeout_secs,
            )
            .await);
        }
        // A deadline that a status already in the channel beat the poll timer to.
        // Reachable because `timeout` polls the channel before its timer: a
        // message is handled, and the loop top then observes the expiry. Without
        // this the expiry would swallow the queued line. Both the notice and the
        // expiry are idempotent, and the arm returns.
        //
        // Coverage limitation: which arm observes an expiry that lands exactly on
        // a status arrival is scheduler-dependent, so this arm is not
        // deterministically testable. It is kept as defence for the deadline the
        // poll-timeout arm would otherwise swallow, and both lines are
        // deliberately cheap and idempotent.
        if let Some((deadline, kind)) = active_queue_deadline.filter(|_| !transfer.has_started()) {
            if now >= deadline {
                queue.notice(basename, username);
                queue.release_bar(progress);
                return Err(expire_queue_wait(
                    &mut handle,
                    &transfer.bar,
                    cancel,
                    kind,
                    basename,
                    username,
                    config,
                    queue.observed_position,
                )
                .await);
            }
        }
        // The position ask is a time-driven check that has to precede the poll, so
        // a peer answering position-0 more often than the poll window cannot
        // starve it. It sits after the exit checks above so an attempt that is
        // expiring this iteration is not asked at all, which is what makes "no ask
        // after a queue timeout" literally true. The vendored crate asks once at
        // enqueue, so this is a refresh; the transfer-started gate is load-bearing
        // because this loop keeps polling for progress after the transfer starts.
        if !transfer.has_started() && queue.position_request_is_due(now) {
            tracing::debug!("Queue position request for {basename} from {username}");
            // Best-effort: a peer with no peer actor to ask answers false, and the
            // queue limits still bound the wait.
            let _ = client.request_queue_position(username, &file.name).await;
        }
        // Poll status with a short timeout so cancellation and deadline
        // expiry are checked frequently, and never later than the active
        // deadline.
        let poll_timeout = active_deadline
            .map(|deadline| deadline.saturating_duration_since(now))
            .unwrap_or(STATUS_POLL_INTERVAL)
            .min(STATUS_POLL_INTERVAL);
        let msg = timeout(poll_timeout, handle.status_rx.recv()).await;

        match msg {
            Ok(Some(DownloadStatus::InProgress {
                speed_bytes_per_sec,
                bytes_downloaded,
                total_bytes,
            })) => {
                let now = tokio::time::Instant::now();
                // Any `InProgress` proves the peer accepted the transfer, so the
                // accept clock starts (or is re-armed) here — before the
                // byte-gated start block, which only the handshake's successors
                // can reach.
                transfer.note_accepted(now, config);
                // Fail closed: a zero-slot candidate with a positive cap must
                // not start on an unproven position, or a peer could keep the
                // file queued and then start it outside the configured policy.
                if !transfer.has_started()
                    && requires_queue_position
                    && queue.observed_position.is_none()
                {
                    let reason = format!(
                        "{basename} from {username} started with an unknown queue position while max_queue_length={}",
                        config.max_queue_length
                    );
                    queue.release_bar(progress);
                    return Err(
                        reject_queued_attempt(&mut handle, &transfer.bar, cancel, reason).await,
                    );
                }
                // The peer's offset handshake reports the resume offset with a
                // zero speed the moment it accepts the transfer, before a byte
                // is read (download_peer.rs `start_transfer`). Only a report
                // that carries bytes is real progress: counting the echo as the
                // start began the speed clock while the peer was still draining
                // its own queue, so a starved peer was cancelled for being too
                // slow and then retried. Until bytes arrive, the queue limits
                // stay in charge of the wait.
                if !transfer.has_started() && bytes_downloaded > 0 {
                    queue.release_bar(progress);
                    queue.notice(basename, username);
                    // The wait reported is the queue wait, and that ends when the
                    // peer accepts rather than when its first bytes arrive: a
                    // peer that accepted at once but needed a moment for its
                    // first ~120 KB report is still reported as immediate, and
                    // the starved interval shows up in whatever ends the attempt.
                    emit_download_started(
                        basename,
                        username,
                        queue.wait(transfer.accepted_at().unwrap_or(now)),
                        queue.observed_position,
                    );
                    transfer.start(now, total_bytes, basename, progress);
                }
                // Everything measured from the transfer start — the inactivity
                // deadline, the speed floor and the progress bar — runs only once
                // real bytes have arrived, so a queued peer is never charged
                // against them.
                if transfer.has_started() {
                    transfer
                        .sample(
                            &mut handle,
                            cancel,
                            now,
                            config,
                            speed_bytes_per_sec,
                            bytes_downloaded,
                            total_bytes,
                        )
                        .await?;
                }
            }
            Ok(Some(DownloadStatus::Completed)) => {
                // Same fail-closed rule as the first progress event: an
                // attempt that completes without ever proving its position
                // must not be accepted. Free-slot candidates are unaffected
                // and keep the zero-throughput path below.
                if !transfer.has_started()
                    && requires_queue_position
                    && queue.observed_position.is_none()
                {
                    let reason = format!(
                        "{basename} from {username} completed without a queue position while max_queue_length={}",
                        config.max_queue_length
                    );
                    queue.release_bar(progress);
                    return Err(
                        reject_queued_attempt(&mut handle, &transfer.bar, cancel, reason).await,
                    );
                }
                // A transfer can reach Completed without ever sending InProgress.
                // It never started, so the started line is emitted here to keep
                // the "one started line per downloaded file" contract. The queued
                // notice goes with it: this arm is the only chance to report the
                // wait, because it returns immediately below.
                if !transfer.has_started() {
                    queue.notice(basename, username);
                    emit_download_started(
                        basename,
                        username,
                        queue.wait(transfer.accepted_at().unwrap_or(now)),
                        queue.observed_position,
                    );
                }
                // The same short-circuit applies to the queue bar: the
                // transfer-start release is never reached, and the bar holds a
                // steady tick, so leaving it would keep a spinner on the terminal
                // for the rest of the run.
                queue.release_bar(progress);
                transfer.complete();
                let dest = dir.join(basename);
                tracing::info!("Download staged: {basename} -> {}", dest.display());

                // Post-download quality verification: when min_bit_rate or
                // min_bit_depth is set and the peer did not provide the
                // metadata in the search result, verify the actual file
                // quality (lofty) before accepting it. A verification
                // failure is treated like any other download failure — the
                // caller (download_album) cleans up the staging dir and
                // tries the next candidate.
                if let Err(e) = verify_downloaded_quality(&dest, file, filters) {
                    tracing::warn!("Quality verification failed for {basename}: {e}");
                    let _ = std::fs::remove_file(&dest);
                    return Err(e);
                }

                return Ok((dest, transfer.elapsed()));
            }
            Ok(Some(DownloadStatus::Failed { reason })) => {
                tracing::warn!("Download of {basename} failed: {reason}");
                queue.release_bar(progress);
                transfer.clear();
                return Err(SeakarrError::Download(format!("transfer failed: {reason}")));
            }
            Ok(Some(DownloadStatus::Queued { queue_position })) => {
                // Positions are only meaningful before the transfer starts: a
                // late Queued must not re-enter queue state or reset any deadline.
                if !transfer.has_started() {
                    let now = tokio::time::Instant::now();
                    let rejected =
                        queue.observe(queue_position, now, basename, username, config, progress);
                    if let Some(reason) = rejected {
                        queue.release_bar(progress);
                        return Err(reject_queued_attempt(
                            &mut handle,
                            &transfer.bar,
                            cancel,
                            reason,
                        )
                        .await);
                    }
                }
            }
            // A pause is not progress: it neither starts nor resets the
            // transfer inactivity deadline, and it does not re-enter queue
            // state once the transfer has started.
            Ok(Some(DownloadStatus::Paused { .. })) => {}
            Ok(None) => {
                transfer.clear();
                tracing::warn!("Download channel closed for {basename}");
                queue.release_bar(progress);
                return Err(SeakarrError::Download(
                    "download channel closed unexpectedly".into(),
                ));
            }
            Err(_elapsed) => {
                // The poll window expired — check the active deadline before
                // declaring the attempt dead. Only the deadline that applies to
                // the current phase can expire.
                let now = tokio::time::Instant::now();
                if transfer.is_inactive(now) {
                    // The queue bar survives until the first byte, so this
                    // pre-start exit must release it: the Err arm is the usual
                    // observer for a peer that accepts and then goes silent.
                    queue.release_bar(progress);
                    return Err(expire_stalled_transfer(
                        &mut handle,
                        &transfer.bar,
                        cancel,
                        basename,
                        config.timeout_secs,
                    )
                    .await);
                }
                if let Some((deadline, kind)) =
                    active_queue_deadline.filter(|_| !transfer.has_started())
                {
                    if now >= deadline {
                        // A queue limit shorter than the notice grace expires
                        // first. Emitting the notice here keeps the wait visible
                        // instead of letting the expiry swallow it, so the queued
                        // line is never skipped whatever the limits are. The notice
                        // carries no position by design: a stored position implies
                        // the notice was already emitted, and a position that did
                        // arrive reaches the `QueueTimeout` warning instead.
                        queue.notice(basename, username);
                        queue.release_bar(progress);
                        return Err(expire_queue_wait(
                            &mut handle,
                            &transfer.bar,
                            cancel,
                            kind,
                            basename,
                            username,
                            config,
                            queue.observed_position,
                        )
                        .await);
                    }
                }
                // Still within the deadline — continue polling.
            }
        }
    }
}

/// Verify that a downloaded file meets the configured quality requirements.
/// Called after download completes when `min_bit_rate` or `min_bit_depth` is
/// set and the peer did not provide the metadata in the search result.
///
/// Returns Ok(()) when the file passes, when verification is not needed
/// (no minimum configured, or the peer already provided the metadata and
/// the pre-download filter checked it), or when the file cannot be parsed
/// (skip — can't verify what we can't read). Returns Err when the actual
/// quality is below the configured minimum.
pub(crate) fn verify_downloaded_quality(
    path: &Path,
    file: &FileInfo,
    filters: &crate::config::FilterConfig,
) -> Result<()> {
    // Bitrate check (lossy files only). The peer didn't provide bitrate
    // (attribs key 0) — read the actual bitrate from the file.
    if filters.min_bit_rate > 0 && !file.attribs.contains_key(&0) {
        if let Some(actual_br) = crate::organizer::extract_bitrate(path) {
            if actual_br < filters.min_bit_rate {
                return Err(SeakarrError::QualityRejected(format!(
                    "bitrate {actual_br} kbps below minimum {} kbps",
                    filters.min_bit_rate
                )));
            }
        }
        // extract_bitrate None → unparseable or lossless: skip.
    }

    // Bitdepth check (lossless files only). The peer didn't provide
    // bitdepth (attribs key 5) — read the actual bit depth from the file.
    if filters.min_bit_depth > 0 && !file.attribs.contains_key(&5) {
        if let Some(actual_bd) = crate::organizer::extract_bitdepth(path) {
            if actual_bd < filters.min_bit_depth {
                return Err(SeakarrError::QualityRejected(format!(
                    "bitdepth {actual_bd} below minimum {}",
                    filters.min_bit_depth
                )));
            }
        }
        // extract_bitdepth None → unparseable or lossy: skip.
    }

    Ok(())
}

/// Group files by their share-relative parent directory path and return
/// the largest group (most files). A peer's search result can span
/// multiple album directories when the query matches several of their
/// albums; downloading all of them would mix tracks from different albums
/// into one staging folder. Files at the share root (no parent component)
/// all belong to a single "<root>" group.
///
/// The key is the full parent path (not just the immediate directory name),
/// so `Abba\Greatest Hits\...` and `Bee Gees\Greatest Hits\...` form
/// separate groups despite sharing the leaf directory name "Greatest Hits".
///
/// Ties are broken by lexicographic key order — `max_by` keeps the
/// lexicographically larger key — so the same album wins on every run.
pub(crate) fn largest_album_group<'a>(files: &[&'a FileInfo]) -> Vec<&'a FileInfo> {
    let mut groups: std::collections::HashMap<String, Vec<&'a FileInfo>> = Default::default();
    for f in files {
        // rsplit_once gives the full parent path (everything before the
        // last separator), not just the immediate directory name.
        let parent = f
            .name
            .rsplit_once(['/', '\\'])
            .map(|(parent, _basename)| parent)
            .filter(|p| !p.is_empty())
            .unwrap_or("<root>");
        // Strip disc/CD designators so the discs of a multi-disc album
        // collapse into a single group (CD 01 + CD 02 of the same album
        // download together). Different albums keep distinct keys.
        let key = album_group_key(parent);
        groups.entry(key).or_default().push(f);
    }
    // Deterministic tie-breaking: prefer the larger group; on equal size the
    // lexicographically larger key wins, because `max_by` keeps the greater of
    // two distinct keys. Either key would do; what matters is that the same
    // album is chosen on every run.
    groups
        .into_iter()
        .max_by(|(ak, av), (bk, bv)| av.len().cmp(&bv.len()).then_with(|| ak.cmp(bk)))
        .map(|(_, v)| v)
        .unwrap_or_default()
}

/// Grouping key for a file's parent directory, with trailing disc/CD
/// designators removed so the discs of a multi-disc album collapse into a
/// single group. Handles both dedicated disc subfolders ("...\\CD 01") and
/// embedded markers in the album folder name ("Gold (Disc 1)"). Only the
/// disc designator is stripped — a genuinely different album name keeps a
/// distinct key and is never merged in.
fn album_group_key(parent: &str) -> String {
    // Split the parent into (grandparent, leaf) on the last separator.
    let Some(sep_idx) = parent.rfind(['/', '\\']) else {
        return parent.to_string();
    };
    let (grandparent, leaf) = (&parent[..sep_idx], &parent[sep_idx + 1..]);
    let sep = &parent[sep_idx..sep_idx + 1];

    // Dedicated disc folder as the leaf, e.g. "...\\CD 01": key = grandparent.
    if discs::is_disc_folder(leaf) {
        return grandparent.to_string();
    }
    // Embedded disc marker in the leaf, e.g. "Gold (Disc 1)": key = parent
    // with the marker stripped from the leaf.
    if let Some(stripped) = discs::strip_embedded_disc_marker(leaf) {
        return format!("{grandparent}{sep}{stripped}");
    }

    parent.to_string()
}

/// Download all files for an album from the best candidate, with fallback.
/// Tries each candidate in ranked order until one succeeds (or all fail).
/// Only downloads files that are safe (no path traversal) and pass the
/// configured extension filters.
#[allow(clippy::too_many_arguments)]
pub async fn download_album(
    client: &dyn SoulseekClient,
    candidates: &[SearchResult],
    staging_dir: &Path,
    config: &DownloadConfig,
    filters: &crate::config::FilterConfig,
    progress: Option<&ProgressDisplay>,
    cancel: Option<&Arc<AtomicBool>>,
    stats: &mut DownloadStats,
) -> Result<Vec<PathBuf>> {
    let mut last_err: Option<SeakarrError> = None;

    // Try each candidate in ranked order; staging dir is created on demand
    // only when a candidate has valid files to download.
    for candidate in candidates {
        // Check cancellation between candidates — avoid queuing network
        // requests to the next peer after the user pressed Ctrl+C.
        // Clean the staging dir before returning so no partial downloads
        // are left behind (same cleanup as the post-loop failure path).
        if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            // Remove staging dir if it was created; suppress ENOENT
            // (dir may not exist yet if cancel fires before first download).
            match std::fs::remove_dir_all(staging_dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!("Failed to clean staging dir on cancel {staging_dir:?}: {e}")
                }
            }
            return Err(SeakarrError::Download("download cancelled by user".into()));
        }

        let all_passed = candidate
            .files
            .iter()
            .filter(|f| safe_basename(&f.name).is_ok() && filter::file_passes_filters(f, filters))
            .count();
        if all_passed == 0 {
            // Show a sample of what we're rejecting to help debug
            for f in candidate.files.iter().take(3) {
                let ext = f.name.rsplit('.').next().unwrap_or("<none>");
                let safe = safe_basename(&f.name).is_ok();
                let passes = filter::file_passes_filters(f, filters);
                tracing::warn!(
                    "reject: {:?} ext={ext} safe={safe} passes={passes} bitrate={bitrate:?}",
                    f.name,
                    bitrate = f.attribs.get(&0),
                );
            }
        }
        let filtered_files: Vec<&FileInfo> = candidate
            .files
            .iter()
            .filter(|f| safe_basename(&f.name).is_ok() && filter::file_passes_filters(f, filters))
            .collect();

        if filtered_files.is_empty() {
            last_err = Some(SeakarrError::Download(
                "candidate had no safe files to download".into(),
            ));
            continue;
        }

        // A peer's search result may span multiple album directories (e.g.
        // "Abba\\[1992] Gold_ Greatest Hits\\..." and
        // "Abba\\[1993] More ABBA Gold\\..." when both match the query).
        // Downloading every matching file would mix tracks from different
        // albums into the single staging folder. Group by parent directory
        // and keep only the largest group — the album the query most likely
        // targeted — so one run downloads a single album per peer.
        let filtered_files = largest_album_group(&filtered_files);

        // Create the staging directory only when we have valid files to
        // download — prevents empty dirs for albums where every candidate
        // fails the safe_basename / file_passes_filters checks.
        std::fs::create_dir_all(staging_dir)?;

        let mut downloaded = Vec::new();
        let mut failed = false;

        for file in &filtered_files {
            // Isolate each disc of a multi-disc album into its own staging
            // subdirectory so same-named tracks across discs (e.g.
            // "01 - Track.flac" on both disc 1 and disc 2) never collide in
            // one flat staging dir — whichever file landed second would
            // otherwise overwrite the first before either reaches the
            // library. Covers dedicated disc folders ("CD 01", "Disc 2")
            // AND album folders with an embedded disc marker ("Gold (Disc
            // 1)" / "Gold (Disc 2)"), matching the album_group_key merge
            // that brings the discs together.
            let disc_leaf = file
                .name
                .rsplit_once(['/', '\\'])
                .and_then(|(parent, _basename)| {
                    parent.rsplit_once(['/', '\\']).map(|(_, leaf)| leaf)
                })
                .unwrap_or("");
            // Only create a disc subdirectory when the leaf designates a
            // single disc of a multi-disc album. For albums without disc
            // folders the leaf is the album name and the staging dir is
            // already correct — no extra nesting needed.
            let disc_dir = if !disc_leaf.is_empty() && discs::is_disc_designator(disc_leaf) {
                staging_dir.join(disc_leaf)
            } else {
                staging_dir.to_path_buf()
            };
            std::fs::create_dir_all(&disc_dir)?;

            match download_file_for_candidate(
                client,
                file,
                &candidate.username,
                candidate.slots,
                &disc_dir,
                config,
                filters,
                progress,
                cancel,
            )
            .await
            {
                Ok((path, speed_kbps)) => {
                    downloaded.push(path);
                    stats.tracks.push(TrackRecord {
                        username: candidate.username.clone(),
                        speed_kbps,
                        success: true,
                    });
                }
                Err(e) => {
                    // download_file_for_candidate already retried this file
                    // on the same peer up to max_retries times (with
                    // retry_delay_secs between attempts). A failure here means
                    // those retries were exhausted, so fall back to the next
                    // ranked candidate — the candidate list is the outer
                    // fallback.
                    //
                    // Debug, not warn: the album outcome is reported once
                    // (this function's error, then the runner's album line and
                    // the run summary), so a warning here would multiply one
                    // failure across every peer that was tried.
                    tracing::debug!(
                        "Download of {} from {} failed after retries: {e}",
                        file.name,
                        candidate.username
                    );
                    stats.tracks.push(TrackRecord {
                        username: candidate.username.clone(),
                        speed_kbps: 0.0,
                        success: false,
                    });
                    last_err = Some(e);
                    failed = true;
                    break; // Move to next candidate
                }
            }
        }

        if !failed {
            return Ok(downloaded);
        }

        // The album's staging directory is per-album (created by the runner).
        // A failed candidate must leave no staging directory at all —
        // the album is either fully downloaded (directory exists with files)
        // or absent (failed/never attempted). This removes completed tracks,
        // `.part` files, and the directory itself. Retry once after a brief
        // pause to handle transient locks from a just-cancelled transfer.
        for attempt in 0..2 {
            match std::fs::remove_dir_all(staging_dir) {
                Ok(()) => break,
                Err(e) if attempt == 0 => {
                    tracing::warn!(
                        "Failed to clean up staging directory {staging_dir:?} (retrying): {e}"
                    );
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(e) => {
                    tracing::warn!("Failed to clean up staging directory {staging_dir:?}: {e}");
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| SeakarrError::Download("all candidates exhausted".into())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{DownloadHandle, FileInfo, MockClient, SearchResult};
    use crate::config::{DownloadConfig, FilterConfig};
    use crate::test_support::{make_file, write_minimal_flac};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    fn default_dl_config() -> DownloadConfig {
        DownloadConfig {
            concurrent: 5,
            max_queue_length: 0,
            max_start_time_secs: 120,
            max_queue_time_secs: 1800,
            min_upload_speed_kbps: 0, // disabled for test
            speed_check_wait_secs: 0, // immediate for test
            timeout_secs: 180,
            max_download_time_mins: 120,
            max_retries: 2,
            retry_delay_secs: 0,
            min_filtered_users: 1,
            skip_retry_hours: 24,
        }
    }

    /// Filter config with the min_tracks gate disabled — these focused
    /// download tests use small mock shares and exercise transfer/cleanup
    /// logic, not share completeness (covered in filter.rs).
    fn default_filter_config_test() -> FilterConfig {
        FilterConfig {
            min_tracks: 0,
            ..FilterConfig::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_candidate_with_only_unsafe_paths_is_skipped() {
        // Every file in this candidate has a basename that escapes the download
        // directory, so the candidate is abandoned before any request is queued,
        // and the run reports the reason instead of downloading nothing quietly.
        let client = ScriptedClient::new(vec![]);
        let dir = TempDir::new().unwrap();
        let candidates = vec![SearchResult {
            username: "unsafe-peer".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file("Music\\A\\B\\..hidden.flac", 900, 10_000_000),
                make_file("Music\\A\\B\\..also-hidden.flac", 900, 10_000_000),
            ],
        }];
        let mut config = default_dl_config();
        config.max_retries = 0;

        let result = download_album(
            &client,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
            &mut DownloadStats::default(),
        )
        .await;

        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("no safe files"),
            "an unsafe candidate must be skipped with a reason, got {error:?}"
        );
        assert_eq!(
            client.calls.load(Ordering::SeqCst),
            0,
            "no request may be queued for a candidate with no safe file"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_file_below_the_minimum_bit_depth_is_rejected_after_download() {
        // The peer's metadata omits bit depth, so the file itself is checked. A
        // 16-bit FLAC against a 24-bit floor must be rejected rather than
        // accepted as an upgrade, and removed so it cannot be placed later.
        let client = ScriptedClient::new(vec![vec![status_step(
            Duration::from_secs(1),
            DownloadStatus::Completed,
        )]]);
        let dir = TempDir::new().unwrap();
        write_minimal_flac(&dir.path().join("depth.flac"));
        let file = make_file("Music\\A\\B\\depth.flac", 900, 10_000_000);
        let mut config = default_dl_config();
        config.max_retries = 0;
        let mut filters = default_filter_config_test();
        filters.min_bit_depth = 24;

        let result = download_file_for_candidate(
            &client,
            &file,
            "depth-peer",
            1,
            dir.path(),
            &config,
            &filters,
            None,
            None,
        )
        .await;

        assert!(
            matches!(result, Err(SeakarrError::QualityRejected(_))),
            "a 16-bit file must not pass a 24-bit floor, got {result:?}"
        );
        assert!(
            !dir.path().join("depth.flac").exists(),
            "the rejected file is removed so it cannot be placed later"
        );
    }

    // EMA (exponential moving average) tests — verifies the smoothing
    // function used for speed display.

    #[test]
    fn ema_first_sample_returns_raw_value() {
        // First call: no smoothing, return the raw value.
        let result = ema_update(None, 1_000_000.0, 0.3);
        assert_eq!(result, 1_000_000.0);
    }

    #[test]
    fn ema_smooths_second_sample() {
        // Second call: EMA = alpha * new + (1 - alpha) * prev
        // 0.3 * 2_000_000 + 0.7 * 1_000_000 = 600_000 + 700_000 = 1_300_000
        let result = ema_update(Some(1_000_000.0), 2_000_000.0, 0.3);
        assert!(
            (result - 1_300_000.0).abs() < 0.01,
            "expected ~1_300_000, got {result}"
        );
    }

    #[test]
    fn ema_converges_to_steady_state() {
        // Seed from 0 and feed the target repeatedly — EMA should converge.
        let alpha = 0.3;
        let target = 1_000_000.0;
        let mut ema = ema_update(None, 0.0, alpha); // start from 0
        for _ in 0..50 {
            ema = ema_update(Some(ema), target, alpha);
        }
        // After 50 iterations with alpha=0.3, the residual from the
        // initial 0 is (0.7)^50 ≈ 1.8e-8 — negligible.
        assert!((ema - target).abs() < 1.0, "expected ~{target}, got {ema}");
    }

    #[test]
    fn ema_smooths_out_spikes() {
        // A single spike should be smoothed significantly.
        let alpha = 0.3;
        let mut ema = ema_update(None, 1_000_000.0, alpha);
        // Spike: 10x normal speed
        ema = ema_update(Some(ema), 10_000_000.0, alpha);
        // EMA should be much less than the spike:
        // 0.3 * 10_000_000 + 0.7 * 1_000_000 = 3_000_000 + 700_000 = 3_700_000
        assert!(
            (ema - 3_700_000.0).abs() < 0.01,
            "expected ~3_700_000, got {ema}"
        );
        // Next reading back to normal:
        ema = ema_update(Some(ema), 1_000_000.0, alpha);
        // 0.3 * 1_000_000 + 0.7 * 3_700_000 = 300_000 + 2_590_000 = 2_890_000
        assert!(
            (ema - 2_890_000.0).abs() < 0.01,
            "expected ~2_890_000, got {ema}"
        );
    }

    #[test]
    fn effective_throughput_computes_bytes_per_wall_second() {
        use std::time::Duration;
        // 1024 bytes over 1 second = 1 KiB/s
        assert!((effective_throughput_kib_s(1024, Duration::from_secs(1)) - 1.0).abs() < 1e-9);
        // 1024 bytes over 2 seconds = 0.5 KiB/s
        assert!((effective_throughput_kib_s(1024, Duration::from_secs(2)) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn effective_throughput_guards_against_zero_elapsed() {
        use std::time::Duration;
        // Zero elapsed must not divide by zero; the 1ms floor yields a finite value.
        assert!(effective_throughput_kib_s(1024, Duration::ZERO).is_finite());
    }

    /// A transfer that completes without any InProgress sample must record a
    /// 0.0 throughput (excluded from the running average), not an inflated
    /// value from the 1ms floor.
    #[tokio::test]
    async fn test_completed_without_inprogress_records_zero_throughput() {
        use std::sync::Arc;
        let client = Arc::new(ControllableClient::new());
        let dir = TempDir::new().unwrap();
        let file = make_file("Music\\Artist\\Album\\01.flac", 900, 10_000_000);
        let config = default_dl_config();

        // Once download() is called, push a bare Completed (no InProgress).
        let pusher = {
            let client = client.clone();
            tokio::spawn(async move {
                while !client.download_called.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
                let tx = client.status_tx.lock().unwrap().clone().unwrap();
                tx.send(DownloadStatus::Completed).await.unwrap();
            })
        };

        let (_, speed) = download_file(
            &*client,
            &file,
            "peer",
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
        )
        .await
        .unwrap();
        pusher.await.unwrap();

        assert_eq!(
            speed, 0.0,
            "a transfer with no InProgress sample must record 0.0 throughput"
        );
    }

    /// A download that needs a retry must record a lower throughput than the
    /// same bytes delivered cleanly, because the retry_delay_secs sleep counts.
    #[tokio::test]
    async fn test_retried_download_records_lower_throughput() {
        let dir = TempDir::new().unwrap();
        let file = make_file("Music\\Artist\\Album\\01 - Track.flac", 900, 10_000_000);
        let mut config = default_dl_config();
        config.retry_delay_secs = 1;
        config.max_retries = 2;

        // Clean: no failures, no retry delay.
        let clean_client = RetryClient::new(0);
        let (_, clean_speed) = download_file(
            &clean_client,
            &file,
            "peer",
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
        )
        .await
        .unwrap();

        // One failure then success: the 1s retry delay is added to the clock.
        let retried_client = RetryClient::new(1);
        let (_, retried_speed) = download_file(
            &retried_client,
            &file,
            "peer",
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            retried_client.call_count(),
            2,
            "one retry must be attempted"
        );
        assert!(
            retried_speed < clean_speed / 2.0,
            "a retried download must record a much lower throughput \
             (retried {retried_speed} vs clean {clean_speed})"
        );
    }

    /// A client whose download status channel the test drives manually, so
    /// the moment a transfer "starts" (first byte-carrying `InProgress`) is
    /// under the test's control rather than raced against a background task.
    struct ControllableClient {
        /// Set when `download()` is called — signals the transfer was queued.
        download_called: Arc<std::sync::atomic::AtomicBool>,
        /// Sender captured by `download()`; the test pushes statuses through it.
        status_tx: Mutex<Option<mpsc::Sender<DownloadStatus>>>,
    }

    impl ControllableClient {
        fn new() -> Self {
            ControllableClient {
                download_called: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                status_tx: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl SoulseekClient for ControllableClient {
        async fn login(
            &self,
            _username: &str,
            _password: &str,
            _server: &str,
            _listen_port: u16,
        ) -> Result<()> {
            Ok(())
        }

        async fn search(&self, _query: &str, _timeout_secs: u64) -> Result<Vec<SearchResult>> {
            Ok(vec![])
        }

        async fn download(
            &self,
            _file: &FileInfo,
            _username: &str,
            _dir: &Path,
        ) -> Result<DownloadHandle> {
            let (status_tx, status_rx) = mpsc::channel(32);
            let (cancel_tx, _cancel_rx) = mpsc::channel(1);
            *self.status_tx.lock().unwrap() = Some(status_tx);
            self.download_called.store(true, Ordering::SeqCst);
            Ok(DownloadHandle {
                status_rx,
                cancel_tx,
            })
        }

        async fn request_queue_position(&self, _username: &str, _filename: &str) -> bool {
            false
        }
    }

    /// A client whose `download()` fails the first `failures` calls (e.g.
    /// with a timeout), then succeeds like MockClient. Records every call so
    /// tests can assert the retry count.
    struct RetryClient {
        /// How many initial `download()` calls should fail.
        failures: std::sync::atomic::AtomicUsize,
        /// Total `download()` calls made.
        calls: std::sync::atomic::AtomicUsize,
    }

    impl RetryClient {
        fn new(failures: usize) -> Self {
            RetryClient {
                failures: std::sync::atomic::AtomicUsize::new(failures),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl SoulseekClient for RetryClient {
        async fn login(
            &self,
            _username: &str,
            _password: &str,
            _server: &str,
            _listen_port: u16,
        ) -> Result<()> {
            Ok(())
        }

        async fn search(&self, _query: &str, _timeout_secs: u64) -> Result<Vec<SearchResult>> {
            Ok(vec![])
        }

        async fn download(
            &self,
            _file: &FileInfo,
            _username: &str,
            _dir: &Path,
        ) -> Result<DownloadHandle> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.failures.load(Ordering::SeqCst) {
                return Err(SeakarrError::Download("download timed out".into()));
            }
            let (status_tx, status_rx) = mpsc::channel(32);
            let (cancel_tx, mut cancel_rx) = mpsc::channel(1);
            let total = 10_000_000u64;
            // Success: emit InProgress then Completed (mirrors MockClient).
            tokio::spawn(async move {
                if cancel_rx.try_recv().is_ok() {
                    let _ = status_tx
                        .send(DownloadStatus::Failed {
                            reason: "cancelled".into(),
                        })
                        .await;
                    return;
                }
                let _ = status_tx
                    .send(DownloadStatus::InProgress {
                        speed_bytes_per_sec: 1_000_000,
                        bytes_downloaded: total,
                        total_bytes: total,
                    })
                    .await;
                let _ = status_tx.send(DownloadStatus::Completed).await;
            });
            Ok(DownloadHandle {
                status_rx,
                cancel_tx,
            })
        }

        async fn request_queue_position(&self, _username: &str, _filename: &str) -> bool {
            false
        }
    }

    /// A client that fails downloads for specific filenames, simulating
    /// partial downloads where some files in a candidate fail.
    struct SelectiveFailClient {
        /// Filenames that should return an error from `download()`.
        fail_files: std::collections::HashSet<String>,
    }

    impl SelectiveFailClient {
        fn new(fail_files: Vec<&str>) -> Self {
            SelectiveFailClient {
                fail_files: fail_files.into_iter().map(String::from).collect(),
            }
        }
    }

    #[async_trait]
    impl SoulseekClient for SelectiveFailClient {
        async fn login(&self, _u: &str, _p: &str, _s: &str, _l: u16) -> Result<()> {
            Ok(())
        }

        async fn search(&self, _q: &str, _t: u64) -> Result<Vec<SearchResult>> {
            Ok(vec![])
        }

        async fn download(
            &self,
            file: &FileInfo,
            _username: &str,
            _dir: &Path,
        ) -> Result<DownloadHandle> {
            if self.fail_files.contains(&file.name) {
                return Err(SeakarrError::Download("simulated failure".into()));
            }
            let (status_tx, status_rx) = mpsc::channel(32);
            let (cancel_tx, mut cancel_rx) = mpsc::channel(1);
            tokio::spawn(async move {
                if cancel_rx.try_recv().is_ok() {
                    let _ = status_tx
                        .send(DownloadStatus::Failed {
                            reason: "cancelled".into(),
                        })
                        .await;
                    return;
                }
                let _ = status_tx
                    .send(DownloadStatus::InProgress {
                        speed_bytes_per_sec: 1_000_000,
                        bytes_downloaded: 10_000_000,
                        total_bytes: 10_000_000,
                    })
                    .await;
                let _ = status_tx.send(DownloadStatus::Completed).await;
            });
            Ok(DownloadHandle {
                status_rx,
                cancel_tx,
            })
        }

        async fn request_queue_position(&self, _username: &str, _filename: &str) -> bool {
            false
        }
    }

    // Regression guard for the UploadDenied-everywhere bug: the Soulseek
    // QueueUpload wire message must carry the FULL share-relative path
    // exactly as the peer shared it ("Music\\Artist\\Album\\01 - Track.flac"),
    // not a basename. Sending only the basename made every peer respond
    // UploadDenied because it could not match the request against its share
    // list.
    #[tokio::test]
    async fn test_download_passes_full_share_path_to_client() {
        let client = MockClient::new();
        let dir = TempDir::new().unwrap();
        let file = make_file(
            "Music\\Amy Winehouse\\Back to Black (2006)\\1-01 - Rehab.flac",
            900,
            10_000_000,
        );
        let config = default_dl_config();

        let result = download_file(
            &client,
            &file,
            "peer",
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
        )
        .await;
        assert!(result.is_ok());

        let wire_name = client
            .last_download_filename
            .lock()
            .unwrap()
            .clone()
            .expect("download() recorded a filename");
        assert_eq!(
            wire_name, "Music\\Amy Winehouse\\Back to Black (2006)\\1-01 - Rehab.flac",
            "the wire filename must be the full share-relative path"
        );
    }

    #[tokio::test]
    async fn test_download_file_with_progress_bar() {
        let client = MockClient::new();
        let dir = TempDir::new().unwrap();
        let file = make_file("01 - track.flac", 900, 10_000_000);

        let config = default_dl_config();
        let display = ProgressDisplay::new();

        let result = download_file(
            &client,
            &file,
            "testuser",
            dir.path(),
            &config,
            &default_filter_config_test(),
            Some(&display),
            None,
        )
        .await;
        assert!(result.is_ok());
        // The mock client emits InProgress immediately, so a bar must have
        // been created once the transfer started.
        assert_eq!(display.created_bars(), 1);
    }

    #[tokio::test]
    async fn staging_line_says_staged_not_completed() {
        // "Download completed: ... -> <staging path>" read as the album's final
        // location. The line must say where the file was staged.
        //
        // A unique filename: `LogCapture` keeps one process-wide window, so under
        // a parallel suite this assertion must select its own staging line rather
        // than another test's.
        let client = MockClient::new();
        let dir = TempDir::new().unwrap();
        let file = make_file("staging-label-fixture.flac", 900, 10_000_000);
        let capture = crate::test_support::LogCapture::start();

        download_file(
            &client,
            &file,
            "peer",
            dir.path(),
            &default_dl_config(),
            &default_filter_config_test(),
            None,
            None,
        )
        .await
        .unwrap();

        let logs = capture.text();
        // Key both checks on this fixture's own staging line. Selecting on the
        // generic prefix would match a neighbouring test's line, and a crate-wide
        // negative on "Download completed:" would pass without proving anything,
        // since that string no longer exists in the crate.
        let line = logs
            .lines()
            .find(|line| line.contains("Download staged: staging-label-fixture.flac"))
            .unwrap_or_else(|| panic!("no staging line for this fixture, got:\n{logs}"));
        assert!(
            line.contains(" -> "),
            "the staging line must name the staging path, got: {line}"
        );
        assert!(
            !line.contains("completed"),
            "no line may call a staging path completed, got: {line}"
        );
    }

    // Regression guard: the progress bar must not be created until a
    // transfer has actually started (first InProgress status). Previously
    // download_album created the bar eagerly from search metadata BEFORE
    // calling download_file, so indicatif rendered a 0 B/29.77 MiB [0%] bar
    // on the runner's log line while the bridge was still starting.
    #[tokio::test]
    async fn progress_bar_not_created_until_transfer_starts() {
        let client = Arc::new(ControllableClient::new());
        let display = Arc::new(ProgressDisplay::new());
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file("01 - track.flac", 900, 10_000_000)],
        }];
        let config = default_dl_config();

        let task = tokio::spawn({
            let client = client.clone();
            let display = display.clone();
            let dir_path = dir.path().to_path_buf();
            async move {
                download_album(
                    client.as_ref() as &dyn SoulseekClient,
                    &candidates,
                    &dir_path,
                    &config,
                    &default_filter_config_test(),
                    Some(display.as_ref()),
                    None,
                    &mut DownloadStats::default(),
                )
                .await
            }
        });

        // Wait until the transfer has been queued with the client but no
        // status has been pushed yet.
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        while !client.download_called.load(Ordering::SeqCst) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "download() was never called"
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }

        // RED: while the transfer is only queued, no progress bar may exist.
        assert_eq!(
            display.created_bars(),
            0,
            "progress bar must not appear before the transfer starts"
        );

        // The transfer starts — push the first InProgress status.
        let tx = client
            .status_tx
            .lock()
            .unwrap()
            .clone()
            .expect("status sender captured by download()");
        tx.send(DownloadStatus::InProgress {
            speed_bytes_per_sec: 1_000_000,
            bytes_downloaded: 1_000_000,
            total_bytes: 10_000_000,
        })
        .await
        .unwrap();

        // The bar must now exist.
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        while display.created_bars() == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "bar was never created after the transfer started"
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
        assert_eq!(display.created_bars(), 1);

        // Complete the transfer so the task can finish.
        tx.send(DownloadStatus::Completed).await.unwrap();
        let result = task.await.unwrap();
        assert!(result.is_ok());
    }

    // Regression guard: downloads must retry the SAME peer up to
    // max_retries times (with retry_delay_secs between attempts) before
    // giving up. Previously download_file made a single attempt — the
    // max_retries/retry_delay_secs config values were dead code and a
    // failing peer moved straight to the next candidate.
    #[tokio::test]
    async fn download_retries_same_peer_after_failure() {
        // Fail the first 2 download() calls (timeout), succeed on the 3rd.
        let client = Arc::new(RetryClient::new(2));
        let dir = TempDir::new().unwrap();
        let file = make_file("track.flac", 900, 10_000_000);

        let mut config = default_dl_config();
        config.max_retries = 2;
        config.retry_delay_secs = 0; // instant retries for the test

        let result = download_file(
            client.as_ref() as &dyn SoulseekClient,
            &file,
            "testuser",
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
        )
        .await;

        // Succeeded after retries.
        assert!(
            result.is_ok(),
            "download should succeed after retries: {result:?}"
        );
        // 1 initial attempt + 2 retries = 3 calls.
        assert_eq!(
            client.call_count(),
            3,
            "expected initial attempt + 2 retries (max_retries=2), got {} calls",
            client.call_count()
        );
    }

    // When all retries are exhausted, download_file must surface the last
    // failure (not silently succeed or spin forever).
    #[tokio::test]
    async fn download_returns_error_after_all_retries_exhausted() {
        let client = Arc::new(RetryClient::new(5)); // always fails
        let dir = TempDir::new().unwrap();
        let file = make_file("track.flac", 900, 10_000_000);

        let mut config = default_dl_config();
        config.max_retries = 2;
        config.retry_delay_secs = 0;

        let result = download_file(
            client.as_ref() as &dyn SoulseekClient,
            &file,
            "testuser",
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
        )
        .await;

        assert!(
            result.is_err(),
            "download must fail after all retries exhausted"
        );
        assert_eq!(
            client.call_count(),
            3,
            "expected 1 initial + 2 retries = 3 attempts total"
        );
    }

    // When the cancel flag is set between attempts, the retry loop must
    // abort without retrying — the user pressed Ctrl+C during the delay
    // window or between attempt 1's failure and attempt 2's start.
    #[tokio::test]
    async fn download_does_not_retry_when_cancelled_between_attempts() {
        let client = Arc::new(RetryClient::new(5)); // always fails
        let dir = TempDir::new().unwrap();
        let file = make_file("track.flac", 900, 10_000_000);

        let mut config = default_dl_config();
        config.max_retries = 2;
        config.retry_delay_secs = 0;

        // Cancel flag is already set — download_file must abort without
        // retrying (the retry loop checks the flag before each attempt).
        let cancel = Arc::new(AtomicBool::new(true));

        let result = download_file(
            client.as_ref() as &dyn SoulseekClient,
            &file,
            "testuser",
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            Some(&cancel),
        )
        .await;

        assert!(result.is_err(), "download must fail when cancelled");
        // Only the initial attempt should have been made — the cancel
        // flag was set before the retry loop entered, so no retry.
        assert_eq!(
            client.call_count(),
            1,
            "expected exactly 1 attempt (cancel before any retry), got {}",
            client.call_count()
        );
    }

    // max_retries=0 disables retries entirely — a single attempt, then give up.
    #[tokio::test]
    async fn download_max_retries_zero_makes_single_attempt() {
        let client = Arc::new(RetryClient::new(5)); // always fails
        let dir = TempDir::new().unwrap();
        let file = make_file("track.flac", 900, 10_000_000);

        let mut config = default_dl_config();
        config.max_retries = 0; // disabled
        config.retry_delay_secs = 0;

        let result = download_file(
            client.as_ref() as &dyn SoulseekClient,
            &file,
            "testuser",
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
        )
        .await;

        assert!(result.is_err(), "download must fail after single attempt");
        assert_eq!(
            client.call_count(),
            1,
            "expected exactly 1 attempt (max_retries=0), got {}",
            client.call_count()
        );
    }

    #[tokio::test]
    async fn test_download_single_file_succeeds() {
        let client = MockClient::new();
        let dir = TempDir::new().unwrap();
        let file = make_file("track.flac", 900, 10_000_000);
        let config = default_dl_config();

        let result = download_file(
            &client,
            &file,
            "testuser",
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
        )
        .await;
        assert!(result.is_ok());
    }

    // Regression guard: when a candidate has multiple files but some
    // fail to download, download_album must NOT return Ok — it should
    // fall back to the next candidate (or return an error if exhausted).
    // The failure is detected by download_file returning Err, which sets
    // failed=true and breaks the loop — the candidate is abandoned and
    // the staging dir is cleaned up.
    #[tokio::test]
    async fn download_album_fails_when_not_all_files_downloaded() {
        // Candidate has 3 files, but "02 - track.flac" always fails.
        // With max_retries=0 (no retries), the 1st file succeeds, the
        // 2nd fails → failed=true, loop breaks, candidate abandoned.
        let client = Arc::new(SelectiveFailClient::new(vec!["02 - track.flac"]));
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file("01 - track.flac", 900, 10_000_000),
                make_file("02 - track.flac", 900, 10_000_000),
                make_file("03 - track.flac", 900, 10_000_000),
            ],
        }];

        let mut config = default_dl_config();
        config.max_retries = 0; // no retries — fail fast
        config.retry_delay_secs = 0;

        let result = download_album(
            client.as_ref() as &dyn SoulseekClient,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
            &mut DownloadStats::default(),
        )
        .await;

        // Must fail: file "02" returned Err → failed=true → candidate
        // abandoned → no more candidates → all candidates exhausted.
        assert!(
            result.is_err(),
            "download_album must fail when a file download fails, got: {result:?}"
        );
    }

    // The album outcome is reported once (the runner's album line and the run
    // summary both carry it), so exhausting retries on one candidate must not
    // add a second warning naming that peer. Fixture names are unique because
    // `LogCapture` keeps recording while other tests run: a concurrent test's
    // identical line must not be able to satisfy the guard.
    #[tokio::test]
    async fn a_candidate_retry_exhaustion_is_a_debug_record() {
        let client = Arc::new(SelectiveFailClient::new(vec!["quiet-02.flac"]));
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "quiet-peer-3a91".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file("quiet-01.flac", 900, 10_000_000),
                make_file("quiet-02.flac", 900, 10_000_000),
            ],
        }];

        let mut config = default_dl_config();
        config.max_retries = 0;
        config.retry_delay_secs = 0;
        let capture = crate::test_support::LogCapture::start();

        let result = download_album(
            client.as_ref() as &dyn SoulseekClient,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
            &mut DownloadStats::default(),
        )
        .await;
        assert!(
            result.is_err(),
            "a failed file must abandon the candidate, got: {result:?}"
        );

        let logs = capture.text();
        let line = logs
            .lines()
            .find(|line| {
                line.contains("failed after retries:")
                    && line.contains("quiet-02.flac")
                    && line.contains("quiet-peer-3a91")
            })
            .unwrap_or_else(|| {
                panic!("no per-candidate failure line for this fixture, got:\n{logs}")
            });
        assert_eq!(
            line.split_whitespace().next(),
            Some("DEBUG"),
            "a candidate retry exhaustion is not a warning: {line}"
        );
    }

    // Bug regression: a peer sharing multiple albums under the same artist
    // (e.g. "Abba\\[1992] Gold_ Greatest Hits\\..." and
    // "Abba\\[1993] More ABBA Gold\\...") returns files from ALL matching
    // album directories in one search result. download_album must only
    // download files from a single album directory, or the staging folder
    // becomes a jumbled mix of tracks from different albums.
    #[tokio::test]
    async fn download_album_downloads_only_one_album_directory_per_candidate() {
        let client = Arc::new(MockClient::new());
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "cassland".into(),
            speed: 900,
            slots: 1,
            // Two different album directories in the same peer's share:
            // [1992] Gold_ Greatest Hits (tracks 16-19) and
            // [1993] More ABBA Gold (tracks 01-08).
            files: vec![
                make_file(
                    "Musikk\\Abba\\[1992] Gold_ Greatest Hits\\01-16- One of Us.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Musikk\\Abba\\[1992] Gold_ Greatest Hits\\01-17- The Name of the Game (edit).flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Musikk\\Abba\\[1993] More ABBA Gold\\01. summer night city.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Musikk\\Abba\\[1993] More ABBA Gold\\02. angeleyes.flac",
                    900,
                    10_000_000,
                ),
            ],
        }];

        let mut config = default_dl_config();
        config.max_retries = 0;
        config.retry_delay_secs = 0;

        let result = download_album(
            client.as_ref() as &dyn SoulseekClient,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
            &mut DownloadStats::default(),
        )
        .await;

        // Must succeed (the single album's files are all downloadable).
        assert!(result.is_ok(), "download_album should succeed: {result:?}");

        // Only files from ONE album directory may be downloaded — never a
        // mix of two albums into the same staging folder.
        let downloaded = client
            .download_filenames
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let dirs: std::collections::HashSet<String> = downloaded
            .iter()
            .map(|n| n.rsplit(['/', '\\']).nth(1).unwrap_or("<root>").to_string())
            .collect();
        assert!(
            dirs.len() == 1,
            "files from multiple album directories were downloaded into the same staging folder: {downloaded:?}"
        );
    }

    // When a peer shares two albums with different sizes, the LARGER album
    // group should be selected (not the first, not the last, not random).
    #[tokio::test]
    async fn download_album_prefers_largest_album_group() {
        let client = Arc::new(MockClient::new());
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "peer".into(),
            speed: 900,
            slots: 1,
            files: vec![
                // Small album: 2 files
                make_file(
                    "Musikk\\Abba\\[1992] Gold_ Greatest Hits\\01-16- One of Us.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Musikk\\Abba\\[1992] Gold_ Greatest Hits\\01-17- The Name of the Game (edit).flac",
                    900,
                    10_000_000,
                ),
                // Large album: 4 files — should be selected
                make_file(
                    "Musikk\\Abba\\[1993] More ABBA Gold\\01. summer night city.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Musikk\\Abba\\[1993] More ABBA Gold\\02. angeleyes.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Musikk\\Abba\\[1993] More ABBA Gold\\03. chiquitita.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Musikk\\Abba\\[1993] More ABBA Gold\\04. does your mother know.flac",
                    900,
                    10_000_000,
                ),
            ],
        }];

        let mut config = default_dl_config();
        config.max_retries = 0;
        config.retry_delay_secs = 0;

        let result = download_album(
            client.as_ref() as &dyn SoulseekClient,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
            &mut DownloadStats::default(),
        )
        .await;

        assert!(result.is_ok(), "download_album should succeed: {result:?}");
        let downloaded = client
            .download_filenames
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            downloaded.len(),
            4,
            "should download the larger album (More ABBA Gold), got {downloaded:?}"
        );
        // All downloaded files must be from the larger album
        assert!(
            downloaded.iter().all(|n| n.contains("More ABBA Gold")),
            "expected all files from More ABBA Gold, got {downloaded:?}"
        );
    }

    // Regression for the Michael Bolton "The Essential Michael Bolton"
    // case: a peer shares a multi-CD album with one subfolder per disc
    // (e.g. "...\FLAC (16bit-44.1kHz)\CD 01\*.flac" and
    // "...\FLAC (16bit-44.1kHz)\CD 02\*.flac"). The previous grouping
    // keyed on the full parent path, so CD 01 and CD 02 were treated as
    // separate albums and only the larger disc was downloaded — losing
    // half the album. ALL discs of the same album must download together.
    #[tokio::test]
    async fn download_album_downloads_all_discs_of_multi_disc_album() {
        let client = Arc::new(MockClient::new());
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "peer".into(),
            speed: 900,
            slots: 1,
            files: vec![
                // CD 01 — 3 files
                make_file(
                    "Music\\Michael Bolton\\The Essential Michael Bolton\\CD 01\\01 - One.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Michael Bolton\\The Essential Michael Bolton\\CD 01\\02 - Two.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Michael Bolton\\The Essential Michael Bolton\\CD 01\\03 - Three.flac",
                    900,
                    10_000_000,
                ),
                // CD 02 — 4 files (larger disc; previously selected alone)
                make_file(
                    "Music\\Michael Bolton\\The Essential Michael Bolton\\CD 02\\01 - Four.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Michael Bolton\\The Essential Michael Bolton\\CD 02\\02 - Five.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Michael Bolton\\The Essential Michael Bolton\\CD 02\\03 - Six.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Michael Bolton\\The Essential Michael Bolton\\CD 02\\04 - Seven.flac",
                    900,
                    10_000_000,
                ),
            ],
        }];

        let mut config = default_dl_config();
        config.max_retries = 0;
        config.retry_delay_secs = 0;

        let result = download_album(
            client.as_ref() as &dyn SoulseekClient,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
            &mut DownloadStats::default(),
        )
        .await;

        assert!(result.is_ok(), "download_album should succeed: {result:?}");
        let downloaded = client
            .download_filenames
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            downloaded.len(),
            7,
            "all discs of the multi-CD album must download together, got {downloaded:?}"
        );
        // Files from BOTH discs are present.
        assert!(
            downloaded.iter().any(|n| n.contains("CD 01"))
                && downloaded.iter().any(|n| n.contains("CD 02")),
            "expected files from both CD 01 and CD 02, got {downloaded:?}"
        );
    }

    // Disc markers embedded in the album folder name ("Gold (Disc 1)" /
    // "Gold (Disc 2)") are the SAME album and must merge into one group.
    // A DIFFERENT album under the same artist ("More ABBA Gold") must NOT
    // be merged in — only discs of the same album group together.
    #[tokio::test]
    async fn download_album_merges_same_album_discs_but_not_different_albums() {
        let client = Arc::new(MockClient::new());
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "peer".into(),
            speed: 900,
            slots: 1,
            files: vec![
                // Gold (Disc 1) — 3 files
                make_file("Music\\Abba\\Gold (Disc 1)\\01 - One.flac", 900, 10_000_000),
                make_file("Music\\Abba\\Gold (Disc 1)\\02 - Two.flac", 900, 10_000_000),
                make_file(
                    "Music\\Abba\\Gold (Disc 1)\\03 - Three.flac",
                    900,
                    10_000_000,
                ),
                // Gold (Disc 2) — 4 files (same album; must merge)
                make_file(
                    "Music\\Abba\\Gold (Disc 2)\\01 - Four.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Abba\\Gold (Disc 2)\\02 - Five.flac",
                    900,
                    10_000_000,
                ),
                make_file("Music\\Abba\\Gold (Disc 2)\\03 - Six.flac", 900, 10_000_000),
                make_file(
                    "Music\\Abba\\Gold (Disc 2)\\04 - Seven.flac",
                    900,
                    10_000_000,
                ),
                // A different album — must NOT be merged with Gold
                make_file(
                    "Music\\Abba\\More ABBA Gold\\01 - Track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Abba\\More ABBA Gold\\02 - Track.flac",
                    900,
                    10_000_000,
                ),
            ],
        }];

        let mut config = default_dl_config();
        config.max_retries = 0;
        config.retry_delay_secs = 0;

        let result = download_album(
            client.as_ref() as &dyn SoulseekClient,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
            &mut DownloadStats::default(),
        )
        .await;

        assert!(result.is_ok(), "download_album should succeed: {result:?}");
        let downloaded = client
            .download_filenames
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            downloaded.len(),
            7,
            "both Gold discs (7 files) must download together, More ABBA Gold excluded, got {downloaded:?}"
        );
        assert!(
            downloaded.iter().all(|n| n.contains("Gold (Disc")),
            "expected only files from Gold (Disc 1)/(Disc 2), got {downloaded:?}"
        );
    }

    // Two albums with the same leaf directory name under different parents
    // must be treated as separate groups (e.g. "Abba\\Greatest Hits" vs
    // "Bee Gees\\Greatest Hits"). The grouping key is the full parent
    // path, not just the immediate directory name.
    #[tokio::test]
    async fn download_album_distinguishes_same_named_dirs_under_different_parents() {
        let client = Arc::new(MockClient::new());
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "peer".into(),
            speed: 900,
            slots: 1,
            files: vec![
                // Abba\Greatest Hits — 2 files
                make_file(
                    "Music\\Abba\\Greatest Hits\\01 - track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Abba\\Greatest Hits\\02 - track.flac",
                    900,
                    10_000_000,
                ),
                // Bee Gees\Greatest Hits — 3 files (larger, should win)
                make_file(
                    "Music\\Bee Gees\\Greatest Hits\\01 - track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Bee Gees\\Greatest Hits\\02 - track.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Bee Gees\\Greatest Hits\\03 - track.flac",
                    900,
                    10_000_000,
                ),
            ],
        }];

        let mut config = default_dl_config();
        config.max_retries = 0;
        config.retry_delay_secs = 0;

        let result = download_album(
            client.as_ref() as &dyn SoulseekClient,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
            &mut DownloadStats::default(),
        )
        .await;

        assert!(result.is_ok(), "download_album should succeed: {result:?}");
        let downloaded = client
            .download_filenames
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            downloaded.len(),
            3,
            "should download Bee Gees (larger group), got {downloaded:?}"
        );
        assert!(
            downloaded.iter().all(|n| n.contains("Bee Gees")),
            "expected all files from Bee Gees, got {downloaded:?}"
        );
    }

    #[tokio::test]
    async fn test_download_slow_speed_fails() {
        // A peer that cannot reach `min_upload_speed_kbps` fails the download.
        // `download_file` retries transient failures on the same peer, but a
        // below-floor abort is permanent for that peer — asking again only puts
        // the file back in that peer's queue — so one attempt is made and the
        // candidate fallback takes over.
        let client = MockClient::new();
        *client.download_speed.lock().unwrap() = 100_000; // 100 KB/s
        let dir = TempDir::new().unwrap();
        let file = make_file("track.flac", 900, 10_000_000);

        let mut config = default_dl_config();
        config.min_upload_speed_kbps = 1_000_000;

        let result = download_file(
            &client,
            &file,
            "testuser",
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(
            client.download_filenames.lock().unwrap().len(),
            1,
            "a below-floor peer must not be re-queued in place"
        );
    }

    #[tokio::test]
    async fn test_download_with_candidate_fallback() {
        let client = MockClient::new();
        *client.download_speed.lock().unwrap() = 1_000_000; // Fast enough
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "user1".into(),
            speed: 300,
            slots: 1,
            files: vec![make_file("track.flac", 900, 10_000_000)],
        }];
        let config = default_dl_config();

        let result = download_album(
            &client,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
            &mut DownloadStats::default(),
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_failed_candidate_cleans_up_part_files() {
        // A candidate whose download fails partway must leave the staging
        // directory clean — no completed tracks, no stale `.part` files.
        // Regression: the orphan loop only removed files returned as `Ok`,
        // leaving `.part` files behind after a failed transfer.
        let client = MockClient::new();
        // Slow speed forces the speed-check failure on the first file.
        *client.download_speed.lock().unwrap() = 100_000; // 100 KB/s
        let dir = TempDir::new().unwrap();

        // Simulate a leftover `.part` file (as the vendor lib leaves behind
        // on an interrupted transfer) plus a completed track.
        std::fs::write(dir.path().join("01 - track.flac.part"), b"partial").unwrap();
        std::fs::write(dir.path().join("02 - track.flac"), b"complete").unwrap();

        let candidates = vec![SearchResult {
            username: "user1".into(),
            speed: 300,
            slots: 1,
            files: vec![make_file("01 - track.flac", 900, 10_000_000)],
        }];
        let mut config = default_dl_config();
        // Require impossibly fast upload so the speed check fails.
        config.min_upload_speed_kbps = 10_000_000;
        config.speed_check_wait_secs = 0;

        let result = download_album(
            &client,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
            &mut DownloadStats::default(),
        )
        .await;
        assert!(result.is_err());

        // The staging directory must be gone — no partial downloads at all.
        assert!(
            !dir.path().exists(),
            "staging dir must be removed after failed candidate"
        );
    }

    #[tokio::test]
    async fn test_cancellation_flag_aborts_download_and_cleans_up() {
        // When the cancellation flag is set (Ctrl+C / SIGINT), download_album
        // must abort and clean the staging directory — including any `.part`
        // files the vendor library would leave behind.

        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file("01 - track.flac", 900, 10_000_000)],
        }];

        let dir = TempDir::new().unwrap();
        // Simulate a leftover .part file from a previous run.
        std::fs::write(dir.path().join("01 - track.flac.part"), b"stale partial").unwrap();

        let config = default_dl_config();
        let filter_config = default_filter_config_test();

        // Cancellation is already requested — download_album should abort
        // before queuing the download to the peer.
        let cancelled = Arc::new(AtomicBool::new(true));
        let results = client.search_results.lock().unwrap().clone();
        let result = download_album(
            client.as_ref(),
            &results,
            dir.path(),
            &config,
            &filter_config,
            None,
            Some(&cancelled),
            &mut DownloadStats::default(),
        )
        .await;
        assert!(result.is_err(), "download_album must abort when cancelled");
        assert!(
            result.unwrap_err().to_string().contains("cancelled"),
            "error must indicate cancellation"
        );

        // The staging directory must be gone — no partial downloads at all.
        assert!(
            !dir.path().exists(),
            "staging dir must be removed after cancellation"
        );
    }

    #[tokio::test]
    async fn test_multi_disc_downloads_into_per_disc_subdirectories() {
        // Regression: when a peer shares a multi-CD album, tracks from
        // different discs must land in separate subdirectories inside the
        // staging folder — not all dumped into the same flat directory.
        // Same-named files from different discs (e.g. "01 - Track.flac"
        // from both CD 01 and CD 02) would previously collide, losing
        // tracks.
        let client = Arc::new(MockClient::new());
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "peer".into(),
            speed: 900,
            slots: 1,
            files: vec![
                // CD 01 — 2 files
                make_file("Music\\Album\\CD 01\\01 - Track.flac", 900, 10_000_000),
                make_file("Music\\Album\\CD 01\\02 - Track.flac", 900, 10_000_000),
                // CD 02 — 2 files with SAME names (collision risk)
                make_file("Music\\Album\\CD 02\\01 - Track.flac", 900, 10_000_000),
                make_file("Music\\Album\\CD 02\\02 - Track.flac", 900, 10_000_000),
            ],
        }];

        let mut config = default_dl_config();
        config.max_retries = 0;
        config.retry_delay_secs = 0;

        let result = download_album(
            client.as_ref() as &dyn SoulseekClient,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
            &mut DownloadStats::default(),
        )
        .await;

        assert!(result.is_ok(), "download_album should succeed: {result:?}");
        let downloaded = result.unwrap();
        assert_eq!(
            downloaded.len(),
            4,
            "all 4 files from both discs must download (no collision), got {downloaded:?}"
        );
        // CD 01 files must be in a CD 01 subdirectory
        assert!(
            downloaded
                .iter()
                .any(|p| p.to_string_lossy().contains("CD 01")),
            "CD 01 files must be in a CD 01 subdirectory, got {downloaded:?}"
        );
        // CD 02 files must be in a CD 02 subdirectory
        assert!(
            downloaded
                .iter()
                .any(|p| p.to_string_lossy().contains("CD 02")),
            "CD 02 files must be in a CD 02 subdirectory, got {downloaded:?}"
        );
    }

    #[tokio::test]
    async fn test_embedded_marker_discs_stage_into_separate_subdirectories() {
        // Regression (release-review Finding 2): albums whose discs carry an
        // embedded marker ("Gold (Disc 1)" / "Gold (Disc 2)") are merged
        // into one download group by album_group_key, so same-named tracks
        // across discs ("01 - Four.flac" on both discs) must still be
        // isolated into per-disc staging subdirectories. Before this fix the
        // marker style was staged flat and the second disc's file overwrote
        // the first.
        let client = Arc::new(MockClient::new());
        *client.write_files.lock().unwrap() = true; // real bytes on disk
        let dir = TempDir::new().unwrap();

        let candidates = vec![SearchResult {
            username: "peer".into(),
            speed: 900,
            slots: 1,
            files: vec![
                make_file(
                    "Music\\Abba\\Gold (Disc 1)\\01 - Four.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Abba\\Gold (Disc 1)\\02 - Five.flac",
                    900,
                    10_000_000,
                ),
                // Same basenames on disc 2 — collision risk if staged flat.
                make_file(
                    "Music\\Abba\\Gold (Disc 2)\\01 - Four.flac",
                    900,
                    10_000_000,
                ),
                make_file(
                    "Music\\Abba\\Gold (Disc 2)\\02 - Five.flac",
                    900,
                    10_000_000,
                ),
            ],
        }];

        let mut config = default_dl_config();
        config.max_retries = 0;
        config.retry_delay_secs = 0;

        let result = download_album(
            client.as_ref() as &dyn SoulseekClient,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
            &mut DownloadStats::default(),
        )
        .await;

        assert!(result.is_ok(), "download_album should succeed: {result:?}");
        let downloaded = result.unwrap();
        assert_eq!(
            downloaded.len(),
            4,
            "all 4 files across both marker discs must download, got {downloaded:?}"
        );

        // Each downloaded path sits under its own disc subdirectory.
        let mut disc1_files = 0;
        let mut disc2_files = 0;
        for p in &downloaded {
            let parent_name = p
                .parent()
                .and_then(|d| d.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            match parent_name.as_str() {
                "Gold (Disc 1)" => disc1_files += 1,
                "Gold (Disc 2)" => disc2_files += 1,
                other => panic!("unexpected staging parent {other:?} for {p:?}"),
            }
            assert!(p.is_file(), "downloaded file must exist on disk: {p:?}");
        }
        assert_eq!(disc1_files, 2);
        assert_eq!(disc2_files, 2);

        // The two same-named files exist as distinct files in distinct dirs.
        let d1 = dir.path().join("Gold (Disc 1)").join("01 - Four.flac");
        let d2 = dir.path().join("Gold (Disc 2)").join("01 - Four.flac");
        assert!(
            d1.is_file() && d2.is_file(),
            "both disc copies must survive"
        );
    }

    #[tokio::test]
    async fn brace_marker_discs_stage_into_separate_subdirectories() {
        let client = Arc::new(MockClient::new());
        *client.write_files.lock().unwrap() = true;
        let dir = TempDir::new().unwrap();
        let candidates = vec![SearchResult {
            username: "peer".into(),
            speed: 900,
            slots: 1,
            files: vec![
                make_file("Music\\Album {cd1}\\01.flac", 900, 10_000_000),
                make_file("Music\\Album {cd2}\\01.flac", 900, 10_000_000),
            ],
        }];
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.retry_delay_secs = 0;

        let downloaded = download_album(
            client.as_ref(),
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
            &mut DownloadStats::default(),
        )
        .await
        .unwrap();

        assert_eq!(downloaded.len(), 2);
        assert!(dir.path().join("Album {cd1}/01.flac").is_file());
        assert!(dir.path().join("Album {cd2}/01.flac").is_file());
    }

    // ── Post-download quality verification ──

    #[tokio::test]
    async fn test_download_skips_verification_when_min_not_set() {
        let client = MockClient::new();
        let dir = TempDir::new().unwrap();
        let file = make_file("track.flac", 900, 10_000_000);
        let config = default_dl_config();
        // No minimums configured → no post-download verification runs.
        let filters = FilterConfig {
            min_bit_rate: 0,
            min_bit_depth: 0,
            ..default_filter_config_test()
        };

        let result = download_file(
            &client,
            &file,
            "testuser",
            dir.path(),
            &config,
            &filters,
            None,
            None,
        )
        .await;
        assert!(
            result.is_ok(),
            "download must succeed with no minimums configured: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_download_skips_verification_when_peer_provides_metadata() {
        let client = MockClient::new();
        let dir = TempDir::new().unwrap();
        // Peer provides bitrate metadata (attribs key 0 = 320) — the
        // pre-download filter already checked it, so no post-download
        // verification may run.
        let file = make_file("track.mp3", 320, 10_000_000);
        let config = default_dl_config();
        let filters = FilterConfig {
            min_bit_rate: 320,
            min_bit_depth: 0,
            ..default_filter_config_test()
        };

        let result = download_file(
            &client,
            &file,
            "testuser",
            dir.path(),
            &config,
            &filters,
            None,
            None,
        )
        .await;
        assert!(
            result.is_ok(),
            "download must succeed when the peer provided the metadata: {result:?}"
        );
    }

    #[test]
    fn test_download_fails_fast_on_bitrate_failure() {
        // verify_downloaded_quality on a real lossless FLAC: min_bitrate
        // only applies to lossy formats, so extract_bitrate returns None
        // (lossless) and the file passes. A genuine low-bitrate lossy MP3
        // is not feasible to generate in a unit test — see final report.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("track.flac");
        write_minimal_flac(&path);

        // Peer did NOT provide bitrate metadata (empty attribs) — the
        // post-download verification must kick in and pass the lossless
        // file.
        let file = FileInfo {
            name: "track.flac".into(),
            size: 10_000_000,
            attribs: HashMap::new(),
        };
        let filters = FilterConfig {
            min_bit_rate: 320,
            min_bit_depth: 0,
            ..default_filter_config_test()
        };

        let result = verify_downloaded_quality(&path, &file, &filters);
        assert!(
            result.is_ok(),
            "lossless file must pass the min_bitrate verification: {result:?}"
        );
    }

    #[test]
    fn test_verify_rejects_low_bitdepth_when_min_set() {
        // A real 16-bit FLAC must fail verification when min_bitdepth is
        // set to 24 — the actual bitdepth (16) is below the minimum.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("track.flac");
        write_minimal_flac(&path);

        let file = FileInfo {
            name: "track.flac".into(),
            size: 10_000_000,
            attribs: HashMap::new(), // peer did NOT provide bitdepth
        };
        let filters = FilterConfig {
            min_bit_rate: 0,
            min_bit_depth: 24,
            ..default_filter_config_test()
        };

        let result = verify_downloaded_quality(&path, &file, &filters);
        assert!(
            result.is_err(),
            "16-bit FLAC must fail min_bitdepth=24 verification"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("bitdepth") && err_msg.contains("below minimum"),
            "error must mention bitdepth and minimum, got: {err_msg}"
        );
    }

    #[test]
    fn quality_rejections_are_not_retried() {
        // Regression (release-review Finding 3): a file rejected by
        // post-download quality verification must not be re-downloaded from
        // the same peer — its quality cannot change. Transient failures
        // remain retryable.
        use crate::error::SeakarrError;
        assert!(!is_retryable(&SeakarrError::QualityRejected(
            "bitrate 90 kbps below minimum 320 kbps".into()
        )));
        assert!(!is_retryable(&SeakarrError::QualityRejected(
            "bitdepth 16 below minimum 24".into()
        )));
        // A below-floor transfer is the peer's rate, not a transient fault:
        // re-requesting it only re-enters that peer's queue.
        assert!(!is_retryable(&SeakarrError::SlowDownload(
            "speed 310 KB/s below minimum 400 KB/s".into()
        )));
        assert!(is_retryable(&SeakarrError::Download(
            "transfer failed: user declined".into()
        )));
        assert!(is_retryable(&SeakarrError::Download(
            "download timed out".into()
        )));
    }

    // ── Extracted wait-state units (QueueWait / TransferProgress) ──
    //
    // `download_once` is now a poll loop over these two state machines, so their
    // transitions are pinned here directly rather than only through the scripted
    // client: a deadline mix-up or a dropped queue position is far cheaper to
    // localise at this level.

    #[tokio::test(start_paused = true)]
    async fn queue_wait_picks_the_earlier_enabled_limit() {
        // The head limit only starts at the first position-1 observation, so it
        // can expire before or after the total limit; the earliest one must win,
        // and it must report which limit produced it. A limit of 0 is disabled,
        // not immediate.
        let mut config = default_dl_config();
        config.max_queue_time_secs = 1800;
        config.max_start_time_secs = 120;
        let start = tokio::time::Instant::now();
        let mut queue = QueueWait::new(start, &config);

        let (deadline, kind) = queue
            .active_deadline(&config)
            .expect("total limit is enabled");
        assert_eq!(kind, QueueDeadlineKind::TotalQueue);
        assert_eq!(deadline, start + Duration::from_secs(1800));

        // Reaching position 1 at +10s starts the head clock, which then wins.
        assert!(queue
            .observe(
                Some(1),
                start + Duration::from_secs(10),
                "01.flac",
                "peer",
                &config,
                None
            )
            .is_none());
        let (deadline, kind) = queue
            .active_deadline(&config)
            .expect("head limit is enabled");
        assert_eq!(kind, QueueDeadlineKind::QueueHead);
        assert_eq!(deadline, start + Duration::from_secs(130));

        // Both limits disabled: the wait is unbounded. The total limit is
        // snapshotted when the attempt is queued, so this needs a fresh state.
        config.max_queue_time_secs = 0;
        config.max_start_time_secs = 0;
        let mut unbounded = QueueWait::new(start, &config);
        assert!(unbounded
            .observe(Some(1), start, "01.flac", "peer", &config, None)
            .is_none());
        assert!(unbounded.active_deadline(&config).is_none());
    }

    #[test]
    fn the_position_ask_is_due_every_thirty_seconds() {
        // The vendored crate asks once at enqueue, so the first ask from seakarr
        // is a refresh, never a duplicate.
        let config = default_dl_config();
        let start = tokio::time::Instant::now();
        let mut queue = QueueWait::new(start, &config);

        assert!(
            !queue.position_request_is_due(start),
            "the crate already asked at enqueue"
        );
        assert!(!queue.position_request_is_due(start + Duration::from_secs(29)));
        assert!(queue.position_request_is_due(start + Duration::from_secs(30)));
        assert!(
            !queue.position_request_is_due(start + Duration::from_secs(59)),
            "the deadline advances by one interval per ask"
        );
        assert!(queue.position_request_is_due(start + Duration::from_secs(60)));
    }

    #[tokio::test(start_paused = true)]
    async fn queue_wait_ignores_unusable_positions_and_rejects_over_limit_ones() {
        let mut config = default_dl_config();
        config.max_queue_length = 3;
        let start = tokio::time::Instant::now();
        let mut queue = QueueWait::new(start, &config);

        // Wire position 0 and "no position" prove nothing about a zero-slot
        // peer: neither may be recorded, rejected, or reported.
        for position in [Some(0), None] {
            assert!(queue
                .observe(position, start, "01.flac", "peer", &config, None)
                .is_none());
        }
        assert_eq!(queue.observed_position, None);
        assert!(!queue.notice_emitted, "an unusable position emits nothing");

        // A position above the cap is rejected, naming the position and limit.
        let reason = queue
            .observe(Some(4), start, "01.flac", "peer", &config, None)
            .expect("position 4 exceeds max_queue_length=3");
        assert!(
            reason.contains("position 4") && reason.contains("max_queue_length=3"),
            "the rejection must name the position and the limit, got: {reason}"
        );

        // In-bounds positions are recorded, and the first one emits the notice.
        assert!(queue
            .observe(Some(2), start, "01.flac", "peer", &config, None)
            .is_none());
        assert_eq!(queue.observed_position, Some(2));
        assert!(queue.notice_emitted);
    }

    #[tokio::test(start_paused = true)]
    async fn queue_wait_emits_the_notice_once_and_times_the_wait_from_enqueue() {
        let config = default_dl_config();
        let start = tokio::time::Instant::now();
        let mut queue = QueueWait::new(start, &config);

        assert!(!queue.notice_is_due(start), "the grace has not expired yet");
        assert!(queue.notice_is_due(start + QUEUE_NOTICE_GRACE));
        queue.notice("01.flac", "peer");
        assert!(queue.notice_emitted);
        // Once emitted, the grace can no longer re-arm it.
        assert!(!queue.notice_is_due(start + Duration::from_secs(600)));
        queue.notice("01.flac", "peer");
        assert_eq!(
            queue.wait(start + Duration::from_secs(42)),
            Duration::from_secs(42),
            "the started line reports the wait from enqueue"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn transfer_progress_holds_the_speed_floor_until_real_bytes_arrive() {
        // The peer's offset handshake reports zero bytes with zero speed before a
        // byte is read, so it must not start the clock: a peer still draining its
        // own queue would be cancelled for a speed it never had a chance to
        // reach. Once real bytes arrive, the floor applies after the wait period.
        let mut config = default_dl_config();
        config.min_upload_speed_kbps = 400;
        config.speed_check_wait_secs = 30;
        let start = tokio::time::Instant::now();
        let mut transfer = TransferProgress::new();
        assert!(!transfer.has_started(), "a new transfer has no start clock");

        transfer.start(start, 10_000_000, "01.flac", None);
        assert!(transfer.has_started());

        // The sender is dropped so the cancel-and-drain on the fatal sample
        // returns immediately instead of waiting out its window.
        let (status_tx, status_rx) = mpsc::channel(1);
        drop(status_tx);
        let (cancel_tx, _cancel_rx) = mpsc::channel(1);
        let mut handle = DownloadHandle {
            status_rx,
            cancel_tx,
        };

        transfer
            .sample(
                &mut handle,
                None,
                start + Duration::from_secs(1),
                &config,
                1_024,
                100,
                10_000_000,
            )
            .await
            .expect("a below-floor sample inside the wait window is not fatal");

        let error = transfer
            .sample(
                &mut handle,
                None,
                start + Duration::from_secs(31),
                &config,
                1_024,
                200,
                10_000_000,
            )
            .await
            .expect_err("a below-floor sample past the wait window must fail");
        assert!(
            matches!(error, SeakarrError::SlowDownload(_)),
            "a below-floor abort must be the permanent SlowDownload, got: {error:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn transfer_progress_inactivity_starts_with_the_transfer() {
        let mut config = default_dl_config();
        config.timeout_secs = 20;
        let now = tokio::time::Instant::now();
        let mut transfer = TransferProgress::new();
        assert!(
            !transfer.is_inactive(now),
            "an unstarted transfer has no inactivity timeout"
        );

        transfer.start(now, 10_000_000, "01.flac", None);
        let (status_tx, status_rx) = mpsc::channel(1);
        let (cancel_tx, _cancel_rx) = mpsc::channel(1);
        let mut handle = DownloadHandle {
            status_rx,
            cancel_tx,
        };

        transfer
            .sample(&mut handle, None, now, &config, 0, 0, 10_000_000)
            .await
            .expect("a sample is applied");
        assert!(!transfer.is_inactive(now + Duration::from_secs(19)));
        assert!(transfer.is_inactive(now + Duration::from_secs(20)));

        // A later sample resets the deadline.
        transfer
            .sample(
                &mut handle,
                None,
                now + Duration::from_secs(25),
                &config,
                0,
                0,
                10_000_000,
            )
            .await
            .expect("a sample is applied");
        assert!(!transfer.is_inactive(now + Duration::from_secs(30)));

        // 0 disables the inactivity timeout, like the queue limits.
        config.timeout_secs = 0;
        transfer
            .sample(&mut handle, None, now, &config, 0, 0, 10_000_000)
            .await
            .expect("a sample is applied");
        assert!(!transfer.is_inactive(now + Duration::from_secs(1_000)));
        drop(status_tx);
    }

    #[tokio::test(start_paused = true)]
    async fn transfer_progress_tolerates_one_slow_sample_window() {
        // The floor is judged on the smoothed rate, so a single jittery ~120 KB
        // window cannot permanently abandon a peer that is otherwise far above
        // the floor. With the raw sample it would (240 KB/s < 400 KB/s).
        let mut config = default_dl_config();
        config.min_upload_speed_kbps = 400;
        config.speed_check_wait_secs = 30;
        let start = tokio::time::Instant::now();
        let mut transfer = TransferProgress::new();
        let (status_tx, status_rx) = mpsc::channel(1);
        drop(status_tx);
        let (cancel_tx, _cancel_rx) = mpsc::channel(1);
        let mut handle = DownloadHandle {
            status_rx,
            cancel_tx,
        };

        transfer.start(start, 10_000_000, "01.flac", None);
        // Past the wait period: two fast windows, then one 120 KB window at
        // 240 KB/s (the instantaneous rate the vendor would report for a hiccup).
        for (secs, speed) in [(31_u64, 5_000_000_u64), (32, 5_000_000), (33, 245_760)] {
            transfer
                .sample(
                    &mut handle,
                    None,
                    start + Duration::from_secs(secs),
                    &config,
                    speed,
                    100,
                    10_000_000,
                )
                .await
                .expect("a single slow window must not end the attempt");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_resumed_transfer_is_judged_on_its_own_first_sample() {
        // The zero-speed handshake must not seed the average either: seeding it
        // held every later sample down (0.7^k of the zero each time), so the first
        // judged sample measured a peer at a fraction of its real rate and — with
        // the permanent classification — abandoned it. 480 KB/s against a 400 KB/s
        // floor must survive with `speed_check_wait_secs: 0`.
        let client = ScriptedClient::new(vec![vec![
            status_step(
                Duration::from_secs(1),
                in_progress_sample(5_000_000, 0, 10_000_000),
            ),
            status_step(
                Duration::from_secs(1),
                in_progress_sample(6_000_000, 491_520, 10_000_000),
            ),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.min_upload_speed_kbps = 400;
        config.speed_check_wait_secs = 0;
        config.max_retries = 0;

        let (_dir, result) = download_with_named_script(
            &client,
            1,
            "Music\\Artist\\Album\\resumed-floor.flac",
            "resumed-floor-peer",
            &config,
        )
        .await;

        assert!(
            result.is_ok(),
            "a peer above the floor must not be judged on an average seeded by the \
             handshake's zero: {result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_outranks_the_speed_verdict() {
        // The queue limits and the stall timeout all let a cancellation outrank
        // the limit they tripped; the floor must too, or a Ctrl+C during the poll
        // window surfaces as a permanent speed failure instead of a cancellation.
        let cancel = Arc::new(AtomicBool::new(false));
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1_900)).await;
            trigger.store(true, Ordering::SeqCst);
        });
        let client = ScriptedClient::new(vec![vec![
            status_step(
                Duration::from_secs(1),
                in_progress_sample(120_000, 300_000, 10_000_000),
            ),
            // Due exactly at the end of the poll window the flag lands in, so the
            // verdict is evaluated with the cancellation already set.
            status_step(
                Duration::from_secs(1),
                in_progress_sample(240_000, 300_000, 10_000_000),
            ),
            status_step(Duration::from_secs(600), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.min_upload_speed_kbps = 400;
        config.speed_check_wait_secs = 0;
        config.max_retries = 0;

        let (_dir, result) =
            download_with_script(&client, 1, 10_000_000, &config, Some(&cancel)).await;

        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("download cancelled by user"),
            "a cancellation during the poll window must outrank the floor verdict: {error:?}"
        );
        assert!(!matches!(error, SeakarrError::SlowDownload(_)));
        assert_eq!(client.cancellations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_outranks_the_stall_timeout() {
        // The accept clock ends an accepted-but-silent peer; a cancellation that
        // landed in the final poll window must be what the attempt reports, the
        // same way it does for both queue limits.
        let cancel = Arc::new(AtomicBool::new(false));
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(2_900)).await;
            trigger.store(true, Ordering::SeqCst);
        });
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), in_progress_sample(0, 0, 10_000_000)),
            status_step(Duration::from_secs(600), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.timeout_secs = 2;
        config.max_retries = 0;

        let (_dir, result) =
            download_with_script(&client, 1, 10_000_000, &config, Some(&cancel)).await;

        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("download cancelled by user"),
            "a cancellation during the stall window must outrank the timeout: {error:?}"
        );
        assert_eq!(client.cancellations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_accepted_but_silent_peer_is_retried_like_any_stall() {
        // A stall — accepted, then no `InProgress` at all — stays *retryable*, as
        // it was before the byte gate: only the below-floor verdict is permanent.
        // Pinned so the choice is explicit: a pre-start stall does re-request the
        // same peer, at the cost of one retry window per attempt.
        let client = ScriptedClient::new(vec![
            vec![
                status_step(Duration::from_secs(1), in_progress_sample(0, 0, 10_000_000)),
                status_step(Duration::from_secs(600), DownloadStatus::Completed),
            ],
            vec![
                status_step(
                    Duration::from_secs(1),
                    in_progress_sample(10_000_000, 5_000_000, 10_000_000),
                ),
                status_step(Duration::from_secs(1), DownloadStatus::Completed),
            ],
        ]);
        let mut config = default_dl_config();
        config.timeout_secs = 20;
        config.max_retries = 1;
        config.retry_delay_secs = 0;

        let (_dir, result) = download_with_named_script(
            &client,
            1,
            "Music\\Artist\\Album\\stalled.flac",
            "stalled-peer",
            &config,
        )
        .await;

        assert!(
            result.is_ok(),
            "the stall must be retried on the same peer: {result:?}"
        );
        assert_eq!(client.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn transfer_progress_creates_no_bar_for_a_zero_length_file() {
        // indicatif renders a zero-length bar as 100%, so a zero-length transfer
        // must not get one at all: the bar is what the lazy creation point avoids.
        let display = ProgressDisplay::new();
        let mut transfer = TransferProgress::new();
        transfer.start(tokio::time::Instant::now(), 0, "empty.flac", Some(&display));
        assert!(transfer.bar.is_none());
        assert!(transfer.has_started(), "the transfer clock still starts");
    }

    #[tokio::test(start_paused = true)]
    async fn transfer_progress_bounds_a_peer_that_accepts_and_goes_silent() {
        // `note_accepted` covers the accept->first-byte phase. Without it a peer
        // that accepts and then sends nothing has no bound at all once both queue
        // limits are configured to 0 — the attempt would hold a concurrency slot,
        // the transfer thread and the `.part` file until Ctrl+C.
        let mut config = default_dl_config();
        config.timeout_secs = 20;
        let now = tokio::time::Instant::now();
        let mut transfer = TransferProgress::new();
        assert!(
            !transfer.is_inactive(now),
            "nothing is armed before the peer accepts"
        );

        transfer.note_accepted(now, &config);
        assert!(!transfer.has_started(), "accepting is not starting");
        assert!(!transfer.is_inactive(now + Duration::from_secs(19)));
        assert!(transfer.is_inactive(now + Duration::from_secs(20)));

        // A later status re-arms it, so a peer that dribbles statuses is not cut
        // off by the accept clock.
        transfer.note_accepted(now + Duration::from_secs(25), &config);
        assert!(!transfer.is_inactive(now + Duration::from_secs(30)));

        // 0 disables the inactivity timeout, like the queue limits.
        config.timeout_secs = 0;
        transfer.note_accepted(now, &config);
        assert!(!transfer.is_inactive(now + Duration::from_secs(1_000)));
    }

    #[tokio::test(start_paused = true)]
    async fn transfer_progress_records_the_duration_from_the_accept() {
        // The accept-to-first-byte phase is transfer time, not queue wait: a peer
        // that accepts and then stalls before its first sample must not be
        // credited with a fast transfer, and a sub-120 KB file must not record
        // 0.0 (the vendor's first sample needs ~120 KB of reads).
        let mut config = default_dl_config();
        config.timeout_secs = 180;
        let accepted = tokio::time::Instant::now();
        let mut transfer = TransferProgress::new();
        assert_eq!(
            transfer.elapsed(),
            Duration::ZERO,
            "nothing is recorded before the peer accepts"
        );

        transfer.note_accepted(accepted, &config);
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(transfer.elapsed(), Duration::from_secs(30));

        let (status_tx, status_rx) = mpsc::channel(1);
        drop(status_tx);
        let (cancel_tx, _cancel_rx) = mpsc::channel(1);
        let mut handle = DownloadHandle {
            status_rx,
            cancel_tx,
        };
        let started = tokio::time::Instant::now();
        transfer.start(started, 10_000_000, "01.flac", None);
        transfer
            .sample(
                &mut handle,
                None,
                started,
                &config,
                1_000_000,
                100,
                10_000_000,
            )
            .await
            .expect("a sample is applied");
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(
            transfer.elapsed(),
            Duration::from_secs(40),
            "the stall before the first byte is transfer time, not queue time"
        );
    }

    // ── Queue-aware download state machine ──
    //
    // These tests drive the state machine with simulated time
    // (`start_paused = true`) through `ScriptedClient`, which replays a
    // fixed status script per `download()` call. The last scripted status
    // may leave the attempt waiting: the client holds the status sender open
    // until seakarr cancels, so a cancelled attempt records the cancellation
    // instead of ending the script early.

    #[derive(Clone)]
    struct StatusStep {
        after: Duration,
        /// `None` ends the script by dropping the sender, so the application
        /// observes the status channel closing mid-attempt.
        status: Option<DownloadStatus>,
    }

    fn status_step(after: Duration, status: DownloadStatus) -> StatusStep {
        StatusStep {
            after,
            status: Some(status),
        }
    }

    /// A script step that closes the status channel instead of reporting a
    /// status. The script otherwise holds its sender open for the whole attempt,
    /// which makes the channel-close path unreachable from a script.
    fn close_step(after: Duration) -> StatusStep {
        StatusStep {
            after,
            status: None,
        }
    }

    struct ScriptedClient {
        scripts: Mutex<std::collections::VecDeque<Vec<StatusStep>>>,
        calls: std::sync::atomic::AtomicUsize,
        cancellations: Arc<std::sync::atomic::AtomicUsize>,
        usernames: Mutex<Vec<String>>,
        position_asks: Mutex<Vec<(String, String)>>,
    }

    impl ScriptedClient {
        fn new(scripts: Vec<Vec<StatusStep>>) -> Self {
            Self {
                scripts: Mutex::new(scripts.into()),
                calls: std::sync::atomic::AtomicUsize::new(0),
                cancellations: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                usernames: Mutex::new(Vec::new()),
                position_asks: Mutex::new(Vec::new()),
            }
        }

        /// Every position ask this client received, in call order.
        fn asks(&self) -> Vec<(String, String)> {
            self.position_asks.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SoulseekClient for ScriptedClient {
        async fn login(&self, _u: &str, _p: &str, _s: &str, _port: u16) -> Result<()> {
            Ok(())
        }

        async fn search(&self, _query: &str, _timeout_secs: u64) -> Result<Vec<SearchResult>> {
            Ok(Vec::new())
        }

        async fn download(
            &self,
            _file: &FileInfo,
            username: &str,
            _dir: &Path,
        ) -> Result<DownloadHandle> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.usernames.lock().unwrap().push(username.to_string());
            let script = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .expect("one status script per download call");
            let (status_tx, status_rx) = mpsc::channel(16);
            let (cancel_tx, mut cancel_rx) = mpsc::channel(1);
            let cancellations = self.cancellations.clone();
            tokio::spawn(async move {
                for step in script {
                    tokio::select! {
                        _ = tokio::time::sleep(step.after) => {
                            match step.status {
                                Some(status) => {
                                    if status_tx.send(status).await.is_err() {
                                        return;
                                    }
                                }
                                // Drop the sender: the application sees the
                                // channel close rather than another status.
                                None => return,
                            }
                        }
                        cancelled = cancel_rx.recv() => {
                            if cancelled.is_some() {
                                cancellations.fetch_add(1, Ordering::SeqCst);
                                let _ = status_tx.send(DownloadStatus::Failed {
                                    reason: "cancelled".into(),
                                }).await;
                            }
                            return;
                        }
                    }
                }
                if cancel_rx.recv().await.is_some() {
                    cancellations.fetch_add(1, Ordering::SeqCst);
                    let _ = status_tx
                        .send(DownloadStatus::Failed {
                            reason: "cancelled".into(),
                        })
                        .await;
                }
            });
            Ok(DownloadHandle {
                status_rx,
                cancel_tx,
            })
        }

        async fn request_queue_position(&self, username: &str, filename: &str) -> bool {
            self.position_asks
                .lock()
                .unwrap()
                .push((username.to_string(), filename.to_string()));
            true
        }
    }

    /// Run one scripted attempt through the private candidate entry point and
    /// return the staging dir (kept alive for the caller) plus the result.
    async fn download_with_script(
        client: &ScriptedClient,
        peer_slots: u8,
        file_size: u64,
        config: &DownloadConfig,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> (TempDir, Result<(PathBuf, f64)>) {
        let dir = TempDir::new().unwrap();
        let file = make_file("Music\\Artist\\Album\\01.flac", 900, file_size);
        let result = download_file_for_candidate(
            client,
            &file,
            "peer",
            peer_slots,
            dir.path(),
            config,
            &default_filter_config_test(),
            None,
            cancel,
        )
        .await;
        (dir, result)
    }

    /// Run one scripted attempt whose log lines cannot be confused with another
    /// test's.
    ///
    /// `LogCapture` keeps recording while other tests run, and almost every test
    /// in this module downloads `Music\Artist\Album\01.flac` from `peer`. A
    /// count assertion on those strings can therefore be satisfied by a
    /// concurrent test's identical line, so a capturing test has to key on a
    /// file and peer name only its own fixture produces.
    async fn download_with_named_script(
        client: &ScriptedClient,
        peer_slots: u8,
        share_path: &str,
        username: &str,
        config: &DownloadConfig,
    ) -> (TempDir, Result<(PathBuf, f64)>) {
        let dir = TempDir::new().unwrap();
        let file = make_file(share_path, 900, 10_000_000);
        let result = download_file_for_candidate(
            client,
            &file,
            username,
            peer_slots,
            dir.path(),
            config,
            &default_filter_config_test(),
            None,
            None,
        )
        .await;
        (dir, result)
    }

    /// Run one scripted attempt with a progress display, so queue-bar lifecycle
    /// can be observed. Mirrors `download_with_script`.
    async fn download_with_script_and_progress(
        client: &ScriptedClient,
        peer_slots: u8,
        display: &ProgressDisplay,
        config: &DownloadConfig,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> (TempDir, Result<(PathBuf, f64)>) {
        let dir = TempDir::new().unwrap();
        let file = make_file("Music\\Artist\\Album\\01.flac", 900, 10_000_000);
        let result = download_file_for_candidate(
            client,
            &file,
            "peer",
            peer_slots,
            dir.path(),
            config,
            &default_filter_config_test(),
            Some(display),
            cancel,
        )
        .await;
        (dir, result)
    }

    #[tokio::test(start_paused = true)]
    async fn queue_bar_is_replaced_by_the_transfer_bar_and_released() {
        // Two in-bound positions: the second must update the existing bar in place.
        // Without a second observation the update arm is never exercised, and
        // creating another bar instead of updating would pass unnoticed.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(7)),
            status_step(Duration::from_secs(1), queue_position(3)),
            status_step(Duration::from_secs(60), in_progress(10_000_000)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let display = ProgressDisplay::new();
        // A positive cap is what admits a zero-slot candidate to the queue at all.
        let mut config = default_dl_config();
        config.max_queue_length = 50;

        let (_dir, result) =
            download_with_script_and_progress(&client, 0, &display, &config, None).await;
        assert!(
            result.is_ok(),
            "expected a completed transfer, got {result:?}"
        );

        assert_eq!(
            display.queue_bars_created(),
            1,
            "the second position must update the existing bar, not create another"
        );
        assert_eq!(
            display.queue_bars_updated(),
            1,
            "the second position must go through the in-place update"
        );
        assert_eq!(
            display.queue_bars_finished(),
            1,
            "the queue bar must be released when the transfer takes over"
        );
        assert_eq!(
            display.created_bars(),
            1,
            "the transfer bar must still be created at transfer start"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn queue_bar_is_released_when_a_position_is_rejected() {
        // An out-of-bound position abandons the attempt. The bar must not be
        // left on the terminal by a path that never starts transferring.
        //
        // The first position is in bound, so a bar exists by the time the
        // out-of-bound one arrives — a rejection only has something to release
        // once an accepted position has put a bar on the terminal.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(3)),
            status_step(Duration::from_secs(1), queue_position(500)),
        ]]);
        let display = ProgressDisplay::new();
        let mut config = default_dl_config();
        config.max_queue_length = 5;

        let (_dir, result) =
            download_with_script_and_progress(&client, 0, &display, &config, None).await;
        assert!(
            result.is_err(),
            "an out-of-bound position must reject the attempt"
        );

        assert_eq!(display.queue_bars_created(), 1);
        assert_eq!(
            display.queue_bars_finished(),
            1,
            "rejection must release the queue bar"
        );
        assert_eq!(
            display.created_bars(),
            0,
            "a rejected attempt must never create a transfer bar"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn queue_bar_is_released_when_the_queue_deadline_expires() {
        let client = ScriptedClient::new(vec![vec![status_step(
            Duration::from_secs(1),
            queue_position(9),
        )]]);
        let display = ProgressDisplay::new();
        let mut config = default_dl_config();
        config.max_queue_length = 50;
        config.max_queue_time_secs = 60;

        let (_dir, result) =
            download_with_script_and_progress(&client, 0, &display, &config, None).await;
        assert!(result.is_err(), "expected a queue timeout, got {result:?}");

        assert_eq!(
            display.queue_bars_created(),
            1,
            "the queue bar must exist before the expiry path can release it"
        );
        assert_eq!(
            display.queue_bars_finished(),
            1,
            "every queue bar must be released on the timeout path"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn queue_bar_is_released_when_an_attempt_completes_without_progress() {
        // A position is reported and the transfer then completes without ever
        // sending InProgress. `SoulseekClient` is public API, so a client is free
        // to do that; the transfer-start release is never reached, and only the
        // Completed arm can release the bar.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(7)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let display = ProgressDisplay::new();
        let config = default_dl_config();

        let (_dir, result) =
            download_with_script_and_progress(&client, 1, &display, &config, None).await;
        assert!(
            result.is_ok(),
            "expected a completed transfer, got {result:?}"
        );

        assert_eq!(display.queue_bars_created(), 1);
        assert_eq!(
            display.queue_bars_finished(),
            1,
            "a completion that never sent InProgress must still release the bar"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn queue_bar_is_released_when_the_attempt_is_cancelled() {
        let cancel = Arc::new(AtomicBool::new(false));
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1_500)).await;
            trigger.store(true, Ordering::SeqCst);
        });
        let client = ScriptedClient::new(vec![vec![status_step(
            Duration::from_millis(100),
            queue_position(4),
        )]]);
        let display = ProgressDisplay::new();
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.max_queue_length = 50;
        config.max_queue_time_secs = 60;

        let (_dir, result) =
            download_with_script_and_progress(&client, 0, &display, &config, Some(&cancel)).await;
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("download cancelled by user"),
            "expected a cancellation, got {error:?}"
        );
        assert_eq!(display.queue_bars_created(), 1);
        assert_eq!(
            display.queue_bars_finished(),
            1,
            "cancellation must release the queue bar"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn queue_bar_is_released_when_the_channel_closes() {
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(7)),
            close_step(Duration::from_secs(1)),
        ]]);
        let display = ProgressDisplay::new();
        let mut config = default_dl_config();
        config.max_retries = 0;

        let (_dir, result) =
            download_with_script_and_progress(&client, 1, &display, &config, None).await;
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("channel closed"),
            "expected a channel-close failure, got {error:?}"
        );
        assert_eq!(display.queue_bars_created(), 1);
        assert_eq!(
            display.queue_bars_finished(),
            1,
            "a closed channel must release the queue bar"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn position_after_the_grace_reaches_the_started_line() {
        // The grace expires first, so the queued line carries no position; the
        // peer's late answer must then reach the started line's last-position
        // field rather than a second INFO line.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(10), queue_position(3)),
            status_step(Duration::from_secs(60), in_progress(10_000_000)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.max_queue_length = 50;
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_named_script(
            &client,
            0,
            "Music\\Artist\\Album\\late-position.flac",
            "late-position-peer",
            &config,
        )
        .await;
        assert!(
            result.is_ok(),
            "expected a completed transfer, got {result:?}"
        );

        let logs = capture.text();
        assert_eq!(
            logs.matches("Download queued: late-position.flac").count(),
            1,
            "the grace emits the line once and a late position adds no INFO line, got:\n{logs}"
        );
        let queued = logs
            .lines()
            .find(|line| line.contains("Download queued: late-position.flac"))
            .unwrap_or_else(|| panic!("no queued line for this fixture, got:\n{logs}"));
        assert!(
            queued.ends_with("Download queued: late-position.flac from late-position-peer"),
            "the grace form carries no position, got: {queued}"
        );
        assert!(
            logs.contains(
                "Download started: late-position.flac from late-position-peer after 1m 10s queued (last position 3)"
            ),
            "the started line must carry the late position, got:\n{logs}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn grace_fires_while_a_peer_floods_position_zero() {
        // A peer that answers position-0 more often than the poll window keeps the
        // status poll alive: each reply maps to `Queued { queue_position: None }`,
        // which the position filter drops and which resets the poll timeout. If the
        // grace were only evaluated in the poll-timeout arm it could be starved for
        // the whole queue wait, so no queued line would ever be emitted.
        let mut steps: Vec<StatusStep> = (0..200)
            .map(|_| {
                status_step(
                    Duration::from_millis(100),
                    DownloadStatus::Queued {
                        queue_position: None,
                    },
                )
            })
            .collect();
        steps.push(status_step(
            Duration::from_secs(30),
            in_progress(10_000_000),
        ));
        let client = ScriptedClient::new(vec![steps]);

        let mut config = default_dl_config();
        config.max_retries = 0;
        config.max_queue_length = 50;
        // Longer than the 5s notice grace, shorter than the 20s of flooding.
        config.max_queue_time_secs = 8;
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_named_script(
            &client,
            3,
            "Music\\Artist\\Album\\flooded.flac",
            "flooded-peer",
            &config,
        )
        .await;
        assert!(
            result.is_err(),
            "the queue limit must end the wait, got {result:?}"
        );

        let logs = capture.text();
        assert_eq!(
            logs.matches("Download queued: flooded.flac").count(),
            1,
            "the grace must fire during the flood, got:\n{logs}"
        );
        let queued = logs
            .lines()
            .find(|line| line.contains("Download queued: flooded.flac"))
            .unwrap_or_else(|| panic!("no queued line for this fixture, got:\n{logs}"));
        assert!(
            queued.ends_with("Download queued: flooded.flac from flooded-peer"),
            "no position was ever reported, so none may be printed, got: {queued}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn queue_bar_is_released_when_a_queued_peer_fails() {
        // A realistic path: the peer reports a position and then refuses the
        // upload. Deleting the release in the Failed arm would otherwise leave the
        // whole suite green.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(4)),
            status_step(
                Duration::from_secs(1),
                DownloadStatus::Failed {
                    reason: "peer refused".into(),
                },
            ),
        ]]);
        let display = ProgressDisplay::new();
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.max_queue_length = 50;

        let (_dir, result) =
            download_with_script_and_progress(&client, 0, &display, &config, None).await;
        assert!(
            result.is_err(),
            "a refused upload must fail the attempt, got {result:?}"
        );
        assert_eq!(display.queue_bars_created(), 1);
        assert_eq!(
            display.queue_bars_finished(),
            1,
            "a failed queued attempt must release the queue bar"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_completion_without_progress_still_reports_a_started_line() {
        // A client may report a position and then complete without ever sending
        // InProgress. That attempt never enters the transfer-start block, so the
        // started line must be emitted from the Completed arm: the design promises
        // one started line per downloaded file.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(7)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let config = default_dl_config();
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_named_script(
            &client,
            1,
            "Music\\Artist\\Album\\no-progress.flac",
            "no-progress-peer",
            &config,
        )
        .await;
        assert!(
            result.is_ok(),
            "expected a completed transfer, got {result:?}"
        );

        let logs = capture.text();
        assert!(
            logs.contains(
                "Download started: no-progress.flac from no-progress-peer after 2s queued (last position 7)"
            ),
            "the started line must be emitted for a completion that skipped InProgress, got:\n{logs}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_below_floor_peer_is_not_retried_in_place() {
        // A peer that cannot reach `min_upload_speed_kbps` cannot get faster by
        // being asked again: re-requesting it drops the file back to the rear of
        // the same queue. The reported bug is exactly that cycle (position
        // 7 -> 6 -> 6 -> 5, each attempt aborted ~30s after "Download
        // started"), so the abort must fail the candidate and let
        // `download_album` fall back to the next ranked peer.
        //
        // The second script exists so the test proves the point by observation:
        // if the attempt were retried in place, the mock would serve a fast
        // transfer and the call count would be 2.
        let client = ScriptedClient::new(vec![
            vec![
                status_step(
                    Duration::from_secs(1),
                    in_progress_sample(120_000, 300_000, 10_000_000),
                ),
                status_step(
                    Duration::from_secs(31),
                    in_progress_sample(240_000, 300_000, 10_000_000),
                ),
                status_step(Duration::from_secs(60), DownloadStatus::Completed),
            ],
            vec![
                status_step(
                    Duration::from_secs(1),
                    in_progress_sample(10_000_000, 5_000_000, 10_000_000),
                ),
                status_step(Duration::from_secs(1), DownloadStatus::Completed),
            ],
        ]);
        let mut config = default_dl_config();
        config.min_upload_speed_kbps = 400;
        config.speed_check_wait_secs = 30;
        config.max_retries = 1;

        let (_dir, result) = download_with_script(&client, 1, 10_000_000, &config, None).await;

        let reason = match result {
            Err(e) => e.to_string(),
            Ok(done) => panic!("a below-floor peer must fail the attempt, got {done:?}"),
        };
        assert!(
            reason.contains("below minimum 400 KB/s"),
            "the abort must name the speed floor, got: {reason}"
        );
        assert_eq!(
            client.calls.load(Ordering::SeqCst),
            1,
            "a below-floor peer must not be re-queued in place"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_offset_handshake_report_does_not_start_the_transfer() {
        // The vendored client reports the resume offset with speed 0.0 as soon
        // as the peer answers TransferResponse(allowed), before any byte is read
        // (download_peer.rs `start_transfer`). Counting that echo as the transfer
        // start began the speed clock while the peer was still draining its own
        // queue, so a peer that sent nothing for the first 30s was cancelled for
        // being too slow and then retried. The start clock must wait for real
        // bytes, which keeps the queue limits in charge while the peer is starved.
        let client = ScriptedClient::new(vec![vec![
            // Offset handshake echo: nothing downloaded yet.
            status_step(Duration::from_secs(1), in_progress_sample(0, 0, 10_000_000)),
            // 31s later the first real bytes arrive, far below the 400 KB/s floor.
            status_step(
                Duration::from_secs(31),
                in_progress_sample(120_000, 3_072, 10_000_000),
            ),
            status_step(
                Duration::from_secs(1),
                in_progress_sample(10_000_000, 5_000_000, 10_000_000),
            ),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.min_upload_speed_kbps = 400;
        config.speed_check_wait_secs = 30;
        // No retry: the starved-window abort must surface as this attempt's
        // failure, not as a second call that runs out of scripts.
        config.max_retries = 0;
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_named_script(
            &client,
            1,
            "Music\\Artist\\Album\\starved.flac",
            "starved-peer",
            &config,
        )
        .await;

        assert!(
            result.is_ok(),
            "a peer that only starts after its own queue drains must not be cancelled: {result:?}"
        );
        let logs = capture.text();
        assert_eq!(
            logs.matches("Download started: starved.flac").count(),
            1,
            "exactly one started line per file, got:\n{logs}"
        );
        assert!(
            logs.contains("Download started: starved.flac from starved-peer after 1s queued"),
            "the started line reports the queue wait, which ended at the peer's accept (1s), \
             not the starved interval before the first byte, got:\n{logs}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_accepted_peer_that_never_sends_bytes_is_timed_out() {
        // The peer answers the transfer request with a zero-byte offset handshake
        // and then goes silent. The queue limits are at their defaults, so the
        // accept clock is the only bound: without it the attempt would hold a
        // concurrency slot, the transfer thread and the `.part` file for half an
        // hour — and forever when both queue limits are configured to 0.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), in_progress_sample(0, 0, 10_000_000)),
            // Far beyond the timeout, so the attempt must end before this arrives.
            status_step(Duration::from_secs(600), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.timeout_secs = 20;
        config.max_retries = 0;
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_named_script(
            &client,
            1,
            "Music\\Artist\\Album\\silent.flac",
            "silent-peer",
            &config,
        )
        .await;

        let reason = match result {
            Err(SeakarrError::Download(reason)) => reason,
            other => panic!("expected the retryable stall timeout, got {other:?}"),
        };
        assert!(
            reason.contains("timed out"),
            "the accept clock must end the attempt, got: {reason}"
        );
        let logs = capture.text();
        assert!(
            !logs.contains("Download started: silent.flac"),
            "a peer that sent no byte must not be reported as started, got:\n{logs}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_byte_less_progress_status_does_not_disarm_the_queue_limit() {
        // The offset handshake is an `InProgress`, but it is not progress: the
        // queue limit must still be the deadline that ends this wait. A
        // regression that let the handshake count as the transfer start (arming
        // the transfer deadline and retiring the queue limits) changes the
        // outcome to a stall timeout.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), in_progress_sample(0, 0, 10_000_000)),
            status_step(Duration::from_secs(600), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.max_queue_time_secs = 5;
        config.max_retries = 0;

        let (_dir, result) = download_with_named_script(
            &client,
            1,
            "Music\\Artist\\Album\\byte-less.flac",
            "byte-less-peer",
            &config,
        )
        .await;

        let reason = queue_timeout_reason(&result);
        assert!(
            reason.contains("max_queue_time_secs"),
            "the queue limit must be what ends the wait, got: {reason}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_resumed_handshake_starts_the_clock_without_a_speed_verdict() {
        // A resumed transfer's handshake reports the surviving `.part` size with a
        // zero speed. Those bytes start the clock, but the sample must not be
        // judged against the speed floor: with `speed_check_wait_secs: 0` a zero
        // speed would otherwise fail a healthy peer on the spot.
        let client = ScriptedClient::new(vec![vec![
            status_step(
                Duration::from_secs(1),
                in_progress_sample(4_000_000, 0, 10_000_000),
            ),
            status_step(
                Duration::from_secs(1),
                in_progress_sample(10_000_000, 5_000_000, 10_000_000),
            ),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.min_upload_speed_kbps = 400;
        config.speed_check_wait_secs = 0;
        config.max_retries = 0;
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_named_script(
            &client,
            1,
            "Music\\Artist\\Album\\resumed.flac",
            "resumed-peer",
            &config,
        )
        .await;

        assert!(
            result.is_ok(),
            "the resume echo must not be judged against the speed floor: {result:?}"
        );
        let logs = capture.text();
        assert!(
            logs.contains("Download started: resumed.flac from resumed-peer after 1s queued"),
            "the resume echo must start the clock, got:\n{logs}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_peer_releases_its_queue_bar() {
        // The queue bar survives until the first byte, so the stall timeout is a
        // pre-start exit that must release it. The poll-timeout arm is the usual
        // observer for a peer that accepts and then goes silent, and leaving the
        // bar behind would keep a spinner on the terminal — once per retry.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(5)),
            status_step(Duration::from_secs(1), in_progress_sample(0, 0, 10_000_000)),
            status_step(Duration::from_secs(600), DownloadStatus::Completed),
        ]]);
        let display = ProgressDisplay::new();
        let mut config = default_dl_config();
        config.max_queue_length = 50;
        config.timeout_secs = 20;
        config.max_retries = 0;

        let (_dir, result) =
            download_with_script_and_progress(&client, 0, &display, &config, None).await;

        assert!(
            result.is_err(),
            "an accepted peer that sends nothing must be timed out, got {result:?}"
        );
        assert_eq!(display.queue_bars_created(), 1);
        assert_eq!(
            display.queue_bars_finished(),
            1,
            "the stall timeout must release the queue bar"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_zero_slot_candidate_cannot_complete_without_a_position() {
        // The fail-closed rule has a second arm, reached only by a completion
        // that never sent progress: a peer must not be accepted just because it
        // finished, and the rejection must come before any started line.
        let client = ScriptedClient::new(vec![vec![status_step(
            Duration::from_secs(1),
            DownloadStatus::Completed,
        )]]);
        let mut config = default_dl_config();
        config.max_queue_length = 50;
        config.max_retries = 0;
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_named_script(
            &client,
            0,
            "Music\\Artist\\Album\\unproven.flac",
            "unproven-peer",
            &config,
        )
        .await;

        let reason = queue_timeout_reason(&result);
        assert!(
            reason.contains("completed without a queue position"),
            "the completion must be rejected as an unproven position, got: {reason}"
        );
        let logs = capture.text();
        assert!(
            !logs.contains("Download started: unproven.flac"),
            "a rejected completion must not report a start, got:\n{logs}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_short_wait_without_a_position_is_not_called_immediate() {
        // The peer never reports a position and holds the transfer for longer than
        // one poll window. The log must report the wait instead of claiming the
        // transfer started immediately: an advertised free slot is a stale
        // snapshot, so "no position reported" does not mean "started at once".
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(2), in_progress(10_000_000)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let config = default_dl_config();
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_named_script(
            &client,
            1,
            "Music\\Artist\\Album\\slow-start.flac",
            "slow-start-peer",
            &config,
        )
        .await;
        assert!(
            result.is_ok(),
            "expected a completed transfer, got {result:?}"
        );

        let logs = capture.text();
        assert!(
            logs.contains("Download started: slow-start.flac from slow-start-peer after 2s queued"),
            "a 2s wait must be reported as a wait, got:\n{logs}"
        );
        assert!(
            !logs.contains("slow-start.flac from slow-start-peer immediately"),
            "a 2s wait must not be called immediate, got:\n{logs}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_short_queue_timeout_still_reports_the_queued_line() {
        // The notice grace is 5s but the queue limit is 3s, so the limit fires
        // first while the peer has reported nothing. The queued line must still be
        // emitted: otherwise a short limit silently hides the wait.
        let client = ScriptedClient::new(vec![vec![]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.max_queue_length = 50;
        config.max_queue_time_secs = 3;
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_named_script(
            &client,
            3,
            "Music\\Artist\\Album\\short-timeout.flac",
            "short-timeout-peer",
            &config,
        )
        .await;
        assert!(
            result.is_err(),
            "the queue limit must end the wait, got {result:?}"
        );

        let logs = capture.text();
        assert_eq!(
            logs.matches("Download queued: short-timeout.flac").count(),
            1,
            "the queue limit must not swallow the queued line, got:\n{logs}"
        );
        let queued = logs
            .lines()
            .find(|line| line.contains("Download queued: short-timeout.flac"))
            .unwrap_or_else(|| panic!("no queued line for this fixture, got:\n{logs}"));
        assert!(
            queued.ends_with("Download queued: short-timeout.flac from short-timeout-peer"),
            "no position was reported, so none may be printed, got: {queued}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_sub_second_wait_is_not_reported_as_zero() {
        // 900ms is past the immediacy window but under a whole second, so the
        // seconds-based formatter would otherwise print `after 0s queued`.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_millis(900), in_progress(10_000_000)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let config = default_dl_config();
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_named_script(
            &client,
            1,
            "Music\\Artist\\Album\\fast-start.flac",
            "fast-start-peer",
            &config,
        )
        .await;
        assert!(
            result.is_ok(),
            "expected a completed transfer, got {result:?}"
        );

        let logs = capture.text();
        assert!(
            logs.contains("Download started: fast-start.flac from fast-start-peer after 1s queued"),
            "a sub-second wait must not be reported as 0s, got:\n{logs}"
        );
        assert!(
            !logs.contains("fast-start.flac from fast-start-peer after 0s"),
            "the wait must never read as zero, got:\n{logs}"
        );
    }

    fn queue_timeout_reason(result: &Result<(PathBuf, f64)>) -> String {
        match result {
            Err(SeakarrError::QueueTimeout(reason)) => reason.clone(),
            other => panic!("expected a QueueTimeout, got {other:?}"),
        }
    }

    fn in_progress(total_bytes: u64) -> DownloadStatus {
        DownloadStatus::InProgress {
            speed_bytes_per_sec: 1_024,
            bytes_downloaded: total_bytes,
            total_bytes,
        }
    }

    /// An `InProgress` sample with explicit byte count and speed.
    ///
    /// The vendored client sends two shapes: the offset-handshake echo
    /// (`part.written` bytes with speed `0.0`, emitted the moment the peer
    /// accepts the transfer and before a byte is read) and real progress
    /// samples (`bytes / elapsed`, always above zero). Tests that care about
    /// that difference need to set both fields, which `in_progress` cannot.
    fn in_progress_sample(
        bytes_downloaded: u64,
        speed_bytes_per_sec: u64,
        total_bytes: u64,
    ) -> DownloadStatus {
        DownloadStatus::InProgress {
            speed_bytes_per_sec,
            bytes_downloaded,
            total_bytes,
        }
    }

    fn queue_position(position: u32) -> DownloadStatus {
        DownloadStatus::Queued {
            queue_position: Some(position),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn queued_line_carries_the_first_observed_position() {
        // The position is not known when the download is enqueued, so the notice
        // is deferred to the first observation and the operator gets one line
        // with the position rather than one line with and one line without.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(42)),
            status_step(Duration::from_secs(60), in_progress(10_000_000)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        // A positive cap is what admits a zero-slot candidate: with the shipped
        // `max_queue_length: 0` the admission gate rejects `peer_slots == 0`
        // before any queue state exists.
        let mut config = default_dl_config();
        config.max_queue_length = 50;
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_named_script(
            &client,
            0,
            "Music\\Artist\\Album\\queue-notice.flac",
            "queue-notice-peer",
            &config,
        )
        .await;
        assert!(
            result.is_ok(),
            "expected a completed transfer, got {result:?}"
        );

        let logs = capture.text();
        assert_eq!(
            logs.matches("Download queued: queue-notice.flac").count(),
            1,
            "exactly one queued line, got:\n{logs}"
        );
        assert!(
            logs.contains(
                "Download queued: queue-notice.flac from queue-notice-peer - position 42"
            ),
            "the queued line must carry the position, got:\n{logs}"
        );
        assert!(
            logs.contains(
                "Download started: queue-notice.flac from queue-notice-peer after 1m 1s queued (last position 42)"
            ),
            "the started line must carry the wait and last position, got:\n{logs}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn free_slot_attempt_reports_an_immediate_start() {
        // A candidate admitted on an advertised free slot starts without a queue
        // position. It must still produce both lines, and the started line must
        // be distinguishable from a queued wait.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::ZERO, in_progress(10_000_000)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let config = default_dl_config();
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_named_script(
            &client,
            1,
            "Music\\Artist\\Album\\free-slot.flac",
            "free-slot-peer",
            &config,
        )
        .await;
        assert!(
            result.is_ok(),
            "expected a completed transfer, got {result:?}"
        );

        let logs = capture.text();
        // A unique share path and peer name: `LogCapture` keeps one process-wide
        // window, and a sibling test emits the same `immediately (free slot)`
        // line for the shared `Music\Artist\Album\01.flac` fixture. `ends_with`
        // also proves nothing follows the peer name, so a position suffix cannot
        // slip past the assertion.
        let queued = logs
            .lines()
            .find(|line| line.contains("Download queued: free-slot.flac"))
            .unwrap_or_else(|| panic!("no queued line for this fixture, got:\n{logs}"));
        assert!(
            queued.ends_with("Download queued: free-slot.flac from free-slot-peer"),
            "the queued line must carry no position suffix, got: {queued}"
        );
        assert!(
            logs.contains(
                "Download started: free-slot.flac from free-slot-peer immediately (free slot)"
            ),
            "a free-slot start must say so, got:\n{logs}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn silent_peer_still_gets_a_queued_line_at_the_grace_deadline() {
        // The whole point of the report: a peer that never answers must not
        // leave the run silent while the file waits.
        //
        // The silence is 30s: well past the 5s notice grace, and well inside
        // the 1800s `max_queue_time_secs` that `default_dl_config` sets — the
        // plan's sketch used 3600s, which the queue timeout ends long before
        // the scripted transfer starts.
        //
        // `peer_slots` is 1 because a zero-slot candidate with the shipped cap
        // of zero never reaches the queue at all; the free slot is what admits
        // this peer, and it simply never reports a position.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(30), in_progress(10_000_000)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let config = default_dl_config();
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_named_script(
            &client,
            1,
            "Music\\Artist\\Album\\queue-notice.flac",
            "queue-notice-peer",
            &config,
        )
        .await;
        assert!(
            result.is_ok(),
            "expected a completed transfer, got {result:?}"
        );

        let logs = capture.text();
        assert_eq!(
            logs.matches("Download queued: queue-notice.flac from queue-notice-peer")
                .count(),
            1,
            "the queued line must be emitted once, at the grace deadline, got:\n{logs}"
        );
        // Scoped to this fixture's own lines: another test's queued line may
        // legitimately carry a position while this window is open.
        assert!(
            !logs
                .lines()
                .filter(|line| line.contains("queue-notice.flac"))
                .any(|line| line.contains(" - position ")),
            "no position was reported, so none may be printed, got:\n{logs}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_queued_wait_re_asks_its_peer_every_thirty_seconds() {
        // A 95 s cap with a 30 s cadence gives three asks: at 30 s, 60 s and 90 s.
        // The cap is deliberately off the cadence boundary: the ask sits after the
        // loop's expiry checks, so an attempt that expires on an ask instant is not
        // asked at all (see no_position_ask_is_sent_when_the_attempt_expires).
        let client = ScriptedClient::new(vec![vec![status_step(
            Duration::from_secs(1),
            queue_position(20),
        )]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.max_queue_length = 50;
        config.max_queue_time_secs = 95;

        let (_dir, result) =
            download_with_named_script(&client, 0, "Music\\A\\B\\ask.flac", "ask-peer", &config)
                .await;

        assert!(result.is_err(), "the 95 s cap must end the wait");
        assert_eq!(
            client.asks(),
            vec![("ask-peer".to_string(), "Music\\A\\B\\ask.flac".to_string()); 3],
            "one ask per 30 s of waiting, carrying the peer's own path"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_position_ask_is_recorded_at_debug_level() {
        // Decision 9: the ask is observable at DEBUG and nowhere louder, so a
        // frozen number can be told apart from a stopped refresh. The fixture
        // names are unique because LogCapture keeps one process-wide window.
        let client = ScriptedClient::new(vec![vec![status_step(
            Duration::from_secs(1),
            queue_position(20),
        )]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.max_queue_length = 50;
        config.max_queue_time_secs = 35;
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_named_script(
            &client,
            0,
            "Music\\A\\B\\logged.flac",
            "logged-peer",
            &config,
        )
        .await;
        assert!(result.is_err(), "the 35 s cap must end the wait");

        let logs = capture.text();
        let line = logs
            .lines()
            .find(|line| line.contains("Queue position request for logged.flac from logged-peer"))
            .unwrap_or_else(|| panic!("no ask record for this fixture, got:\n{logs}"));
        assert_eq!(
            line.split_whitespace().next(),
            Some("DEBUG"),
            "the ask record must be debug-level: {line}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_position_ask_is_sent_when_the_attempt_expires() {
        // A wait that ends at the cap records no ask for the instant the cap lands
        // on: with a 90 s cap the asks due at 30 s and 60 s go out, and the one due
        // at 90 s does not. In a quiet wait the expiry is observed in the
        // poll-timeout arm rather than at the loop top, so this pins the cadence and
        // the count rather than the ask's exact position in the loop; whether the
        // loop top runs at the expiry instant is scheduler-dependent (see the expiry
        // arm's coverage note).
        let client = ScriptedClient::new(vec![vec![status_step(
            Duration::from_secs(1),
            queue_position(20),
        )]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.max_queue_length = 50;
        config.max_queue_time_secs = 90;

        let (_dir, result) = download_with_named_script(
            &client,
            0,
            "Music\\A\\B\\expire.flac",
            "expire-peer",
            &config,
        )
        .await;

        assert!(result.is_err(), "the 90 s cap must end the wait");
        assert_eq!(
            client.asks(),
            vec![
                (
                    "expire-peer".to_string(),
                    "Music\\A\\B\\expire.flac".to_string()
                );
                2
            ],
            "the ask due on the expiry instant must not be sent"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reported_positions_update_the_queue_bar_in_place() {
        // Repeated positions must reach the bar as one creation plus one update
        // each, so a deep queue costs one terminal line. This pins the bar's
        // update contract only - the ask that produces fresh positions is pinned
        // by a_queued_wait_re_asks_its_peer_every_thirty_seconds.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(20)),
            status_step(Duration::from_secs(30), queue_position(12)),
            status_step(Duration::from_secs(30), queue_position(3)),
            status_step(Duration::from_secs(1), in_progress(10_000_000)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let display = ProgressDisplay::new();
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.max_queue_length = 50;

        let (_dir, result) =
            download_with_script_and_progress(&client, 0, &display, &config, None).await;

        assert!(result.is_ok(), "the transfer must complete, got {result:?}");
        assert_eq!(
            display.queue_bars_created(),
            1,
            "the first reported position creates the bar"
        );
        assert_eq!(
            display.queue_bars_updated(),
            2,
            "each later position must update the bar in place"
        );
        assert_eq!(display.queue_bars_finished(), 1, "the bar is released once");
    }

    #[tokio::test(start_paused = true)]
    async fn no_position_ask_is_sent_before_the_refresh_interval() {
        // A 20 s cap ends the wait before the first 30 s ask is due.
        let client = ScriptedClient::new(vec![vec![status_step(
            Duration::from_secs(1),
            queue_position(20),
        )]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.max_queue_length = 50;
        config.max_queue_time_secs = 20;

        let (_dir, result) = download_with_script(&client, 0, 10_000_000, &config, None).await;

        assert!(result.is_err(), "the 20 s cap must end the wait");
        assert!(
            client.asks().is_empty(),
            "the crate already asked at enqueue; seakarr must not ask before 30 s, got {:?}",
            client.asks()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn position_asks_stop_once_the_transfer_starts() {
        // The transfer starts at 25 s, before the first ask is due, and the
        // attempt then runs on for a minute: a refresh that outlived the queue
        // would appear as asks at 30 s and 60 s.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(20)),
            status_step(Duration::from_secs(24), in_progress(10_000_000)),
            status_step(Duration::from_secs(60), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.max_queue_length = 50;
        config.max_queue_time_secs = 600;

        let (_dir, result) = download_with_script(&client, 0, 10_000_000, &config, None).await;

        assert!(result.is_ok(), "the transfer must complete, got {result:?}");
        assert!(
            client.asks().is_empty(),
            "a running transfer needs no position, got {:?}",
            client.asks()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn position_asks_stop_on_cancellation() {
        let cancel = Arc::new(AtomicBool::new(false));
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(25)).await;
            trigger.store(true, Ordering::SeqCst);
        });
        let client = ScriptedClient::new(vec![vec![status_step(
            Duration::from_secs(1),
            queue_position(20),
        )]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.max_queue_length = 50;
        config.max_queue_time_secs = 600;

        let (_dir, result) =
            download_with_script(&client, 0, 10_000_000, &config, Some(&cancel)).await;

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("download cancelled by user"),
            "cancellation must win"
        );
        assert!(
            client.asks().is_empty(),
            "a cancelled attempt must send nothing further, got {:?}",
            client.asks()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn position_changes_after_the_notice_are_debug_only() {
        // A queue hundreds deep must not produce one INFO line per step: the
        // depth problem is a volume problem, so mid-queue updates are demoted.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(42)),
            status_step(Duration::from_secs(60), queue_position(9)),
            status_step(Duration::from_secs(60), queue_position(1)),
            status_step(Duration::from_secs(60), in_progress(10_000_000)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        // Same admission gate as the position test above: the cap has to admit
        // the zero-slot candidate before any of these positions can arrive.
        let mut config = default_dl_config();
        config.max_queue_length = 50;
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_named_script(
            &client,
            0,
            "Music\\Artist\\Album\\queue-notice.flac",
            "queue-notice-peer",
            &config,
        )
        .await;
        assert!(
            result.is_ok(),
            "expected a completed transfer, got {result:?}"
        );

        let logs = capture.text();
        assert_eq!(
            logs.matches("Download queued: queue-notice.flac").count(),
            1,
            "only the first observation may reach INFO, got:\n{logs}"
        );
        assert!(
            logs.contains(
                "Download started: queue-notice.flac from queue-notice-peer after 3m 1s queued (last position 1)"
            ),
            "the started line must report the last position seen, got:\n{logs}"
        );
        let debug_lines = logs
            .lines()
            .filter(|line| line.contains("Queue position for queue-notice.flac"))
            .count();
        assert!(
            debug_lines >= 2,
            "every position change must be visible at DEBUG, got:\n{logs}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_failure_does_not_send_redundant_cancel() {
        let client = ScriptedClient::new(vec![vec![status_step(
            Duration::ZERO,
            DownloadStatus::Failed {
                reason: "peer denied transfer".to_string(),
            },
        )]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 60;

        let (_dir, result) = download_with_script(&client, 1, 1_024, &config, None).await;

        assert!(matches!(
            result,
            Err(SeakarrError::Download(reason)) if reason.contains("peer denied transfer")
        ));
        assert_eq!(
            client.cancellations.load(Ordering::SeqCst),
            0,
            "a terminal vendor status must not be cancelled and drained again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn direct_cap_zero_rejects_zero_slot_before_queueing() {
        let client =
            ScriptedClient::new(vec![vec![status_step(Duration::ZERO, queue_position(1))]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.max_queue_length = 0;

        let (_dir, result) = download_with_script(&client, 0, 1_024, &config, None).await;

        let reason = queue_timeout_reason(&result);
        assert!(reason.contains("requires an advertised free slot"));
        assert_eq!(
            client.calls.load(Ordering::SeqCst),
            0,
            "an invalid direct candidate must be rejected before contacting the peer"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn queued_wait_does_not_consume_transfer_timeout() {
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::ZERO, queue_position(2)),
            status_step(Duration::from_secs(2), in_progress(1_024)),
            status_step(Duration::from_millis(10), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 1;
        config.max_queue_length = 3;
        config.max_start_time_secs = 10;
        config.max_queue_time_secs = 10;

        let (_dir, result) = download_with_script(&client, 0, 1_024, &config, None).await;

        assert!(
            result.is_ok(),
            "two seconds of queue wait must not consume a one-second transfer timeout: {result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn total_queue_timeout_expires_queued_attempt() {
        let client =
            ScriptedClient::new(vec![vec![status_step(Duration::ZERO, queue_position(2))]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 60;
        config.max_queue_length = 3;
        config.max_start_time_secs = 0;
        config.max_queue_time_secs = 2;

        let (_dir, result) = download_with_script(&client, 0, 1_024, &config, None).await;

        let reason = queue_timeout_reason(&result);
        assert!(
            reason.contains("max_queue_time_secs=2"),
            "the reason must name the exhausted limit: {reason}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn queue_head_timeout_starts_at_position_one() {
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::ZERO, queue_position(2)),
            status_step(Duration::from_secs(2), queue_position(1)),
        ]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 60;
        config.max_queue_length = 3;
        config.max_start_time_secs = 1;
        config.max_queue_time_secs = 10;

        let (_dir, result) = download_with_script(&client, 0, 1_024, &config, None).await;

        let reason = queue_timeout_reason(&result);
        assert!(
            reason.contains("max_start_time_secs=1"),
            "the reason must name the queue-head limit: {reason}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn time_at_position_two_does_not_consume_start_limit() {
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::ZERO, queue_position(2)),
            status_step(Duration::from_secs(2), queue_position(1)),
            status_step(Duration::from_millis(500), in_progress(1_024)),
            status_step(Duration::from_millis(10), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 1;
        config.max_queue_length = 3;
        config.max_start_time_secs = 1;
        config.max_queue_time_secs = 10;

        let (_dir, result) = download_with_script(&client, 0, 1_024, &config, None).await;

        assert!(
            result.is_ok(),
            "the start limit may only count from the first position-1 observation: {result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pre_start_pause_does_not_start_transfer_timeout() {
        let client = ScriptedClient::new(vec![vec![
            status_step(
                Duration::ZERO,
                DownloadStatus::Paused {
                    bytes_downloaded: 0,
                    total_bytes: 1_024,
                },
            ),
            status_step(Duration::from_secs(2), in_progress(1_024)),
            status_step(Duration::from_millis(10), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 1;
        config.max_queue_time_secs = 10;

        let (_dir, result) = download_with_script(&client, 1, 1_024, &config, None).await;

        assert!(
            result.is_ok(),
            "a pre-start pause is queue time, not transfer inactivity: {result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn post_start_pause_does_not_reset_transfer_timeout() {
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::ZERO, in_progress(1_024)),
            status_step(
                Duration::from_millis(800),
                DownloadStatus::Paused {
                    bytes_downloaded: 0,
                    total_bytes: 1_024,
                },
            ),
            status_step(Duration::from_millis(400), in_progress(1_024)),
            status_step(Duration::from_millis(10), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 1;
        config.max_queue_time_secs = 10;

        let (_dir, result) = download_with_script(&client, 1, 1_024, &config, None).await;

        let error = result.expect_err("a one-second stall past the pause must time out");
        assert!(
            matches!(&error, SeakarrError::Download(reason) if reason.contains("timed out")),
            "a post-start pause is transfer inactivity, so the ordinary timeout applies: {error:?}"
        );
        assert!(!matches!(error, SeakarrError::QueueTimeout(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn zero_transfer_timeout_disables_the_inactivity_check() {
        // `timeout_secs: 0` means disabled, as it does for the queue limits, so a
        // long stall must not cancel the transfer.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::ZERO, in_progress(1_024)),
            status_step(Duration::from_secs(600), in_progress(1_024)),
            status_step(Duration::from_millis(10), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 0;
        config.max_queue_time_secs = 10;

        let (_dir, result) = download_with_script(&client, 1, 1_024, &config, None).await;

        assert!(
            result.is_ok(),
            "timeout_secs 0 must disable the inactivity timeout: {result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn zero_queue_timers_allow_delayed_start() {
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(2), in_progress(1_024)),
            status_step(Duration::from_millis(10), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 1;
        config.max_start_time_secs = 0;
        config.max_queue_time_secs = 0;

        let (_dir, result) = download_with_script(&client, 1, 1_024, &config, None).await;

        assert!(
            result.is_ok(),
            "zero queue timers disable queue expiry entirely: {result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn zero_queue_cap_preserves_admitted_free_slot_candidate() {
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::ZERO, queue_position(1)),
            status_step(Duration::from_millis(10), in_progress(1_024)),
            status_step(Duration::from_millis(10), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 60;
        config.max_queue_length = 0;

        let (_dir, result) = download_with_script(&client, 1, 1_024, &config, None).await;

        assert!(
            result.is_ok(),
            "a cap-zero candidate admitted with a free slot must not be retroactively rejected: {result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn positive_cap_rejects_out_of_bound_position() {
        let client =
            ScriptedClient::new(vec![vec![status_step(Duration::ZERO, queue_position(4))]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 60;
        config.max_queue_length = 3;

        let (_dir, result) = download_with_script(&client, 0, 1_024, &config, None).await;

        let reason = queue_timeout_reason(&result);
        assert!(
            reason.contains('4') && reason.contains('3'),
            "the reason must name the observed position and the cap: {reason}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_positive_cap_applies_the_position_check_even_with_a_free_slot() {
        // The free-slot exemption is scoped to `max_queue_length: 0`. With a
        // positive cap, an advertised free slot does not exempt a candidate from
        // the reported-position check — the README states exactly that.
        let client =
            ScriptedClient::new(vec![vec![status_step(Duration::ZERO, queue_position(4))]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 60;
        config.max_queue_length = 3;

        let (_dir, result) = download_with_script(&client, 1, 1_024, &config, None).await;

        let reason = queue_timeout_reason(&result);
        assert!(
            reason.contains('4') && reason.contains('3'),
            "a free-slot candidate above the cap must still be rejected by position: {reason}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wire_zero_position_is_ignored_for_free_slot_candidate() {
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::ZERO, queue_position(0)),
            status_step(Duration::from_millis(10), in_progress(1_024)),
            status_step(Duration::from_millis(10), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 60;
        config.max_queue_length = 3;

        let (_dir, result) = download_with_script(&client, 1, 1_024, &config, None).await;

        assert!(
            result.is_ok(),
            "wire position zero means no actionable queue position and must not abort a free-slot transfer: {result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wire_zero_position_does_not_prove_zero_slot_candidate() {
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::ZERO, queue_position(0)),
            status_step(Duration::from_millis(10), in_progress(1_024)),
        ]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 60;
        config.max_queue_length = 3;

        let (_dir, result) = download_with_script(&client, 0, 1_024, &config, None).await;

        let reason = queue_timeout_reason(&result);
        assert!(
            reason.contains("unknown queue position"),
            "wire position zero must not satisfy the positive-cap proof requirement: {reason}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn zero_slot_candidate_fails_closed_without_position() {
        let client =
            ScriptedClient::new(vec![vec![status_step(Duration::ZERO, in_progress(1_024))]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 60;
        config.max_queue_length = 3;

        let (_dir, result) = download_with_script(&client, 0, 1_024, &config, None).await;

        let reason = queue_timeout_reason(&result);
        assert!(
            reason.contains("unknown queue position"),
            "a candidate without a free slot must prove its position: {reason}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn free_slot_candidate_can_start_without_position() {
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::ZERO, in_progress(1_024)),
            status_step(Duration::from_millis(10), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 60;
        config.max_queue_length = 3;

        let (_dir, result) = download_with_script(&client, 1, 1_024, &config, None).await;

        assert!(
            result.is_ok(),
            "a candidate with a free slot may start without a queue position: {result:?}"
        );
    }

    // Queue timeouts are per-peer policy failures: waiting on the same peer
    // again cannot change its queue, so the retry loop must skip straight to
    // the next candidate instead of burning max_retries on a doomed peer.
    #[tokio::test(start_paused = true)]
    async fn queue_timeout_falls_back_without_same_peer_retry() {
        let client = ScriptedClient::new(vec![
            vec![status_step(Duration::ZERO, queue_position(1))],
            vec![
                status_step(Duration::ZERO, in_progress(1_024)),
                status_step(Duration::from_millis(10), DownloadStatus::Completed),
            ],
        ]);
        let dir = TempDir::new().unwrap();
        let candidates = vec![
            SearchResult {
                username: "queued-peer".into(),
                speed: 100,
                slots: 0,
                files: vec![make_file("Music\\Artist\\Album\\01.flac", 900, 1_024)],
            },
            SearchResult {
                username: "free-peer".into(),
                speed: 200,
                slots: 1,
                files: vec![make_file("Music\\Artist\\Album\\01.flac", 900, 1_024)],
            },
        ];
        let mut config = default_dl_config();
        config.max_retries = 3;
        config.retry_delay_secs = 0;
        config.timeout_secs = 60;
        config.max_queue_length = 3;
        config.max_start_time_secs = 1;
        config.max_queue_time_secs = 10;

        let result = download_album(
            &client,
            &candidates,
            dir.path(),
            &config,
            &default_filter_config_test(),
            None,
            None,
            &mut DownloadStats::default(),
        )
        .await;

        assert!(
            result.is_ok(),
            "the free-slot peer must serve the file: {result:?}"
        );
        // One attempt per peer: a retryable queue error would call the
        // queued peer four times (1 + max_retries) before falling back.
        assert_eq!(client.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            client.usernames.lock().unwrap().as_slice(),
            ["queued-peer", "free-peer"]
        );
        assert_eq!(client.cancellations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn queue_wait_is_excluded_from_effective_throughput() {
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::ZERO, queue_position(1)),
            status_step(Duration::from_secs(5), in_progress(1_024)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 60;
        config.max_queue_length = 3;
        config.max_start_time_secs = 60;
        config.max_queue_time_secs = 60;

        let (_dir, result) = download_with_script(&client, 0, 1_024, &config, None).await;

        let (_, speed_kbps) = result.expect("a queued wait within the limits must succeed");
        // 1 KiB transferred over one second of transfer time: including the
        // five-second queue wait would report roughly one-sixth KiB/s and
        // wrongly demote a busy-but-fast peer.
        assert!(
            (speed_kbps - 1.0).abs() < 0.05,
            "unexpected speed: {speed_kbps}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_wins_when_requested_during_queue_expiry_poll() {
        let cancel = Arc::new(AtomicBool::new(false));
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1_900)).await;
            trigger.store(true, Ordering::SeqCst);
        });
        let client = ScriptedClient::new(vec![vec![]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 60;
        config.max_queue_time_secs = 2;

        let (_dir, result) = download_with_script(&client, 1, 1_024, &config, Some(&cancel)).await;

        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("download cancelled by user"),
            "cancellation set while recv is pending must outrank queue expiry: {error:?}"
        );
        assert!(!matches!(error, SeakarrError::QueueTimeout(_)));
        assert_eq!(client.cancellations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_wins_when_requested_during_position_poll() {
        let cancel = Arc::new(AtomicBool::new(false));
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.store(true, Ordering::SeqCst);
        });
        let client = ScriptedClient::new(vec![vec![status_step(
            Duration::from_millis(150),
            queue_position(4),
        )]]);
        let mut config = default_dl_config();
        config.max_retries = 0;
        config.timeout_secs = 60;
        config.max_queue_length = 3;
        config.max_queue_time_secs = 10;

        let (_dir, result) = download_with_script(&client, 0, 1_024, &config, Some(&cancel)).await;

        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("download cancelled by user"),
            "cancellation set while recv is pending must outrank position rejection: {error:?}"
        );
        assert!(!matches!(error, SeakarrError::QueueTimeout(_)));
        assert_eq!(client.cancellations.load(Ordering::SeqCst), 1);
    }
}
