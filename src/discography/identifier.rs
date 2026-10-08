//! Identifying a MusicBrainz artist from a `--artist` value.
//!
//! An explicit `--artist` may be a name, an ISNI, an IPI or a MusicBrainz UUID.
//! The value's shape decides, and the shape rule is deliberately narrow: only a
//! value that is *entirely* one of the code shapes is treated as a code, so an
//! ordinary name is never reinterpreted unless it is spelled exactly like a code.
//! A band named `12345678901` is therefore read as an IPI; `discography.artist_mbids`
//! keyed by that input's cache key - `ipi:12345678901` - is the way to pin it to an
//! artist instead. A near miss is reported rather than searched for: a digit run one
//! either side of a code length, or a hyphenated value within two of a UUID's 32 hex digits.
//! Anything else stays a name.

use super::{
    normalize_catalog_key, resolve_artist_by_code, ArtistCandidate, DiscographyError,
    ResolvedArtist,
};

/// What an explicit `--artist` value means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtistInput {
    /// An artist name, kept as typed.
    Name(String),
    /// A 16-character ISNI, normalised: digits with an optional trailing `X`.
    Isni(String),
    /// An 11-digit IPI.
    Ipi(String),
    /// A hyphenated MusicBrainz artist UUID, lower-cased.
    Mbid(String),
    /// A value shaped like a code but not a usable one - a digit run of the wrong
    /// length, or a hyphenated value that is not a UUID. [`ArtistInput::validate`]
    /// rejects it, so a typo is not silently searched for as a name.
    Malformed { value: String, reason: String },
}

/// A code that has to be searched for in the MusicBrainz artist index.
///
/// The MBID is deliberately absent: an ID is fetched with the entity lookup
/// instead, so a search for one cannot be expressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeIdentifier<'a> {
    Isni(&'a str),
    Ipi(&'a str),
}

impl CodeIdentifier<'_> {
    /// The query term MusicBrainz expects for this code.
    pub(crate) fn query(&self) -> String {
        match self {
            Self::Isni(value) => format!("isni:\"{value}\""),
            Self::Ipi(value) => format!("ipi:\"{value}\""),
        }
    }

    /// The human name of the code, for messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Isni(_) => "ISNI",
            Self::Ipi(_) => "IPI",
        }
    }

    /// The normalised code value.
    pub fn value(&self) -> &str {
        match self {
            Self::Isni(value) | Self::Ipi(value) => value,
        }
    }

    /// True when `candidate` claims this code in its own identifier list.
    ///
    /// Comparison is on the normalised forms, so spacing and an ISNI's letter
    /// case cannot make MusicBrainz's own record look like a different code.
    pub fn matches(&self, candidate: &ArtistCandidate) -> bool {
        let claimed = match self {
            Self::Isni(_) => &candidate.isnis,
            Self::Ipi(_) => &candidate.ipis,
        };
        claimed.iter().any(|code| {
            let normalised = normalise_code(code);
            normalised.eq_ignore_ascii_case(self.value())
        })
    }
}

impl ArtistInput {
    /// Classify an explicit `--artist` value by its shape.
    ///
    /// Total: a value that is not a code shape stays a name, so classification
    /// never silently drops an artist. A code-shaped value that cannot be used as
    /// one - an ISNI with a bad check character, a digit run of the wrong length,
    /// a hyphenated value that is not a UUID - is reported by
    /// [`ArtistInput::validate`].
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        let stripped: String = trimmed
            .chars()
            .filter(|character| !character.is_whitespace() && *character != '-')
            .collect();
        if valid_mbid(trimmed) {
            return Self::Mbid(trimmed.to_ascii_lowercase());
        }
        if is_isni_shape(&stripped) {
            return Self::Isni(stripped.to_ascii_uppercase());
        }
        if is_ipi_shape(&stripped) {
            return Self::Ipi(stripped);
        }
        // Only a *near* miss is reported rather than searched for as a name: a digit
        // run one either side of a code length, or a hyphenated value within two of the
        // 32 hex digits a UUID carries. Anything else is a name.
        if stripped.chars().all(|c| c.is_ascii_digit())
            && ((10..=12).contains(&stripped.len()) || (15..=17).contains(&stripped.len()))
        {
            return Self::Malformed {
                value: raw.to_string(),
                reason: format!(
                    "artist {raw:?} is {} digits; an IPI is {IPI_LENGTH} digits and an ISNI {ISNI_LENGTH}",
                    stripped.len()
                ),
            };
        }
        if trimmed.matches('-').count() == 4
            && (30..=34).contains(&stripped.len())
            && trimmed
                .split('-')
                .all(|group| !group.is_empty() && group.chars().all(|c| c.is_ascii_hexdigit()))
        {
            return Self::Malformed {
                value: raw.to_string(),
                reason: format!(
                    "artist {raw:?} is hyphenated like a MusicBrainz ID but is not one \
                     (8-4-4-4-12 hexadecimal digits)"
                ),
            };
        }
        // The prefixed form is the `discography.artist_mbids` key syntax the README
        // teaches; it is not a `--artist` value. Rejecting it here stops it being
        // searched for as a literal name.
        if let Some((prefix, _)) = trimmed.split_once(':') {
            if matches!(
                prefix.trim().to_ascii_lowercase().as_str(),
                "isni" | "ipi" | "mbid"
            ) {
                return Self::Malformed {
                    value: raw.to_string(),
                    reason: format!(
                        "artist {raw:?} is a discography.artist_mbids key, not an artist; pass the code itself"
                    ),
                };
            }
        }
        Self::Name(raw.to_string())
    }

    /// Wrap a library-derived artist name. The sweep never classifies: a name it
    /// read from a tag or folder is a name even when it is spelled like a code.
    pub fn name(value: impl Into<String>) -> Self {
        Self::Name(value.into())
    }

    /// The code to search for, when this input is one.
    pub fn code(&self) -> Option<CodeIdentifier<'_>> {
        match self {
            Self::Isni(value) => Some(CodeIdentifier::Isni(value)),
            Self::Ipi(value) => Some(CodeIdentifier::Ipi(value)),
            Self::Name(_) | Self::Mbid(_) | Self::Malformed { .. } => None,
        }
    }

    /// The MusicBrainz ID to look up, when this input is one.
    pub fn mbid(&self) -> Option<&str> {
        match self {
            Self::Mbid(value) => Some(value),
            _ => None,
        }
    }

    /// Why this value cannot be used, when it is shaped like a code but is not a
    /// usable one. Always `None` for a name, a code or an ID.
    pub fn malformed_reason(&self) -> Option<&str> {
        match self {
            Self::Malformed { reason, .. } => Some(reason),
            _ => None,
        }
    }

    /// The value to show and to match album titles against: a name as it was
    /// typed, a code or an ID in the normalised form [`ArtistInput::parse`] gave
    /// it.
    pub fn display(&self) -> &str {
        match self {
            Self::Name(value) | Self::Isni(value) | Self::Ipi(value) | Self::Mbid(value) => value,
            Self::Malformed { value, .. } => value,
        }
    }

    /// Reject a value that claims to be a code but cannot be used as one.
    ///
    /// A name is only checked for blankness here; `select_artists` is stricter,
    /// rejecting a name that still carries nothing once the catalogue key has
    /// folded it.
    pub fn validate(&self) -> std::result::Result<(), String> {
        match self {
            Self::Name(value) if value.trim().is_empty() => {
                Err(format!("artist {value:?} is blank"))
            }
            Self::Name(_) => Ok(()),
            Self::Ipi(_) => Ok(()),
            Self::Mbid(_) => Ok(()),
            Self::Malformed { reason, .. } => Err(reason.clone()),
            Self::Isni(value) => {
                if isni_check_character_is_valid(value) {
                    Ok(())
                } else {
                    Err(format!("ISNI {value:?} has an invalid check character"))
                }
            }
        }
    }

    /// The key for the discography and failure caches.
    ///
    /// Prefixed for a code or an ID so the three spaces cannot collide, and so a
    /// configured `discography.artist_mbids` key can be written as `isni:<digits>`
    /// to pin a code that several artists share.
    pub fn cache_key(&self) -> String {
        match self {
            Self::Name(value) => normalize_catalog_key(value),
            Self::Isni(value) => format!("isni:{value}"),
            Self::Ipi(value) => format!("ipi:{value}"),
            Self::Mbid(value) => format!("mbid:{value}"),
            // Never reached: every caller validates first, and a malformed value is
            // rejected there.
            Self::Malformed { value, .. } => normalize_catalog_key(value),
        }
    }

    /// True when this input is not a plain name: a code, an ID, or a malformed shape
    /// (which every caller rejects first, and which is deliberately not treated as a
    /// name so a broken code is never searched for).
    pub fn is_identity(&self) -> bool {
        !matches!(self, Self::Name(_))
    }

    /// A short label for messages: `ISNI`, `IPI`, `MusicBrainz ID`, or `artist`
    /// for a name.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Name(_) | Self::Malformed { .. } => "artist",
            Self::Isni(_) => "ISNI",
            Self::Ipi(_) => "IPI",
            Self::Mbid(_) => "MusicBrainz ID",
        }
    }
}

/// The cache key a configured `discography.artist_mbids` key stands for.
///
/// A prefixed key is normalised exactly as the input itself is, so a pin written
/// `isni:0000 0004 6659 7022` or `isni:0000-0004-6659-7022` matches the
/// `isni:0000000466597022` key that run produces. A key without a known prefix is
/// a name key, folded as before.
pub fn pin_key(key: &str) -> String {
    let trimmed = key.trim();
    let Some((prefix, rest)) = trimmed.split_once(':') else {
        return normalize_catalog_key(trimmed);
    };
    let code = normalise_code(rest);
    match prefix.trim().to_ascii_lowercase().as_str() {
        // The ISNI and IPI forms ignore the separators they are printed with; the ID
        // form keeps its hyphens, exactly as `ArtistInput::cache_key` does.
        "isni" if is_isni_shape(&code) => format!("isni:{}", code.to_ascii_uppercase()),
        "ipi" if is_ipi_shape(&code) => format!("ipi:{code}"),
        "mbid" if valid_mbid(rest.trim()) => format!("mbid:{}", rest.trim().to_ascii_lowercase()),
        // Any other colon-bearing key - including a band named `ISNI: Some Band` - is
        // a name key, folded as before.
        _ => normalize_catalog_key(trimmed),
    }
}

/// ISNI length, including its check character.
const ISNI_LENGTH: usize = 16;
/// IPI length. IPIs carry no check digit.
const IPI_LENGTH: usize = 11;

/// True when `value` is exactly an 11-digit IPI.
fn is_ipi_shape(value: &str) -> bool {
    value.len() == IPI_LENGTH && value.chars().all(|c| c.is_ascii_digit())
}

/// Strip the separators MusicBrainz and printed ISNIs use.
fn normalise_code(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace() && *character != '-')
        .collect()
}

/// True when `value` is 16 characters of digits with an optional trailing `X`.
fn is_isni_shape(value: &str) -> bool {
    // An ISNI is ASCII by definition, and byte length equals character length only
    // for ASCII: `split_at` panics on an index inside a multi-byte character, so a
    // 16-byte value such as fourteen `a`s plus an accent must be rejected here
    // rather than aborting the process.
    if !value.is_ascii() || value.len() != ISNI_LENGTH {
        return false;
    }
    let (body, last) = value.split_at(ISNI_LENGTH - 1);
    body.chars().all(|c| c.is_ascii_digit())
        && (last.eq_ignore_ascii_case("x") || last.chars().all(|c| c.is_ascii_digit()))
}

/// ISO 7064 MOD 11-2, the ISNI check-character algorithm.
///
/// The first 15 characters must be digits, and the remainder runs from 0 as
/// `(remainder + digit) * 2 % 11`. The check character is valid when it equals
/// `(12 - remainder) % 11`, with 10 written as `X`. The wrap matters: a body
/// leaving remainder 0 takes the digit 1, not 12.
fn isni_check_character_is_valid(value: &str) -> bool {
    let mut characters = value.chars();
    let mut remainder: u32 = 0;
    for character in characters.by_ref().take(ISNI_LENGTH - 1) {
        let Some(digit) = character.to_digit(10) else {
            return false;
        };
        remainder = (remainder + digit) * 2 % 11;
    }
    let Some(check) = characters.next() else {
        return false;
    };
    // ISO 7064 MOD 11-2: the check character is `(12 - remainder) mod 11`, so
    // remainder 0 takes the digit 1, remainder 1 takes 0, and 10 is written `X`.
    let expected = (12 - remainder) % 11;
    match check {
        'X' | 'x' => expected == 10,
        other => other.to_digit(10) == Some(expected),
    }
}

/// True when `value` is a hyphenated MusicBrainz artist UUID.
///
/// The same shape `discography.artist_mbids` has always accepted.
pub(crate) fn valid_mbid(value: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let parts: Vec<&str> = value.split('-').collect();
    parts.len() == GROUPS.len()
        && parts.iter().zip(GROUPS).all(|(part, length)| {
            part.len() == length && part.chars().all(|c| c.is_ascii_hexdigit())
        })
}

/// Resolve a code against the artists MusicBrainz returned for it.
///
/// Only a candidate whose own identifier list contains the code counts, so a
/// fuzzy search hit that does not carry it is discarded rather than trusted on
/// its relevance score.
pub fn resolve_code(
    code: CodeIdentifier<'_>,
    candidates: &[ArtistCandidate],
) -> std::result::Result<ResolvedArtist, DiscographyError> {
    resolve_artist_by_code(code, candidates)
}

#[cfg(test)]
mod tests {
    use super::super::ArtistResolution;
    use super::*;

    // The plan's `fn code(value: &str) -> Option<CodeIdentifier<'_>>` helper cannot
    // compile: `ArtistInput::parse` owns the String the code borrows, so the
    // borrow cannot outlive the call. Each test binds the parsed input locally
    // instead; names, assertions and messages are unchanged.

    #[test]
    fn an_eleven_digit_value_is_an_ipi() {
        let input = ArtistInput::parse("00581977202");
        assert_eq!(input.code(), Some(CodeIdentifier::Ipi("00581977202")));
    }

    #[test]
    fn a_sixteen_digit_value_is_an_isni_with_spaces_and_hyphens_removed() {
        let spaced = ArtistInput::parse("0000 0004 6659 7022");
        assert_eq!(
            spaced.code(),
            Some(CodeIdentifier::Isni("0000000466597022"))
        );
        let hyphenated = ArtistInput::parse("0000-0004-6659-7022");
        assert_eq!(
            hyphenated.code(),
            Some(CodeIdentifier::Isni("0000000466597022"))
        );
    }

    #[test]
    fn a_lower_case_isni_check_character_is_upper_cased() {
        let input = ArtistInput::parse("000000012345678x");
        assert_eq!(input.code(), Some(CodeIdentifier::Isni("000000012345678X")));
    }

    #[test]
    fn a_hyphenated_uuid_is_a_musicbrainz_id() {
        let input = ArtistInput::parse("c02a5966-b5c4-483a-8326-483edcf3680e");
        assert_eq!(
            input,
            ArtistInput::Mbid("c02a5966-b5c4-483a-8326-483edcf3680e".to_string())
        );
        assert!(input.code().is_none(), "an MBID is looked up, not searched");
    }

    #[test]
    fn a_name_stays_a_name() {
        assert_eq!(
            ArtistInput::parse("Grafix"),
            ArtistInput::Name("Grafix".to_string())
        );
        assert_eq!(
            ArtistInput::parse("  Grafix  "),
            ArtistInput::Name("  Grafix  ".to_string()),
            "names keep their spelling; trimming stays with select_artists"
        );
    }

    #[test]
    fn a_band_named_like_a_code_shape_is_read_as_that_code() {
        // Documented accepted consequence: the shape wins over the name.
        let input = ArtistInput::parse("12345678901");
        assert_eq!(input.code(), Some(CodeIdentifier::Ipi("12345678901")));
    }

    #[test]
    fn a_near_miss_digit_run_is_rejected_rather_than_searched_as_a_name() {
        // One digit either side of an IPI (11) or an ISNI (16) is a typo,
        // not a name: searching Soulseek for the digit run is the confusing
        // outcome the design exists to prevent.
        for value in [
            "1234567890",
            "123456789012",
            "123456789012345",
            "12345678901234567",
        ] {
            let input = ArtistInput::parse(value);
            assert!(input.code().is_none(), "{value} must not become a code");
            assert!(
                input.validate().is_err(),
                "{value} is shaped like a mistyped IPI or ISNI and must be rejected, got {input:?}"
            );
        }
        // Far from either code length, a digit run is just a name.
        assert_eq!(
            ArtistInput::parse("12345"),
            ArtistInput::Name("12345".to_string())
        );
        assert_eq!(
            ArtistInput::parse("123456789012345678901234567890"),
            ArtistInput::Name("123456789012345678901234567890".to_string())
        );
    }

    #[test]
    fn a_hyphenated_value_that_is_not_a_uuid_is_rejected() {
        // Four hyphens and hexadecimal groups, but the last group is too short.
        let input = ArtistInput::parse("c02a5966-b5c4-483a-8326-483edcf3680");
        assert!(
            input.malformed_reason().is_some(),
            "a wrong-length UUID must be reported, got {input:?}"
        );
        assert!(input.validate().is_err());
    }

    #[test]
    fn an_isni_whose_check_character_is_one_validates() {
        // ISO 7064 MOD 11-2 wraps: a body leaving remainder 0 takes the check
        // digit 1, which a bare `12 - remainder` computes as 12 and rejects.
        assert_eq!(ArtistInput::parse("0000 0008 6659 7021").validate(), Ok(()));
        // A published ORCID, whose check character confirmed the formula, and one
        // ending in the letter `X`.
        assert_eq!(ArtistInput::parse("0000 0002 1825 0097").validate(), Ok(()));
        assert_eq!(ArtistInput::parse("0000 0002 1694 233X").validate(), Ok(()));
    }

    #[test]
    fn a_sixteen_byte_value_with_a_multi_byte_character_is_a_name_not_a_panic() {
        // Sixteen bytes but fewer characters, so the fifteenth byte can fall
        // inside a character: the shape test must not split there.
        for value in [
            format!("{}é", "a".repeat(14)),
            format!("{}あ", "a".repeat(13)),
        ] {
            assert_eq!(value.len(), 16, "the fixture must be sixteen bytes");
            assert!(ArtistInput::parse(&value).code().is_none());
            assert_eq!(ArtistInput::parse(&value), ArtistInput::Name(value.clone()));
        }
    }

    #[test]
    fn a_sixteen_character_value_with_a_letter_inside_is_a_name() {
        let input = ArtistInput::parse("0000000a66597022");
        assert_eq!(input.code(), None);
    }

    #[test]
    fn a_bad_isni_check_digit_is_rejected_by_validation() {
        let input = ArtistInput::parse("0000000466597023");
        assert_eq!(
            input.validate(),
            Err("ISNI \"0000000466597023\" has an invalid check character".to_string())
        );
    }

    #[test]
    fn the_motivating_isni_and_ipi_validate() {
        assert_eq!(ArtistInput::parse("0000 0004 6659 7022").validate(), Ok(()));
        assert_eq!(ArtistInput::parse("00581977202").validate(), Ok(()));
    }

    #[test]
    fn a_uuid_with_a_letter_outside_hexadecimal_is_a_name_not_a_rejected_code() {
        // Only a shape that claims to be a code can fail validation.
        assert_eq!(
            ArtistInput::parse("c02a5966-b5c4-483a-8326-483edcf3680g"),
            ArtistInput::Name("c02a5966-b5c4-483a-8326-483edcf3680g".to_string())
        );
    }

    #[test]
    fn a_blank_name_fails_validation() {
        assert_eq!(
            ArtistInput::parse("   ").validate(),
            Err("artist \"   \" is blank".to_string())
        );
    }

    #[test]
    fn cache_keys_separate_names_codes_and_ids() {
        assert_eq!(ArtistInput::parse("Grafix").cache_key(), "grafix");
        assert_eq!(
            ArtistInput::parse("0000 0004 6659 7022").cache_key(),
            "isni:0000000466597022"
        );
        assert_eq!(
            ArtistInput::parse("00581977202").cache_key(),
            "ipi:00581977202"
        );
        assert_eq!(
            ArtistInput::parse("C02A5966-B5C4-483A-8326-483EDCF3680E").cache_key(),
            "mbid:c02a5966-b5c4-483a-8326-483edcf3680e"
        );
    }

    #[test]
    fn the_motivating_ipi_resolves_the_artist_that_claims_it() {
        let candidates = vec![ArtistCandidate {
            id: "c02a5966-b5c4-483a-8326-483edcf3680e".to_string(),
            name: "Grafix".to_string(),
            score: Some(100),
            ipis: vec!["00581977202".to_string()],
            isnis: vec!["0000000466597022".to_string()],
            disambiguation: Some("DnB producer Josh Jackson".to_string()),
        }];

        let by_ipi = resolve_code(CodeIdentifier::Ipi("00581977202"), &candidates).unwrap();
        let by_isni = resolve_code(CodeIdentifier::Isni("0000000466597022"), &candidates).unwrap();

        assert_eq!(by_ipi.candidate.id, by_isni.candidate.id);
        assert_eq!(by_ipi.resolution, ArtistResolution::Identity);
    }

    #[test]
    fn the_prefixed_config_key_form_is_not_an_artist_value() {
        // `isni:<digits>` is the `discography.artist_mbids` key syntax the README
        // teaches. Passing it to `--artist` must be reported, not searched for.
        for value in [
            "isni:0000000466597022",
            "ipi:00581977202",
            "ISNI:00581977202",
        ] {
            let input = ArtistInput::parse(value);
            assert!(
                input.malformed_reason().is_some(),
                "{value} is a config key, not an artist value"
            );
            assert!(input.validate().is_err());
        }
    }

    #[test]
    fn a_hex_looking_name_is_still_a_name() {
        // Only a near miss is a broken code: a digit run one either side of 11 or 16,
        // or five hyphenated hex groups within two of a UUID's 32 hex digits.
        for value in [
            "Face-Babe-Dead-Beef-Cafe",
            "1-2-3-4-5",
            "1234567890123",
            "1234567890123456789",
        ] {
            assert_eq!(
                ArtistInput::parse(value),
                ArtistInput::Name(value.to_string()),
                "{value} must stay a name"
            );
        }
    }

    #[test]
    fn a_prefixed_pin_key_is_normalised_like_the_input() {
        assert_eq!(pin_key("isni:0000 0004 6659 7022"), "isni:0000000466597022");
        assert_eq!(pin_key("isni:0000-0004-6659-7022"), "isni:0000000466597022");
        assert_eq!(pin_key("ipi:00581977202"), "ipi:00581977202");
        assert_eq!(
            pin_key("isni:0000 0002 1694 233x"),
            "isni:000000021694233X",
            "the check character must fold to the case the cache key uses"
        );
        assert_eq!(
            pin_key("mbid:C02A5966-B5C4-483A-8326-483EDCF3680E"),
            "mbid:c02a5966-b5c4-483a-8326-483edcf3680e"
        );
        // A key without a known prefix, or one whose remainder is not code-shaped, is
        // a name key, folded as it always was.
        assert_eq!(pin_key("Grafix"), "grafix");
        assert_eq!(
            pin_key("ISNI: Some Band"),
            normalize_catalog_key("ISNI: Some Band"),
            "a band named like a prefix is a name, not a pin"
        );
    }
}
