//! Pure selection for a label sweep: a label's release groups become download
//! targets.
//!
//! Nothing here performs I/O. The provider layer fetches and caches; the runner
//! sequences and downloads; this module decides what is wanted, in what order,
//! and how many of each rejection reason were seen.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::DiscographyReleaseType;
use crate::discography::label::LabelRelease;
use crate::discography::{
    normalize_catalog_key, parse_partial_date, release_allowed, select_albums, AlbumTarget,
    PartialDate, ReleaseGroup,
};
use crate::search::album_identity_key;

/// One album to download, with the artist credit it is searched under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelTarget {
    pub artist: String,
    pub target: AlbumTarget,
}

/// How many releases each filter removed.
///
/// The reasons are counted per release; they explain why releases were dropped and must
/// not be summed to `releases_examined`. The type and `--since` reasons are not mutually
/// exclusive: a release can be, say, both of an unwanted type and undated, and is counted
/// under each reason that applies. An excluded artist credit is decided first and is
/// exclusive: such a release is counted only as `excluded_credits`.
///
/// `eligible` is measured after the `--since` filter as well as after collapsing, so
/// it is smaller than the number of eligible releases whenever a release is dropped
/// by the bound or when one album is listed more than once.
///
/// An album whose identity collapses several release groups keeps the earliest dated
/// one as its representative. When a `--since` bound sits between them, the album is
/// still kept if any release group of the same identity is of a wanted type and at or
/// after the bound: the operator asked about the album, and one of its releases
/// qualifies. The only unconditional drop is an identity with no qualifying group,
/// which is already counted per release by `since_skipped`/`undated_skipped`. The
/// representative's own date is what orders the work list.
///
/// The rescue set is built from the release groups the catalogue lists, before the
/// bundle split. A bundle title ("A / B") that is at or after the bound therefore
/// qualifies the identity of an album literally titled the same way, even though
/// `select_albums` will not select the bundle itself. That errs towards keeping an
/// album, which is the safe direction: the alternative would drop an album the operator
/// asked for because of a naming coincidence in someone else's catalogue.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SelectionCounts {
    pub releases_examined: usize,
    pub not_wanted_type: usize,
    pub since_skipped: usize,
    pub undated_skipped: usize,
    pub excluded_credits: usize,
    /// Releases whose artist credit folds away to nothing, so there is no artist to
    /// search under. A data problem, counted apart from the operator's exclusions.
    pub undescribed_credits: usize,
    pub eligible: usize,
}

/// The work list plus the counts that explain it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LabelTargets {
    pub targets: Vec<LabelTarget>,
    pub counts: SelectionCounts,
}

/// Turn a label's release groups into the albums this run should download.
///
/// A release listed twice under two credit spellings ("Nu:Tone" and "Nu:Tone feat. Stamina
/// MC") produces one target, under the credit whose folded key sorts first, because the
/// second copy of the same release group is dropped. Both spellings are legitimate search
/// names, and downloading the album twice would be worse than choosing one; the choice is a
/// deterministic ordering rather than a preference for either credit.
///
/// The label-specific rejections are counted per release, so the summary can explain a
/// catalogue that produced no work. Two drops are not counted here because they are not
/// this module's decisions: a release whose title is empty or names a bundle, and a release
/// group already seen for the same identity, are dropped inside `select_albums` (which logs
/// each at debug level). A catalogue consisting only of those reports its examined count and
/// no drop reason. Ordering is total and deterministic - artist key, then
/// the existing partial-date order (dated before undated, earliest first), then
/// normalised title, then release-group id - which is what makes a
/// budget-limited sweep converge instead of oscillating.
///
/// Bundle detection is scoped to one folded artist credit: `select_albums` decides a title
/// bundles other albums from the titles of the group it is given, so a composite and its
/// companion are recognised together only when both appear under the same credit. Two
/// spellings of a credit are separate groups, and a composite whose companion this run never
/// saw at all is not recognised as a bundle - the conservative direction (it is downloaded,
/// not silently dropped).
pub fn label_targets(
    releases: &[LabelRelease],
    allowed: &[DiscographyReleaseType],
    since: Option<PartialDate>,
    exclude_artists: &[String],
) -> LabelTargets {
    let mut counts = SelectionCounts {
        releases_examined: releases.len(),
        ..SelectionCounts::default()
    };
    let excluded: BTreeSet<String> = exclude_artists
        .iter()
        .map(|name| normalize_catalog_key(name))
        .filter(|key| !key.is_empty())
        .collect();

    // Group by the credit a release was issued under, and remember each release
    // group's earliest date: `select_albums` returns targets without dates, and
    // the date is part of the ordering contract.
    //
    // Every non-excluded release group is pushed, whatever its type or date.
    // `select_albums` builds its bundle-visibility set from the groups it is given,
    // and the artist sweep builds that set *before* the release-type filter runs, so
    // pre-filtering here would let a bundle's companion disappear and the bundle be
    // selected as a plain album. The filters are counted per release below and
    // applied to the selected targets afterwards.
    let mut groups: BTreeMap<String, (String, Vec<ReleaseGroup>)> = BTreeMap::new();
    let mut dates: BTreeMap<String, Option<PartialDate>> = BTreeMap::new();
    // Identities with at least one release group that provably satisfies `--since`.
    // `select_albums` keeps the earliest-dated group per identity, so a later
    // qualifying group can otherwise be represented by an earlier one and lose the
    // album entirely.
    let mut identities_in_bound: BTreeSet<(String, String)> = BTreeSet::new();

    for release in releases {
        let credit_key = normalize_catalog_key(&release.artist_credit);
        if credit_key.is_empty() {
            // Nothing to search under: a data problem, not an operator exclusion, so it
            // must not be reported as one - but it must still be reported.
            counts.undescribed_credits += 1;
            tracing::debug!(
                release_id = %release.release_id,
                "skipping a release whose artist credit folds away"
            );
            continue;
        }
        if excluded.contains(&credit_key) {
            counts.excluded_credits += 1;
            continue;
        }
        let wanted = release_allowed(&release.release_group, allowed);
        if !wanted {
            counts.not_wanted_type += 1;
        }
        let date = release
            .release_group
            .first_release_date
            .as_deref()
            .and_then(parse_partial_date);
        if let Some(bound) = since {
            match date {
                // Only a release the run would actually select may rescue the album:
                // a qualifying group of an unwanted type must not pull in an album
                // whose own representative is below the bound.
                Some(date) if date >= bound => {
                    if wanted {
                        identities_in_bound.insert((
                            credit_key.clone(),
                            album_identity_key(
                                &release.release_group.title,
                                &release.artist_credit,
                            ),
                        ));
                    }
                }
                Some(_) => counts.since_skipped += 1,
                // A release group with no date - or with one MusicBrainz wrote in a
                // shape this crate cannot read - cannot prove it satisfies a lower
                // bound, so it is excluded rather than assumed recent.
                None => counts.undated_skipped += 1,
            }
        }
        dates
            .entry(release.release_group.id.clone())
            .and_modify(|slot| {
                if let Some(candidate) = date {
                    *slot = Some(match *slot {
                        Some(existing) => existing.min(candidate),
                        None => candidate,
                    });
                }
            })
            .or_insert(date);
        // Keep the lexicographically smallest spelling for a folded credit, so the
        // search string and the staging folder do not depend on payload order.
        let entry = groups
            .entry(credit_key)
            .or_insert_with(|| (release.artist_credit.clone(), Vec::new()));
        if release.artist_credit < entry.0 {
            entry.0 = release.artist_credit.clone();
        }
        entry.1.push(release.release_group.clone());
    }

    // `select_albums` is reused per credit group rather than reimplemented, so
    // bundle decomposition, artist-prefix stripping and the album-identity
    // collapse behave exactly as they do in the artist sweep.
    let mut merged: Vec<(String, Option<PartialDate>, LabelTarget)> = Vec::new();
    let mut seen_identities: BTreeSet<(String, String)> = BTreeSet::new();
    // One release group is one album, so the same group listed under a second
    // credit cannot become a second target and a second download.
    let mut seen_groups: BTreeSet<String> = BTreeSet::new();
    for (credit_key, (credit, groups)) in groups {
        for target in select_albums(&groups, allowed, &credit) {
            let date = dates.get(&target.release_group_id).copied().flatten();
            let identity = album_identity_key(&target.title, &credit);
            if let Some(bound) = since {
                // `select_albums` does not know the bound, so a target whose own date
                // cannot satisfy it is dropped here - unless another release group of
                // the same album does satisfy it, which is the date the operator asked
                // about. Undated targets are dropped: they cannot prove a lower bound.
                let own_date_satisfies = date.is_some_and(|date| date >= bound);
                if !own_date_satisfies
                    && !identities_in_bound.contains(&(credit_key.clone(), identity.clone()))
                {
                    continue;
                }
            }
            if !seen_identities.insert((credit_key.clone(), identity))
                || !seen_groups.insert(target.release_group_id.clone())
            {
                continue;
            }
            merged.push((
                credit_key.clone(),
                date,
                LabelTarget {
                    artist: credit.clone(),
                    target,
                },
            ));
        }
    }

    merged.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| match (left.1, right.1) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| {
                normalize_catalog_key(&left.2.target.title)
                    .cmp(&normalize_catalog_key(&right.2.target.title))
            })
            .then_with(|| {
                left.2
                    .target
                    .release_group_id
                    .cmp(&right.2.target.release_group_id)
            })
    });

    counts.eligible = merged.len();
    LabelTargets {
        targets: merged.into_iter().map(|(_, _, target)| target).collect(),
        counts,
    }
}

/// Counters accumulated across one label run, used to build the summary.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LabelCounters {
    /// The `--label` values as given, in order.
    pub requested: Vec<String>,
    /// Labels resolved this run, as `(name, mbid)` in encounter order.
    pub resolved: Vec<(String, String)>,
    /// Labels MusicBrainz could not resolve, in encounter order.
    pub unresolved: Vec<String>,
    /// Labels skipped because a previous run recorded a resolution failure.
    pub cached_failures: Vec<String>,
    /// Labels whose request failed, with the reason.
    pub provider_failed: Vec<(String, String)>,
    /// Labels served from a stale catalogue because the refresh failed. Kept apart from
    /// `provider_failed`: the label was *not* skipped - its albums were selected - so the
    /// "label skipped" wording would contradict the staged line in the same summary.
    pub stale_catalogues: Vec<(String, String)>,
    /// Labels actually examined before the run stopped.
    pub labels_examined: usize,
    /// What the selection filters removed and kept.
    pub selection: SelectionCounts,
    /// True once the selection has run. A run that stops before it (the breaker
    /// tripped) has no selection to report, so the eligible/staged notices are
    /// withheld rather than printed as zero.
    pub selection_ran: bool,
    /// True when the run stopped because the operator cancelled it - before a label,
    /// before an album, or during an album. A cancelled run would otherwise report
    /// like a completed one, and an interrupted download would be counted as a
    /// failure.
    pub cancelled: bool,
    /// Releases whose payload carried no release group or artist credit.
    pub incomplete: usize,
    /// Albums the library already holds.
    pub present: usize,
    /// Albums skipped because an earlier run recorded them as processed. Counted apart
    /// from `present`: the library does not hold them, which is exactly why a repeated
    /// sweep looks as if it had nothing to do unless this is reported.
    pub skipped_processed: usize,
    /// Albums downloaded and left in staging, counted from the outcome.
    pub staged_albums: usize,
    /// Albums that failed, with the reason recorded on the report.
    pub failed_albums: usize,
    /// The artist the run stopped at because the budget was spent.
    pub budget_reached_at: Option<String>,
    /// The configured budget limit, echoed in the notice.
    pub budget_limit: u32,
}

/// Values listed in one notice before it is truncated.
const MAX_LISTED_VALUES: usize = 10;

/// Join values for a notice, truncating long lists the way the artist summary does.
///
/// The wording and the scope are this summary's own: it truncates the resolved,
/// unresolved and cached-failure lists alike, where the artist summary truncates
/// only its unresolved list.
fn listed(values: &[String]) -> String {
    if values.len() <= MAX_LISTED_VALUES {
        return values.join(", ");
    }
    format!(
        "{}, and {} more",
        values[..MAX_LISTED_VALUES].join(", "),
        values.len() - MAX_LISTED_VALUES
    )
}

/// Build the aggregate notices for a finished label run, in spec order.
///
/// Pure: the caller records the returned strings on its `RunReport`. A run that
/// stopped before the selection - the breaker tripped - reports no selection-derived
/// notice, because there was no selection to report. A run that stops after it (a
/// cancelled scan, an unreadable library) still reports its selection.
pub fn label_notices(counters: &LabelCounters, staging_dir: &str) -> Vec<String> {
    let mut notices = Vec::new();

    if !counters.resolved.is_empty() {
        let values: Vec<String> = counters
            .resolved
            .iter()
            .map(|(name, mbid)| format!("{name} ({mbid})"))
            .collect();
        notices.push(format!(
            "label: resolved {} label(s): {}",
            counters.resolved.len(),
            listed(&values)
        ));
    }
    if !counters.requested.is_empty() {
        // Unconditional, like the artist summary's "nothing examined" line: a run
        // cancelled before its first label would otherwise print nothing at all,
        // which is indistinguishable from a crash-free no-op.
        notices.push(format!(
            "label: {} of {} label(s) processed",
            counters.labels_examined,
            counters.requested.len()
        ));
    }
    if counters.cancelled {
        // Phase-neutral wording: cancellation is honoured before a label, before an
        // album and during a download, and in the last two every label was examined.
        notices.push("label: cancelled by user; the run stopped before it finished".to_string());
    }
    if !counters.unresolved.is_empty() {
        notices.push(format!(
            "label: {} label(s) unresolved: {}",
            counters.unresolved.len(),
            listed(&counters.unresolved)
        ));
        // One refusal is answered by its own candidate list; several mean the
        // operator needs the deterministic escape hatch, which the refusal path
        // and the README both name.
        if counters.unresolved.len() > 1 {
            notices.push(format!(
                "label: pin {} unresolved name(s) with discography.label_mbids to resolve them deterministically",
                counters.unresolved.len()
            ));
        }
    }
    if !counters.cached_failures.is_empty() {
        notices.push(format!(
            "label: {} label(s) skipped from a recorded resolution failure: {}",
            counters.cached_failures.len(),
            listed(&counters.cached_failures)
        ));
        // A replayed failure suppresses the lookup for the whole expiry, which is
        // surprising for an explicit --label run, so name the two ways out.
        notices.push(
            "label: pin the name with discography.label_mbids, or wait for \
             discography.failure_cache_days to expire, to ask again"
                .to_string(),
        );
    }
    for (label, reason) in &counters.provider_failed {
        notices.push(format!(
            "label: provider failed for {label} ({reason}); label skipped"
        ));
    }
    for (label, reason) in &counters.stale_catalogues {
        notices.push(format!(
            "label: provider failed for {label} ({reason}); its stale catalogue was used"
        ));
    }
    if counters.selection.releases_examined > 0 {
        notices.push(format!(
            "label: {} release(s) examined; {} not of the wanted type",
            counters.selection.releases_examined, counters.selection.not_wanted_type
        ));
    }
    if counters.incomplete > 0 {
        notices.push(format!(
            "label: MusicBrainz data could not describe {} release(s)",
            counters.incomplete
        ));
    }
    if counters.selection.undescribed_credits > 0 {
        notices.push(format!(
            "label: {} release(s) skipped: the artist credit names nobody",
            counters.selection.undescribed_credits
        ));
    }
    if counters.selection.since_skipped > 0 || counters.selection.undated_skipped > 0 {
        // The two counts are disjoint - a dated release below the bound, and one with no
        // usable date - so the first number is their total and the parenthetical breaks it
        // down. Reporting only the dated count beside the undated count read as if the
        // undated ones were a subset of it.
        notices.push(format!(
            "label: {} release(s) excluded by --since ({} below the bound, {} undated or unparseable)",
            counters.selection.since_skipped + counters.selection.undated_skipped,
            counters.selection.since_skipped,
            counters.selection.undated_skipped
        ));
    }
    if counters.selection.excluded_credits > 0 {
        notices.push(format!(
            "label: {} release(s) skipped: excluded artist credit",
            counters.selection.excluded_credits
        ));
    }
    if counters.present > 0 {
        notices.push(format!(
            "label: {} album(s) already present; skipped",
            counters.present
        ));
    }
    if counters.skipped_processed > 0 {
        notices.push(format!(
            "label: {} album(s) already processed; skipped",
            counters.skipped_processed
        ));
    }
    if counters.selection_ran {
        // No derived "still to fetch" number: what is left depends on the budget, on
        // which albums were already processed, and on how many were staged - each of
        // which is reported on its own line. A subtraction here would either overstate
        // the work (a budget-stopped run never examined the rest) or contradict the
        // staged/failed line right below it.
        notices.push(format!(
            "label: {} album(s) selected",
            counters.selection.eligible
        ));
        notices.push(format!(
            "label: {} album(s) staged; {} failed",
            counters.staged_albums, counters.failed_albums
        ));
    }
    if let Some(artist) = &counters.budget_reached_at {
        notices.push(format!(
            "label: budget of {} download(s) reached at {artist}; the rest is left for a later run",
            counters.budget_limit
        ));
    }
    if counters.staged_albums > 0 {
        notices.push(format!(
            "label: staged albums were not written to the library; move them from {staging_dir}"
        ));
    }
    notices
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(
        id: &str,
        title: &str,
        date: Option<&str>,
        primary: &str,
        secondary: &[&str],
    ) -> ReleaseGroup {
        ReleaseGroup {
            id: id.to_string(),
            title: title.to_string(),
            first_release_date: date.map(str::to_string),
            primary_type: Some(primary.to_string()),
            secondary_types: secondary.iter().map(|value| value.to_string()).collect(),
        }
    }

    fn release(id: &str, credit: &str, group: ReleaseGroup) -> LabelRelease {
        LabelRelease {
            release_id: format!("release-{id}"),
            release_group: group,
            artist_credit: credit.to_string(),
        }
    }

    fn studio() -> Vec<DiscographyReleaseType> {
        vec![DiscographyReleaseType::StudioAlbum]
    }

    fn nothing_excluded() -> Vec<String> {
        Vec::new()
    }

    fn bound(value: &str) -> PartialDate {
        parse_partial_date(value).expect("test bound must parse")
    }

    #[test]
    fn a_studio_album_becomes_one_target_under_its_credit() {
        let releases = vec![release(
            "1",
            "London Elektricity",
            group("g1", "Syncopated City", Some("2008-06-02"), "Album", &[]),
        )];
        let selected = label_targets(&releases, &studio(), None, &nothing_excluded());
        assert_eq!(selected.targets.len(), 1);
        assert_eq!(selected.targets[0].artist, "London Elektricity");
        assert_eq!(selected.targets[0].target.title, "Syncopated City");
        assert_eq!(selected.counts.eligible, 1);
    }

    #[test]
    fn a_release_whose_credit_names_nobody_is_counted() {
        // The release is dropped because there is no artist to search under. That is a
        // data problem, so it is not an operator exclusion - but it must still appear in
        // the counts, or the summary reports releases examined and none dropped.
        let releases = vec![
            // A credit made only of apostrophe-like marks folds away to nothing.
            release(
                "1",
                "\u{2019}",
                group("g1", "Album", Some("2001"), "Album", &[]),
            ),
            release("2", "A", group("g2", "Kept", Some("2002"), "Album", &[])),
        ];
        let selected = label_targets(&releases, &studio(), None, &nothing_excluded());
        assert_eq!(selected.counts.undescribed_credits, 1);
        assert_eq!(selected.counts.excluded_credits, 0);
        assert_eq!(selected.counts.eligible, 1);
    }

    #[test]
    fn releases_of_another_type_are_counted_not_selected() {
        let releases = vec![
            release("1", "A", group("g1", "Album", Some("2001"), "Album", &[])),
            release("2", "A", group("g2", "Single", Some("2002"), "Single", &[])),
            release(
                "3",
                "A",
                group("g3", "Live Thing", Some("2003"), "Album", &["Live"]),
            ),
        ];
        let selected = label_targets(&releases, &studio(), None, &nothing_excluded());
        assert_eq!(selected.targets.len(), 1);
        assert_eq!(selected.counts.not_wanted_type, 2);
    }

    #[test]
    fn an_ep_is_selected_only_when_it_is_wanted() {
        let releases = vec![release(
            "1",
            "A",
            group("g1", "An EP", Some("2001"), "EP", &[]),
        )];
        assert!(
            label_targets(&releases, &studio(), None, &nothing_excluded())
                .targets
                .is_empty()
        );
        let ep_only = vec![DiscographyReleaseType::Ep];
        assert_eq!(
            label_targets(&releases, &ep_only, None, &nothing_excluded())
                .targets
                .len(),
            1
        );
    }

    #[test]
    fn the_since_bound_is_inclusive_and_counts_what_it_drops() {
        let releases = vec![
            release(
                "1",
                "A",
                group("g1", "Before", Some("1999-12-31"), "Album", &[]),
            ),
            release(
                "2",
                "A",
                group("g2", "On The Bound", Some("2000-01-01"), "Album", &[]),
            ),
            release("3", "A", group("g3", "After", Some("2001"), "Album", &[])),
        ];
        let selected = label_targets(
            &releases,
            &studio(),
            Some(bound("2000-01-01")),
            &nothing_excluded(),
        );
        let titles: Vec<&str> = selected
            .targets
            .iter()
            .map(|target| target.target.title.as_str())
            .collect();
        assert_eq!(titles, vec!["On The Bound", "After"]);
        assert_eq!(selected.counts.since_skipped, 1);
    }

    #[test]
    fn an_undated_release_cannot_satisfy_a_lower_bound() {
        let releases = vec![
            release("1", "A", group("g1", "Undated", None, "Album", &[])),
            release("2", "A", group("g2", "Dated", Some("2025"), "Album", &[])),
        ];
        let selected = label_targets(
            &releases,
            &studio(),
            Some(bound("2024")),
            &nothing_excluded(),
        );
        assert_eq!(selected.targets.len(), 1);
        assert_eq!(selected.targets[0].target.title, "Dated");
        assert_eq!(selected.counts.undated_skipped, 1);
        assert_eq!(selected.counts.since_skipped, 0);
    }

    #[test]
    fn a_bound_is_satisfied_by_any_release_group_of_the_album() {
        // The same album listed twice: an early release group and a later one. The
        // collapse keeps the earliest as the identity's representative, so without the
        // identity check the album would vanish from a run bounded after the early
        // group.
        let releases = vec![
            release("1", "A", group("g1", "Album X", Some("1999"), "Album", &[])),
            release("2", "A", group("g2", "Album X", Some("2023"), "Album", &[])),
        ];

        let selected = label_targets(
            &releases,
            &studio(),
            Some(bound("2020")),
            &nothing_excluded(),
        );
        assert_eq!(
            selected.targets.len(),
            1,
            "an album with a qualifying release group must stay in the work list"
        );
        assert_eq!(selected.targets[0].target.title, "Album X");
    }

    #[test]
    fn an_unwanted_type_cannot_rescue_an_album_through_the_bound() {
        // The only release group at or after the bound is an EP, which this run does not
        // want; the album's own studio group is below the bound, so the album must drop.
        let releases = vec![
            release("1", "A", group("g1", "Album X", Some("1999"), "Album", &[])),
            release("2", "A", group("g2", "Album X", Some("2023"), "EP", &[])),
        ];

        let selected = label_targets(
            &releases,
            &studio(),
            Some(bound("2020")),
            &nothing_excluded(),
        );
        assert!(
            selected.targets.is_empty(),
            "an unwanted type must not pull an out-of-bound album in: {:?}",
            selected.targets
        );
    }

    #[test]
    fn without_a_bound_an_undated_release_is_selected_last() {
        let releases = vec![
            release("1", "A", group("g1", "Undated", None, "Album", &[])),
            release("2", "A", group("g2", "Dated", Some("2025"), "Album", &[])),
        ];
        let selected = label_targets(&releases, &studio(), None, &nothing_excluded());
        let titles: Vec<&str> = selected
            .targets
            .iter()
            .map(|t| t.target.title.as_str())
            .collect();
        assert_eq!(titles, vec!["Dated", "Undated"]);
        assert_eq!(selected.counts.undated_skipped, 0);
    }

    #[test]
    fn excluded_credits_are_dropped_and_counted() {
        let releases = vec![
            release(
                "1",
                "Various Artists",
                group("g1", "Compilation", Some("2001"), "Album", &[]),
            ),
            release(
                "2",
                "VA",
                group("g2", "Another", Some("2002"), "Album", &[]),
            ),
            release(
                "3",
                "Real Artist",
                group("g3", "Album", Some("2003"), "Album", &[]),
            ),
        ];
        let excluded = vec!["Various Artists".to_string(), "VA".to_string()];
        let selected = label_targets(&releases, &studio(), None, &excluded);
        assert_eq!(selected.targets.len(), 1);
        assert_eq!(selected.counts.excluded_credits, 2);
    }

    #[test]
    fn a_collaboration_keeps_its_full_credit_string() {
        let releases = vec![release(
            "1",
            "Nu:Tone feat. Stamina MC",
            group("g1", "Words 2 Believe In", Some("2007"), "Album", &[]),
        )];
        let selected = label_targets(&releases, &studio(), None, &nothing_excluded());
        assert_eq!(selected.targets[0].artist, "Nu:Tone feat. Stamina MC");
    }

    #[test]
    fn the_same_album_on_two_labels_collapses_to_one_target() {
        let releases = vec![
            release("1", "A", group("g1", "Album", Some("2001"), "Album", &[])),
            release("2", "A", group("g2", "Album", Some("2001"), "Album", &[])),
        ];
        let selected = label_targets(&releases, &studio(), None, &nothing_excluded());
        assert_eq!(selected.targets.len(), 1);
    }

    #[test]
    fn ordering_is_by_artist_then_date_then_title() {
        let releases = vec![
            release("1", "Zed", group("g1", "Later", Some("2010"), "Album", &[])),
            release(
                "2",
                "Abc",
                group("g2", "Second", Some("2005"), "Album", &[]),
            ),
            release("3", "Abc", group("g3", "First", Some("2001"), "Album", &[])),
            release(
                "4",
                "Abc",
                group("g4", "Also First", Some("2001"), "Album", &[]),
            ),
        ];
        let selected = label_targets(&releases, &studio(), None, &nothing_excluded());
        let ordered: Vec<(&str, &str)> = selected
            .targets
            .iter()
            .map(|target| (target.artist.as_str(), target.target.title.as_str()))
            .collect();
        assert_eq!(
            ordered,
            vec![
                ("Abc", "Also First"),
                ("Abc", "First"),
                ("Abc", "Second"),
                ("Zed", "Later"),
            ]
        );
    }

    #[test]
    fn a_credit_spelling_does_not_depend_on_payload_order() {
        let upper = release(
            "1",
            "Nu:Tone",
            group("g1", "Album X", Some("2001"), "Album", &[]),
        );
        let lower = release(
            "2",
            "nu:tone",
            group("g2", "Album Y", Some("2002"), "Album", &[]),
        );

        let forward = label_targets(
            &[upper.clone(), lower.clone()],
            &studio(),
            None,
            &nothing_excluded(),
        );
        let reversed = label_targets(&[lower, upper], &studio(), None, &nothing_excluded());

        assert_eq!(
            forward, reversed,
            "the same catalogue in another order must select the same work list"
        );
    }

    #[test]
    fn the_same_input_in_another_order_selects_the_same_work_list() {
        let releases = vec![
            release("1", "A", group("g1", "One", Some("2001"), "Album", &[])),
            release("2", "B", group("g2", "Two", Some("2002"), "Album", &[])),
            release("3", "A", group("g3", "Three", Some("2003"), "Album", &[])),
        ];
        let mut shuffled = releases.clone();
        shuffled.reverse();
        let first = label_targets(&releases, &studio(), None, &nothing_excluded());
        let second = label_targets(&shuffled, &studio(), None, &nothing_excluded());
        assert_eq!(first, second);
    }

    #[test]
    fn a_bundle_part_excluded_by_type_still_suppresses_the_bundle() {
        // The artist sweep builds its bundle-visibility set before the release-type
        // filter runs, so a companion of another type still marks the composite as a
        // bundle. Filtering before grouping would hide the companion and let the
        // composite be selected as a plain album.
        let releases = vec![
            release(
                "1",
                "A",
                group("g1", "First Album", Some("2001"), "Album", &[]),
            ),
            release(
                "2",
                "A",
                group("g2", "Second Album", Some("2002"), "Single", &[]),
            ),
            release(
                "3",
                "A",
                group(
                    "g3",
                    "First Album / Second Album",
                    Some("2003"),
                    "Album",
                    &[],
                ),
            ),
        ];

        let selected = label_targets(&releases, &studio(), None, &nothing_excluded());
        let titles: Vec<&str> = selected
            .targets
            .iter()
            .map(|target| target.target.title.as_str())
            .collect();
        assert_eq!(
            titles,
            vec!["First Album"],
            "the composite must not be selected"
        );
        assert_eq!(selected.counts.not_wanted_type, 1);
    }

    #[test]
    fn one_release_group_credited_two_ways_is_one_target() {
        let releases = vec![
            release(
                "1",
                "Nu:Tone",
                group("g1", "Words 2 Believe In", Some("2007"), "Album", &[]),
            ),
            release(
                "2",
                "Nu:Tone feat. Stamina MC",
                group("g1", "Words 2 Believe In", Some("2007"), "Album", &[]),
            ),
        ];

        let selected = label_targets(&releases, &studio(), None, &nothing_excluded());
        assert_eq!(
            selected.targets.len(),
            1,
            "one release group is one album, whatever credit it carries"
        );
    }

    #[test]
    fn an_empty_catalogue_selects_nothing_and_says_so() {
        let selected = label_targets(&[], &studio(), None, &nothing_excluded());
        assert!(selected.targets.is_empty());
        assert_eq!(selected.counts.releases_examined, 0);
        assert_eq!(selected.counts.eligible, 0);
    }

    fn counters() -> LabelCounters {
        LabelCounters {
            requested: vec!["Hospital Records".into()],
            labels_examined: 1,
            selection_ran: true,
            budget_limit: 5,
            ..LabelCounters::default()
        }
    }

    #[test]
    fn a_quiet_run_says_what_it_resolved_and_where_the_albums_went() {
        let mut run = counters();
        run.resolved.push((
            "Hospital Records".into(),
            "8562598d-60d1-4bd6-8b85-4d709e951770".into(),
        ));
        run.selection.releases_examined = 12;
        run.selection.eligible = 4;
        run.present = 1;
        run.staged_albums = 3;

        let notices = label_notices(&run, "downloads/staging");
        assert!(notices[0].contains("resolved 1 label"), "{notices:?}");
        assert!(notices[0].contains("Hospital Records"), "{notices:?}");
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("12 release(s) examined")),
            "{notices:?}"
        );
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("1 album(s) already present")),
            "{notices:?}"
        );
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("3 album(s) staged")),
            "{notices:?}"
        );
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("downloads/staging")),
            "the closing line must name the staging root: {notices:?}"
        );
    }

    #[test]
    fn unresolved_and_failed_labels_are_reported_separately() {
        let mut run = counters();
        run.unresolved.push("Hospital Records".into());
        run.cached_failures.push("Med School".into());
        run.provider_failed
            .push(("Spearhead".into(), "MusicBrainz returned HTTP 503".into()));

        let notices = label_notices(&run, "downloads/staging");
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("1 label(s) unresolved")),
            "{notices:?}"
        );
        assert!(
            notices.iter().any(|notice| notice.contains("Med School")),
            "{notices:?}"
        );
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("provider failed for Spearhead")),
            "{notices:?}"
        );
    }

    #[test]
    fn the_since_and_type_counts_are_reported_when_they_explain_nothing_eligible() {
        let mut run = counters();
        run.selection.releases_examined = 30;
        run.selection.not_wanted_type = 20;
        run.selection.since_skipped = 5;
        run.selection.undated_skipped = 3;
        run.selection.excluded_credits = 2;

        let notices = label_notices(&run, "downloads/staging");
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("20 not of the wanted type")),
            "{notices:?}"
        );
        assert!(
            notices.iter().any(|notice| notice
                == "label: 8 release(s) excluded by --since (5 below the bound, 3 undated or unparseable)"),
            "the total, not the dated subset, is the exclusion count: {notices:?}"
        );
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("2 release(s) skipped: excluded artist credit")),
            "{notices:?}"
        );
    }

    #[test]
    fn a_full_credit_cannot_fold_a_lead_prefixed_title() {
        // `select_albums` folds "Artist - Title" onto "Title" only when the prefix is the whole
        // artist or its initials. Under a collaboration credit the lead artist's name is
        // neither, so the two spellings keep separate identities and both become targets: one
        // album downloaded twice. Pinned deliberately - the alternative is guessing which part
        // of a credit is the lead artist.
        let credit = "Grafix feat. Stamina MC";
        let releases = vec![
            release(
                "1",
                credit,
                group("g1", "Half Light", Some("2001"), "Album", &[]),
            ),
            release(
                "2",
                credit,
                group("g2", "Grafix - Half Light", Some("2001"), "Album", &[]),
            ),
        ];

        let selected = label_targets(&releases, &studio(), None, &nothing_excluded());

        assert_eq!(
            selected.targets.len(),
            2,
            "the full-credit artist cannot fold the prefixed title, so both survive"
        );
        assert!(selected
            .targets
            .iter()
            .all(|target| target.artist == credit));
    }

    #[test]
    fn a_stale_catalogue_is_not_reported_as_a_skipped_label() {
        // The albums from a stale catalogue are selected and staged, so saying the label was
        // skipped would contradict the staged line printed beside it.
        let mut run = counters();
        run.stale_catalogues
            .push(("Hospital Records".into(), "connection reset".into()));

        let notices = label_notices(&run, "downloads/staging");
        let line = notices
            .iter()
            .find(|notice| {
                notice.contains("Hospital Records") && notice.contains("provider failed")
            })
            .unwrap_or_else(|| panic!("no provider line: {notices:?}"));
        assert!(line.contains("its stale catalogue was used"), "got: {line}");
        assert!(
            !line.contains("label skipped"),
            "a label whose albums were used was not skipped: {line}"
        );
    }

    #[test]
    fn a_repeated_sweep_reports_the_albums_it_skipped_as_processed() {
        // An album this run skipped because an earlier one recorded it is neither in the
        // library nor staged nor failed, so without its own line the summary reads as if
        // the sweep found no work at all.
        let mut run = counters();
        run.selection.eligible = 3;
        run.skipped_processed = 2;

        let notices = label_notices(&run, "downloads/staging");
        assert!(
            notices
                .iter()
                .any(|notice| notice == "label: 2 album(s) already processed; skipped"),
            "a repeat run must say what it skipped: {notices:?}"
        );
    }

    #[test]
    fn more_than_one_unresolved_label_advises_pinning() {
        let mut run = counters();
        run.unresolved
            .extend(["Hospital Records".to_string(), "Med School".to_string()]);

        let notices = label_notices(&run, "downloads/staging");
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("pin 2 unresolved name(s)")),
            "got {notices:?}"
        );

        let mut single = counters();
        single.unresolved.push("Med School".to_string());
        assert!(
            !label_notices(&single, "downloads/staging")
                .iter()
                .any(|notice| notice.contains("pin ")),
            "one refusal is answered by its own candidate list"
        );
    }

    #[test]
    fn an_aborted_run_does_not_report_selection_results() {
        let mut run = counters();
        run.selection_ran = false;
        run.unresolved.push("Hospital Records".into());

        let notices = label_notices(&run, "downloads/staging");
        assert!(
            !notices
                .iter()
                .any(|notice| notice.contains("album(s) selected")),
            "a run that never selected must not claim a selection: {notices:?}"
        );
        assert!(
            !notices
                .iter()
                .any(|notice| notice.contains("album(s) staged")),
            "a run that never selected must not claim staging: {notices:?}"
        );
        assert!(
            !notices
                .iter()
                .any(|notice| notice.contains("move them from")),
            "nothing was staged, so there is nothing to move: {notices:?}"
        );
    }

    #[test]
    fn a_selected_run_reports_the_selection_without_deriving_what_is_left() {
        // The selection line counts what was selected and nothing else. What remains to
        // fetch depends on the budget and on albums already present, processed, staged or
        // failed, and each of those has its own line; a derived number here contradicted
        // them (a run could report "3 to fetch" beside "3 staged").
        let mut run = counters();
        run.selection_ran = true;
        run.selection.eligible = 7;
        run.present = 2;
        run.skipped_processed = 3;
        run.staged_albums = 2;

        let notices = label_notices(&run, "downloads/staging");
        assert!(
            notices
                .iter()
                .any(|notice| notice == "label: 7 album(s) selected"),
            "the selection must be reported: {notices:?}"
        );
        assert!(
            !notices.iter().any(|notice| notice.contains("to fetch")),
            "no line may derive a remaining-work count: {notices:?}"
        );
        assert!(
            notices
                .iter()
                .any(|notice| notice == "label: 2 album(s) staged; 0 failed"),
            "the staged count stands on its own: {notices:?}"
        );
    }

    #[test]
    fn a_replayed_failure_names_both_ways_out() {
        let mut run = counters();
        run.cached_failures.push("Med School".to_string());

        let notices = label_notices(&run, "downloads/staging");
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("discography.label_mbids")
                    && notice.contains("failure_cache_days")),
            "a suppressed lookup must name the escape hatches: {notices:?}"
        );
    }

    #[test]
    fn a_cancelled_run_says_it_was_cancelled() {
        let mut run = counters();
        run.cancelled = true;

        let notices = label_notices(&run, "downloads/staging");
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("cancelled by user")),
            "a cancelled run must be distinguishable from a completed one: {notices:?}"
        );
    }

    #[test]
    fn a_run_that_examined_nothing_still_says_so() {
        let run = LabelCounters {
            requested: vec!["Hospital Records".into()],
            ..LabelCounters::default()
        };

        let notices = label_notices(&run, "downloads/staging");
        assert_eq!(
            notices,
            vec!["label: 0 of 1 label(s) processed".to_string()],
            "a cancelled run must not be silent"
        );
    }

    #[test]
    fn long_label_lists_are_truncated_in_notices() {
        let mut run = counters();
        run.unresolved = (0..12).map(|index| format!("Label {index}")).collect();

        let notices = label_notices(&run, "downloads/staging");
        let unresolved = notices
            .iter()
            .find(|notice| notice.contains("unresolved"))
            .expect("the unresolved notice is emitted");
        assert!(unresolved.contains("Label 9"), "got {unresolved}");
        assert!(!unresolved.contains("Label 10"), "got {unresolved}");
        assert!(unresolved.contains("and 2 more"), "got {unresolved}");
    }

    #[test]
    fn the_budget_stop_names_the_artist_it_stopped_on() {
        let mut run = counters();
        run.budget_reached_at = Some("London Elektricity".into());
        let notices = label_notices(&run, "downloads/staging");
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("5 download(s)")
                    && notice.contains("London Elektricity")),
            "{notices:?}"
        );
    }

    #[test]
    fn incomplete_releases_are_named() {
        let mut run = counters();
        run.incomplete = 2;
        let notices = label_notices(&run, "downloads/staging");
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("could not describe 2 release(s)")),
            "{notices:?}"
        );
    }
}
