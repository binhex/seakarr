//! Recognition of MusicBrainz release groups whose title bundles several
//! albums.
//!
//! MusicBrainz publishes real release groups such as Archive's
//! `Controlling Crowds / You All Look the Same to Me` (Album, 2014), where
//! each part is also a release group of its own. A bundled title adds no
//! music: it only fabricates an album target that duplicates albums the
//! discography already lists, and it matched no peer share in the runs that
//! motivated this module, so it was searched again on every discover run for
//! nothing.
//!
//! The rule is deliberately conservative. A title is a bundle only when every
//! part is already a release-group title for the same artist, so a genuine
//! title that happens to contain a spaced slash - Metallica's
//! `Live at Wembley Stadium, London, England / April 20th, 1992`, Oasis's A/B
//! single `Little by Little / She Is Love` - is left alone. Every ambiguous
//! shape resolves to "keep", which is the behaviour that existed before this
//! module.

use std::collections::BTreeSet;

use unicode_normalization::UnicodeNormalization;

use super::normalize_catalog_key;

/// Split a release-group title into the release-group titles it bundles.
///
/// Returns the trimmed parts only when the title has at least two parts, none
/// of them empty, and every `/` has whitespace on both sides. A slash without
/// surrounding whitespace is never a separator, so `Either/Or` and
/// `John Lennon/Plastic Ono Band` stay single titles.
///
/// The title is NFKC-folded before splitting so a fullwidth slash is
/// recognised, matching the fold [`normalize_catalog_key`] applies.
///
/// The whitespace test is the same `char::is_whitespace` class that
/// [`normalize_catalog_key`] collapses, so a slash separated by any Unicode
/// space - a non-breaking space included - is a separator exactly as it is in
/// the compared key. Accepting only ASCII space here would let the separator
/// test and the key comparison disagree about the same title.
pub(crate) fn split_parts(title: &str) -> Option<Vec<String>> {
    let folded: String = title.nfkc().collect();
    let characters: Vec<char> = folded.chars().collect();
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut separators = 0usize;
    for (index, character) in characters.iter().enumerate() {
        if *character != '/' {
            current.push(*character);
            continue;
        }
        let spaced_before = index > 0 && characters[index - 1].is_whitespace();
        let spaced_after = characters
            .get(index + 1)
            .is_some_and(|next| next.is_whitespace());
        if !spaced_before || !spaced_after {
            return None;
        }
        separators += 1;
        parts.push(current.trim().to_owned());
        current = String::new();
    }
    if separators == 0 {
        return None;
    }
    parts.push(current.trim().to_owned());
    // `separators >= 1` already guarantees two parts - one pushed per separator
    // plus this final push - so only emptiness needs rejecting. That covers
    // ` / `, `A / ` and ` / B`.
    if parts.iter().any(String::is_empty) {
        return None;
    }
    Some(parts)
}

/// True when `title` names several release groups that the same artist already
/// has as separate release groups.
///
/// The check is type-agnostic: it runs for every release group that
/// `release_allowed` accepted, so a composite EP or single whose parts are also
/// release groups of the same artist is decomposed too.
///
/// `artist_titles` holds [`normalize_catalog_key`] of every release-group title
/// returned for the artist, including groups the configured release-type filter
/// later rejects, so a part counts as known even when its own release group is
/// not selectable.
pub(crate) fn is_bundle(title: &str, artist_titles: &BTreeSet<String>) -> bool {
    let Some(parts) = split_parts(title) else {
        return false;
    };
    // `split_parts` already folded the parts, and `normalize_catalog_key` folds
    // them again. The second fold is idempotent, and going through the canonical
    // key function is what keeps a part compared with exactly the same
    // normalisation as the title set it is looked up in.
    let own_key = normalize_catalog_key(title);
    parts.iter().all(|part| {
        let part_key = normalize_catalog_key(part);
        // Both leading clauses are unreachable today and are kept as defence in
        // depth for the spec's "never the bundled title itself" and non-empty
        // key contracts: `split_parts` returns only parts that are non-empty
        // after trimming, and no part contains a separator, so no part key can
        // be empty or equal `own_key`, which always contains one.
        !part_key.is_empty() && part_key != own_key && artist_titles.contains(&part_key)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::discography::normalize_catalog_key;

    use super::*;

    /// Long titles are assembled from parts so no source line is unwrapped in
    /// a way that changes the string.
    const JAMIROQUAI_BUNDLE: &str = concat!(
        "Emergency on Planet Earth",
        " / The Return of the Space Cowboy",
        " / Travelling Without Moving"
    );
    const DYLAN_BUNDLE: &str = concat!(
        "The Freewheelin\u{2019} Bob Dylan",
        " / The Times They Are A\u{2010}Changin\u{2019}",
        " / Another Side Of Bob Dylan"
    );
    const METALLICA_VENUE: &str = concat!(
        "Live at Wembley Stadium, London, England",
        " / April 20th, 1992"
    );

    #[test]
    fn decomposition_splits_spaced_slashes_and_trims_each_part() {
        assert_eq!(
            split_parts("Controlling Crowds / You All Look the Same to Me"),
            Some(vec![
                "Controlling Crowds".to_owned(),
                "You All Look the Same to Me".to_owned(),
            ])
        );
        assert_eq!(
            split_parts("  Controlling Crowds   /   Noise  "),
            Some(vec!["Controlling Crowds".to_owned(), "Noise".to_owned()])
        );
        assert_eq!(
            split_parts(JAMIROQUAI_BUNDLE),
            Some(vec![
                "Emergency on Planet Earth".to_owned(),
                "The Return of the Space Cowboy".to_owned(),
                "Travelling Without Moving".to_owned(),
            ])
        );
        assert_eq!(
            split_parts(DYLAN_BUNDLE),
            Some(vec![
                "The Freewheelin\u{2019} Bob Dylan".to_owned(),
                "The Times They Are A\u{2010}Changin\u{2019}".to_owned(),
                "Another Side Of Bob Dylan".to_owned(),
            ])
        );
    }

    #[test]
    fn decomposition_folds_a_fullwidth_slash_before_splitting() {
        // U+FF0F FULLWIDTH SOLIDUS folds to '/' under NFKC.
        assert_eq!(
            split_parts("Controlling Crowds \u{ff0f} Noise"),
            Some(vec!["Controlling Crowds".to_owned(), "Noise".to_owned()])
        );
    }

    #[test]
    fn decomposition_keeps_titles_whose_slashes_are_not_separators() {
        assert_eq!(split_parts("Either/Or"), None);
        assert_eq!(split_parts("John Lennon/Plastic Ono Band"), None);
        assert_eq!(split_parts("Alternate Versions From either/or"), None);
        assert_eq!(
            split_parts("Controlling Crowds /You All Look the Same to Me"),
            None
        );
        assert_eq!(
            split_parts("Controlling Crowds/ You All Look the Same to Me"),
            None
        );
        assert_eq!(split_parts("Controlling Crowds"), None);
    }

    #[test]
    fn decomposition_uses_the_whitespace_class_of_the_key_normaliser() {
        // A slash surrounded by U+00A0 NO-BREAK SPACE is a separator, because
        // `normalize_catalog_key` collapses that character too, so the separator
        // test and the key comparison agree about the same title. Both parts
        // then have to be release groups of the artist for anything to be
        // dropped.
        assert_eq!(
            split_parts("Controlling Crowds\u{a0}/\u{a0}Noise"),
            Some(vec!["Controlling Crowds".to_owned(), "Noise".to_owned()])
        );
        assert!(is_bundle(
            "Controlling Crowds\u{a0}/\u{a0}Noise",
            &archive_titles()
        ));
    }

    #[test]
    fn decomposition_rejects_empty_shapes() {
        assert_eq!(split_parts(""), None);
        assert_eq!(split_parts("   "), None);
        assert_eq!(split_parts(" / "), None);
        assert_eq!(split_parts("Controlling Crowds / "), None);
        assert_eq!(split_parts(" / Noise"), None);
        assert_eq!(split_parts("Controlling Crowds / / Noise"), None);
    }

    /// Release-group titles each artist also has, normalised the way the
    /// production code compares them.
    ///
    /// The per-artist sets below are excerpts of the live MusicBrainz sample of
    /// 2026-09-17, trimmed to the titles the cases in this module need - not the
    /// artist's full discography. A case that depends on a title being absent
    /// records that in its own comment.
    fn titles(values: &[&str]) -> BTreeSet<String> {
        values
            .iter()
            .map(|value| normalize_catalog_key(value))
            .collect()
    }

    fn archive_titles() -> BTreeSet<String> {
        titles(&[
            "Controlling Crowds",
            "You All Look the Same to Me",
            "Noise",
            "Londinium",
        ])
    }

    fn jamiroquai_titles() -> BTreeSet<String> {
        titles(&[
            "Emergency on Planet Earth",
            "The Return of the Space Cowboy",
            "Travelling Without Moving",
        ])
    }

    fn dylan_titles() -> BTreeSet<String> {
        titles(&[
            "The Freewheelin\u{2019} Bob Dylan",
            "The Times They Are A\u{2010}Changin\u{2019}",
            "Another Side Of Bob Dylan",
            "Oh Mercy",
            "Time Out of Mind",
            "Blonde on Blonde",
            // Present on purpose: it is the album the `2cd:` assertion below
            // names, so that assertion fails if prefix stripping is ever added.
            "Highway 61 Revisited",
        ])
    }

    fn metallica_titles() -> BTreeSet<String> {
        titles(&["Metallica", "Master of Puppets", "Load"])
    }

    fn oasis_titles() -> BTreeSet<String> {
        titles(&["Definitely Maybe", "(What's the Story) Morning Glory?"])
    }

    fn elliott_smith_titles() -> BTreeSet<String> {
        titles(&["Roman Candle", "Elliott Smith", "Either/Or"])
    }

    fn john_lennon_titles() -> BTreeSet<String> {
        titles(&["John Lennon/Plastic Ono Band", "Double Fantasy", "Imagine"])
    }

    #[test]
    fn bundled_titles_are_recognised_from_their_known_albums() {
        assert!(is_bundle(
            "Controlling Crowds / You All Look the Same to Me",
            &archive_titles()
        ));
        assert!(is_bundle(
            "You All Look the Same to Me / Noise",
            &archive_titles()
        ));
        assert!(is_bundle(JAMIROQUAI_BUNDLE, &jamiroquai_titles()));
        assert!(is_bundle(DYLAN_BUNDLE, &dylan_titles()));
    }

    #[test]
    fn titles_whose_parts_are_not_exact_release_group_titles_are_kept() {
        assert!(!is_bundle("Wiped Out / Violently", &archive_titles()));
        // Both part titles are absent from the excerpt on purpose: the live
        // sample has no release group spelled `Little by Little` or
        // `She Is Love` for Oasis, so absence is what keeps this A/B single
        // whole. Adding either title to the fixture would invert the behaviour
        // this assertion protects.
        assert!(!is_bundle(
            "Little by Little / She Is Love",
            &oasis_titles()
        ));
        assert!(!is_bundle(METALLICA_VENUE, &metallica_titles()));
        // `2cd:` is not part of any release-group title, so the first part
        // matches nothing and the title is kept. The artist does have
        // `Highway 61 Revisited` and that title is in `dylan_titles()`, so this
        // assertion fails if a disc prefix is ever stripped before matching -
        // which is the limit the spec records as out of scope.
        assert!(!is_bundle(
            "2cd: Highway 61 Revisited / Blonde on Blonde",
            &dylan_titles()
        ));
        // This asserts the absent-title case: `dylan_titles` deliberately omits
        // the canonical spelling of the second part, so the excerpt is what
        // keeps this title whole, not a spelling difference.
        assert!(!is_bundle(
            "Time Out of Mind / Love and Theft",
            &dylan_titles()
        ));
        assert!(!is_bundle(
            "Roman Candle / Elliott Smith / Either/Or",
            &elliott_smith_titles()
        ));
        assert!(!is_bundle(
            "Elton John / John Lennon",
            &john_lennon_titles()
        ));
        assert!(!is_bundle("Either/Or", &elliott_smith_titles()));
    }

    /// A title must be matched against *other* release groups, the spec clause
    /// "never the bundled title itself". The assertion below pins that
    /// observable contract only: it passes because the fixture set holds no
    /// *part* title, so it does not exercise the `part_key != own_key` clause,
    /// which is unreachable while a part can never contain a separator.
    #[test]
    fn a_title_must_be_matched_against_other_release_groups() {
        let only_itself = titles(&["Controlling Crowds / You All Look the Same to Me"]);
        assert!(!is_bundle(
            "Controlling Crowds / You All Look the Same to Me",
            &only_itself
        ));
    }
}
