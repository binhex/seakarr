//! The staging album layout.
//!
//! A staged album is two real directories — `<staging_dir>/<artist>/<album>` —
//! rather than one flattened `<artist>--<album>` name. Both components are
//! produced by [`crate::organizer::sanitize_component`], the same sanitiser
//! every library write uses, so for a given album the staging components are the
//! ones its library path is built from. Two differences are deliberate: a
//! library write that places into an existing artist folder uses that folder's
//! on-disk name verbatim, and an artist-only run names the album level
//! [`UNKNOWN_ALBUM`] (`unknown`) while the library side spells its own
//! placeholder `Unknown`. Neither difference can reach a destination: a
//! destination component is taken from a staged file's immediate parent only
//! when that parent is a disc designator and is not the album root itself (see
//! [`crate::organizer::disc_subdir`]), and for an artist-only run the album root
//! is the `unknown` level, which is not a disc designator.

use std::path::{Path, PathBuf};

use crate::error::Result;

/// The album component used when a run names no album. Matches the value the
/// flattened layout used, so an artist-only run's staging directory keeps the
/// name an operator already recognises. The library writers spell their own
/// placeholder `Unknown`; the two never have to agree, because a library
/// destination is built from the album value in hand and never by decoding a
/// staging path.
pub const UNKNOWN_ALBUM: &str = "unknown";

/// `<staging_dir>/<sanitized artist>`.
pub fn artist_dir(staging_dir: &Path, artist: &str) -> PathBuf {
    staging_dir.join(crate::organizer::sanitize_component(artist))
}

/// `<staging_dir>/<sanitized artist>/<sanitized album>`.
///
/// `album: None` keeps the [`UNKNOWN_ALBUM`] component an artist-only run used
/// under the flattened layout.
pub fn album_dir(staging_dir: &Path, artist: &str, album: Option<&str>) -> PathBuf {
    artist_dir(staging_dir, artist).join(crate::organizer::sanitize_component(
        album.unwrap_or(UNKNOWN_ALBUM),
    ))
}

/// What a [`normalise_layout`] pass changed, and what it could not.
///
/// Every field is a record for a warning, not an error: a wasted rename must
/// never abort a download run.
#[derive(Debug, Default)]
pub struct MigrationReport {
    /// Legacy directories renamed wholesale: the nested destination was absent.
    pub migrated: Vec<PathBuf>,
    /// Legacy directories merged into a destination that already existed.
    pub merged: Vec<PathBuf>,
    /// A legacy directory left in place, with the first entry that stopped it.
    pub left_in_place: Vec<(PathBuf, PathBuf)>,
    /// A top-level directory that is neither a legacy slug nor an artist directory.
    pub unrecognised: Vec<PathBuf>,
    /// Names that are not valid UTF-8; renaming them lossily could orphan an album.
    pub non_utf8: Vec<PathBuf>,
    /// A directory whose rewrite failed, with the error.
    pub failed: Vec<(PathBuf, String)>,
    /// Empty artist directories removed.
    pub pruned_artist_dirs: Vec<PathBuf>,
    /// Legacy-named directories the migration refused to attribute, because their
    /// contents cannot be told apart from an artist level.
    pub ambiguous: Vec<PathBuf>,
    /// Entries left untouched because they lie inside a configured library root.
    pub protected: Vec<PathBuf>,
}

impl MigrationReport {
    /// Log every anomaly at the level an operator sees at startup.
    fn log_anomalies(&self) {
        for (path, error) in &self.failed {
            tracing::warn!("Staging migration could not rewrite {path:?}: {error}");
        }
        for path in &self.non_utf8 {
            tracing::warn!(
                "Staging migration left {path:?} untouched: its name is not valid UTF-8, and renaming it lossily could orphan an album"
            );
        }
        for (path, first) in &self.left_in_place {
            tracing::warn!(
                "Staging migration left {path:?} in place: {first:?} already exists at the destination and was not overwritten"
            );
        }
        for path in &self.unrecognised {
            tracing::warn!(
                "Staging holds {path:?}, which is neither a legacy artist--album directory nor an album directory; leaving it untouched"
            );
        }
        for path in &self.ambiguous {
            tracing::warn!(
                "Staging migration left {path:?} in place: its name carries the legacy separator, but it holds no file at its top level, only directories, so it cannot be told apart from an artist level — renaming it could move a staged album a level deeper"
            );
        }
        for path in &self.protected {
            tracing::warn!(
                "Staging migration left {path:?} untouched: it lies inside a configured library path, so it is library content rather than staging debris (keep storage.staging_dir outside every library.paths entry)"
            );
        }
        // The successful rewrites are logged too: this migration is one-way, so
        // the operator needs a record of what actually moved.
        for path in &self.migrated {
            tracing::info!("Staging migration renamed {path:?} into the nested layout");
        }
        for path in &self.merged {
            tracing::info!("Staging migration merged {path:?} into the nested layout");
        }
        for path in &self.pruned_artist_dirs {
            tracing::debug!("Staging migration removed the empty artist directory {path:?}");
        }
    }
}

/// Create the staging root if it is missing, then bring its layout up to date.
///
/// The one call a run makes before it stages anything. An anomaly is logged,
/// never returned as an error. Two structural failures do abort the run through
/// the caller's `?`: the root cannot be created, or it exists but cannot be
/// read. The second is stricter than the bare `create_dir_all` this replaced,
/// which succeeded on an unreadable root and let the run continue.
pub fn prepare(staging_dir: &Path, library_paths: &[String]) -> Result<MigrationReport> {
    std::fs::create_dir_all(staging_dir)?;
    let protected: Vec<PathBuf> = library_paths.iter().map(PathBuf::from).collect();
    let report = normalise_layout_protecting(staging_dir, &protected)?;
    report.log_anomalies();
    Ok(report)
}

/// Rename legacy `<artist>--<album>` directories into `<artist>/<album>` and
/// remove artist directories that hold nothing. A missing staging root is not
/// an error: there is nothing stale in a directory that does not exist.
/// Test-only entry point: production always calls [`prepare`], which passes the
/// configured library paths so the migration can protect them.
#[cfg(test)]
pub(crate) fn normalise_layout(staging_dir: &Path) -> Result<MigrationReport> {
    normalise_layout_protecting(staging_dir, &[])
}

/// [`normalise_layout`], with directories that must never be touched named
/// explicitly. A configured library root is not staging debris whatever it looks
/// like, so it is never pruned and never renamed — the README forbids putting
/// the staging root inside one, and the migration must not be what makes that
/// misconfiguration destructive.
pub fn normalise_layout_protecting(
    staging_dir: &Path,
    protected_roots: &[PathBuf],
) -> Result<MigrationReport> {
    let mut report = MigrationReport::default();
    let entries = match std::fs::read_dir(staging_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(e.into()),
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                report
                    .failed
                    .push((staging_dir.to_path_buf(), e.to_string()));
                continue;
            }
        };
        if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            if let Err(e) = entry.file_type() {
                report.failed.push((entry.path(), e.to_string()));
            }
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            report.non_utf8.push(path);
            continue;
        };

        // An unreadable directory is a failure, not an ambiguous shape: the two
        // need different operator action.
        if std::fs::read_dir(&path).is_err() {
            report.failed.push((path, "unreadable".to_string()));
            continue;
        }

        // Library content is never staging debris. A directory that *is* a
        // configured root is the staging root's own child, which the README
        // allows, so it is skipped silently; a directory merely inside one warns,
        // because that is the actionable case.
        if protected_roots
            .iter()
            .any(|root| is_the_same_path(root, &path))
        {
            continue;
        }
        if protected_roots.iter().any(|root| is_inside(root, &path)) {
            report.protected.push(path);
            continue;
        }

        // An empty top-level directory has nothing to lose whatever its name, so
        // it is removed rather than renamed into an empty nested pair that no
        // later pass would prune.
        if dir_is_empty(&path) {
            match std::fs::remove_dir(&path) {
                Ok(()) => report.pruned_artist_dirs.push(path),
                Err(e) => report.failed.push((path, e.to_string())),
            }
            continue;
        }

        // A directory without the legacy separator is an artist level, and so is
        // one that already holds an album directory: an artist whose own name
        // contains `--` was written by this layout, not the one being migrated.
        // Renaming it would move the album a level deeper and leave recovery to
        // delete it as untracked debris.
        if !name.contains("--") || looks_like_an_artist_level(&path) {
            if name.contains("--") {
                // Left alone on purpose: it is either an artist level written by
                // this layout whose name carries the separator, or a legacy album
                // directory holding a subdirectory the stager never creates.
                // Renaming it could move a staged album a level deeper, so the
                // operator is told instead.
                tracing::warn!(
                    "Staging migration left {path:?} in place: its name carries the legacy separator, but its contents identify it as an artist directory, so it is neither renamed nor recovered"
                );
            }
            prune_or_report_artist_dir(&path, &mut report);
            continue;
        }

        // The evidence must be audio: a stray sidecar is not a legacy album, and
        // a tree whose children are all directories is exactly what a legacy
        // multi-disc album directory looks like *and* what an artist level looks
        // like when every album under it is named like a disc folder.
        if !looks_like_a_legacy_album(&path) {
            report.ambiguous.push(path);
            continue;
        }

        let (artist, album) = crate::organizer::parse_album_slug(&name);
        // A legacy name whose album half still carries the separator cannot be
        // attributed: `parse_album_slug` splits at the first occurrence, so the
        // rename would invent an album directory the next pass re-parses as
        // another legacy directory.
        if album.contains("--") {
            report.ambiguous.push(path);
            continue;
        }
        let destination =
            artist_dir(staging_dir, &artist).join(crate::organizer::sanitize_component(&album));
        // `exists()` follows a symlink and the merge writes through one, so a
        // linked destination is left alone rather than written into.
        if std::fs::symlink_metadata(&destination)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
        {
            report.ambiguous.push(path);
            continue;
        }
        // Renaming *into* a protected root would write library content, so the
        // destination is checked too. `is_inside` resolves through the deepest
        // existing ancestor, so it works for a destination that does not exist yet
        // and for a symlinked or `.`-containing spelling of the root.
        if protected_roots
            .iter()
            .any(|root| is_inside(root, &destination))
        {
            report.protected.push(path);
            continue;
        }
        if !destination.exists() {
            let parent = destination.parent().unwrap_or(staging_dir);
            match std::fs::create_dir_all(parent)
                .and_then(|()| std::fs::rename(&path, &destination))
            {
                Ok(()) => report.migrated.push(path),
                Err(e) => report.failed.push((path, e.to_string())),
            }
            continue;
        }

        let mut first_skipped = None;
        let mut residue = None;
        match merge_without_overwrite(&path, &destination, &mut first_skipped, &mut residue) {
            Ok(()) => {
                // A residue only exists when the directory was emptied: one that
                // still holds a skipped entry is already named by `left_in_place`.
                if let Some(residue) = residue {
                    if first_skipped.is_none() {
                        report.failed.push((
                            residue,
                            "merged, but the emptied directory could not be removed".to_string(),
                        ));
                    }
                } else if first_skipped.is_none() {
                    match std::fs::remove_dir(&path) {
                        Ok(()) => report.merged.push(path.clone()),
                        Err(e) => report.failed.push((path.clone(), e.to_string())),
                    }
                }
                if let Some(first) = first_skipped {
                    report.left_in_place.push((path, first));
                }
            }
            Err(e) => report.failed.push((path, e.to_string())),
        }
    }

    Ok(report)
}

/// True when `path` already has the shape of an artist level written by the
/// current layout: it holds at least one child directory that is not a disc
/// folder, which a legacy album directory can never contain because the stager
/// nests at most one disc level inside it.
fn looks_like_an_artist_level(path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false)
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| !crate::discs::is_disc_designator(name))
    })
}

/// True when `path` holds a top-level file that parses as audio: positive
/// evidence of a legacy album directory.
///
/// The stager only ever writes audio at an album's top level, so audio is what
/// tells a legacy album directory apart from an artist level whose albums are
/// named like disc folders. A directory holding only sidecars (a `cover.jpg`, a
/// `.DS_Store`, a `.part`) is evidence of neither, and is left in place; an empty
/// directory is handled before this predicate is reached.
fn looks_like_a_legacy_album(path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    entries.flatten().any(|entry| {
        !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false)
            && crate::organizer::parses_as_audio(&entry.path())
    })
}

/// True when `path` is, or lies inside, `root`. Both sides are resolved, so a
/// symlinked or `.`-containing spelling of either still matches, and a path that
/// does not exist yet is resolved through its deepest existing ancestor instead
/// of failing the comparison.
pub(crate) fn is_inside(root: &Path, path: &Path) -> bool {
    match (canonicalize_lenient(root), canonicalize_lenient(path)) {
        (Some(root), Some(path)) => path == root || path.starts_with(root),
        _ => false,
    }
}

/// True when two paths name the same place, whether or not it exists yet.
pub(crate) fn is_the_same_path(left: &Path, right: &Path) -> bool {
    match (canonicalize_lenient(left), canonicalize_lenient(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// Resolve a path that may not exist yet: canonicalise its deepest existing
/// ancestor and append the remaining components. `fs::canonicalize` requires
/// every component to exist, which is wrong for a path a run is about to create
/// — and the library-root guards must work before the first byte moves.
fn canonicalize_lenient(path: &Path) -> Option<PathBuf> {
    if let Ok(real) = std::fs::canonicalize(path) {
        return Some(real);
    }
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        if let Some(name) = current.file_name() {
            missing.push(name.to_os_string());
        }
        let parent = current.parent()?.to_path_buf();
        if let Ok(real) = std::fs::canonicalize(&parent) {
            let mut resolved = real;
            for name in missing.iter().rev() {
                resolved.push(name);
            }
            return Some(resolved);
        }
        current = parent;
    }
}

/// True when `path` holds no entries at all.
fn dir_is_empty(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
}

/// Move every entry of `legacy` that is absent at `destination`, recursing so a
/// nested disc folder keeps its shape. A name that already exists at the
/// destination is never overwritten: the destination entry stays and the legacy
/// copy is left where it is. An existing *directory* is recursed into, so an
/// entry inside it that is absent at the destination still moves.
///
/// `first_skipped` records the first colliding source entry, and `residue` the
/// first directory that was emptied but could not itself be removed, so the
/// caller can name the real path in its warning instead of the legacy root.
fn merge_without_overwrite(
    legacy: &Path,
    destination: &Path,
    first_skipped: &mut Option<PathBuf>,
    residue: &mut Option<PathBuf>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(legacy)? {
        let entry = entry?;
        let source = entry.path();
        let target = destination.join(entry.file_name());
        // A symlink at the destination is never written through: renaming would
        // replace the link and recursion would follow it into an unrelated tree.
        if std::fs::symlink_metadata(&target)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
        {
            first_skipped.get_or_insert(source);
            continue;
        }
        let source_is_dir = entry.file_type()?.is_dir();
        if target.exists() {
            if source_is_dir && target.is_dir() {
                merge_without_overwrite(&source, &target, first_skipped, residue)?;
                if let Err(e) = std::fs::remove_dir(&source) {
                    tracing::debug!("Merged {source:?} but could not remove it: {e}");
                    residue.get_or_insert(source);
                }
            } else {
                first_skipped.get_or_insert(source);
            }
            continue;
        }
        if source_is_dir {
            std::fs::create_dir_all(&target)?;
            merge_without_overwrite(&source, &target, first_skipped, residue)?;
            if let Err(e) = std::fs::remove_dir(&source) {
                tracing::debug!("Merged {source:?} but could not remove it: {e}");
                residue.get_or_insert(source);
            }
        } else {
            std::fs::rename(&source, &target)?;
        }
    }
    Ok(())
}

/// Remove a top-level directory that holds nothing; report one that holds files
/// but no subdirectory, because in a nested layout that shape is ambiguous and
/// deleting real content is worse than leaving junk.
///
/// Only the top level is pruned. A second level cannot be pruned safely: the
/// staging root may hold directories that are not artist levels at all (a
/// library root configured beneath it, for one), and removing an empty
/// directory below the top level destroyed a real, intentionally empty library
/// artist folder when that was tried.
fn prune_or_report_artist_dir(path: &Path, report: &mut MigrationReport) {
    let Ok(entries) = std::fs::read_dir(path) else {
        report
            .failed
            .push((path.to_path_buf(), "unreadable".to_string()));
        return;
    };
    let mut holds_anything = false;
    let mut holds_a_subdirectory = false;
    for entry in entries.flatten() {
        holds_anything = true;
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            holds_a_subdirectory = true;
        }
    }
    if !holds_anything {
        match std::fs::remove_dir(path) {
            Ok(()) => report.pruned_artist_dirs.push(path.to_path_buf()),
            Err(e) => report.failed.push((path.to_path_buf(), e.to_string())),
        }
    } else if !holds_a_subdirectory {
        report.unrecognised.push(path.to_path_buf());
    }
}

/// One album found under the staging root.
#[derive(Debug)]
pub struct StagedAlbum {
    /// `<staging_dir>/<artist>` — the level to prune when the album leaves.
    pub artist_dir_path: PathBuf,
    /// `<staging_dir>/<artist>/<album>`.
    pub album_dir_path: PathBuf,
    /// The album directory name as it is on disk (the sanitised component).
    pub dir_album: String,
    /// `None` when no `processed_albums` row matches: an untracked leftover.
    pub identity: Option<StagedIdentity>,
}

/// Raw names and status, as stored in the database.
#[derive(Debug)]
pub struct StagedIdentity {
    pub artist: String,
    pub album: String,
    pub status: String,
}

/// Walk the two levels under the staging root and resolve each album's identity.
///
/// The directory names are sanitised while the database rows are raw, so an
/// exact lookup would miss a title such as `Gold: Greatest Hits` and the
/// leftover would be deleted as debris. Rows are matched by sanitising both
/// sides, case-insensitively, which is what the database's own `COLLATE NOCASE`
/// lookup does.
pub fn scan(staging_dir: &Path, db: &crate::db::Database) -> Result<Vec<StagedAlbum>> {
    let rows = db.get_processed_albums()?;
    let mut albums = Vec::new();
    let artist_entries = match std::fs::read_dir(staging_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(albums),
        Err(e) => return Err(e.into()),
    };

    for artist_entry in artist_entries {
        let artist_entry = artist_entry?;
        if !artist_entry.file_type()?.is_dir() {
            continue;
        }
        let artist_dir_path = artist_entry.path();
        let Some(artist_name) = artist_dir_path.file_name().and_then(|n| n.to_str()) else {
            tracing::warn!(
                "Staging holds the artist directory {artist_dir_path:?}, whose name is not valid UTF-8; recovery cannot match it to a processed album"
            );
            continue;
        };
        // A top-level directory that carries the legacy separator is one the
        // migration left alone: a blocked merge, a failed rewrite, an ambiguous
        // tree, a legacy album directory whose rename was refused, or an artist
        // level whose own name carries the separator. Its children are that
        // album's own files or disc folders, not staged albums of this layout —
        // matching them to a row would resume the wrong album, and matching none
        // would delete them. An artist level named with the separator is skipped
        // too: leaving its albums in place is preferred to deleting anything
        // below it (see the design's dated note).
        if artist_name.contains("--") {
            tracing::debug!("Staging holds the unmigrated directory {artist_name:?}; skipping it");
            continue;
        }
        for album_entry in std::fs::read_dir(&artist_dir_path)? {
            let album_entry = album_entry?;
            if !album_entry.file_type()?.is_dir() {
                continue;
            }
            let album_dir_path = album_entry.path();
            let Some(album_name) = album_dir_path.file_name().and_then(|n| n.to_str()) else {
                tracing::warn!(
                    "Staging holds the album directory {album_dir_path:?}, whose name is not valid UTF-8; recovery cannot match it to a processed album"
                );
                continue;
            };
            // Own the directory name before the path moves into the struct:
            // the literal moves `album_dir_path` while `album_name` borrows it.
            let dir_album = album_name.to_string();
            let identity = resolve_identity(&rows, artist_name, &dir_album);
            albums.push(StagedAlbum {
                artist_dir_path: artist_dir_path.clone(),
                album_dir_path,
                dir_album,
                identity,
            });
        }
    }

    Ok(albums)
}

/// Find the `processed_albums` row a staging directory belongs to.
///
/// A `success` row outranks a newer row of any other status, because a success
/// row means the album was already written to the library: deleting its
/// directory loses the album, whereas resuming a `failed` row's directory would
/// only re-verify it. Among rows of equal standing the highest `id` wins, and
/// the ambiguity is warned about, so a shared sanitised name still recovers the
/// most recently tried album.
fn resolve_identity(
    rows: &[crate::db::ProcessedAlbum],
    artist_dir: &str,
    album_dir: &str,
) -> Option<StagedIdentity> {
    let mut matches: Vec<&crate::db::ProcessedAlbum> = rows
        .iter()
        .filter(|row| {
            crate::organizer::sanitize_component(&row.artist).eq_ignore_ascii_case(artist_dir)
                && crate::organizer::sanitize_component(&row.album).eq_ignore_ascii_case(album_dir)
        })
        .collect();
    matches.sort_by_key(|row| (row.status == "success", row.id));
    let (preferred, older) = matches.split_last()?;
    if let Some(first) = older.first() {
        tracing::warn!(
            "Staging {artist_dir:?}/{album_dir:?} matches several processed albums; using {:?} - {:?} (status {:?}), which also matches {:?} - {:?} (status {:?})",
            preferred.artist,
            preferred.album,
            preferred.status,
            first.artist,
            first.album,
            first.status
        );
    }
    Some(StagedIdentity {
        artist: preferred.artist.clone(),
        album: preferred.album.clone(),
        status: preferred.status.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::organizer::sanitize_component;

    #[test]
    fn artist_dir_sanitises_the_artist_component() {
        assert_eq!(
            artist_dir(Path::new("/staging"), "AC/DC"),
            Path::new("/staging/AC-DC"),
            "a path separator in an artist name must not create a level"
        );
    }

    #[test]
    fn album_dir_sanitises_the_album_component() {
        assert_eq!(
            album_dir(Path::new("/staging"), "Artist", Some("Gold: Greatest Hits")),
            Path::new("/staging/Artist/Gold Greatest Hits"),
            "a Windows-reserved character is removed by the library sanitiser"
        );
    }

    #[test]
    fn album_dir_uses_the_placeholder_for_a_component_that_sanitises_away() {
        assert_eq!(
            album_dir(Path::new("/staging"), "???", Some("Album")),
            Path::new("/staging/_/Album"),
            "a component that keeps nothing becomes the sanitiser's placeholder"
        );
    }

    #[test]
    fn album_dir_neutralises_a_reserved_device_name() {
        assert_eq!(
            album_dir(Path::new("/staging"), "Artist", Some("CON")),
            Path::new("/staging/Artist/_CON"),
            "a reserved device name is prefixed by the library sanitiser"
        );
    }

    #[test]
    fn album_dir_keeps_the_unknown_component_when_no_album_is_named() {
        assert_eq!(
            album_dir(Path::new("/staging"), "Artist", None),
            Path::new("/staging/Artist/unknown"),
            "an artist-only run keeps the album component it always had"
        );
    }

    #[test]
    fn album_dir_makes_an_album_only_run_a_two_level_tree() {
        assert_eq!(
            album_dir(Path::new("/staging"), "", Some("Orphan Album")),
            Path::new("/staging/_/Orphan Album"),
            "a blank artist still gets an artist level, so the tree depth is constant"
        );
    }

    #[test]
    fn album_dir_matches_the_library_components_for_a_hostile_title() {
        let staging = Path::new("/staging");
        let artist = "Gold: Greatest Hits";
        let album = "A/B .. ???";
        assert_eq!(
            album_dir(staging, artist, Some(album)),
            staging
                .join(sanitize_component(artist))
                .join(sanitize_component(album)),
            "staging must use one sanitiser, the same one the library write uses"
        );
    }

    use tempfile::TempDir;

    /// A top-level audio file: the evidence the migration needs before it will
    /// treat a separator-named directory as a legacy album directory.
    fn write_top_level_audio(dir: &Path) {
        crate::test_support::write_minimal_flac(&dir.join("01 - Track.flac"));
    }

    /// The permission-based migration tests need an euid that enforces the access
    /// checks they rely on: root bypasses them, so the operation they expect to
    /// fail would succeed. Probe for that instead of assuming it.
    #[cfg(unix)]
    fn permissions_are_enforced(root: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;

        let probe = root.join("permission-probe");
        if std::fs::write(&probe, b"probe").is_err() {
            return false;
        }
        if std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o000)).is_err() {
            return false;
        }
        let enforced = std::fs::read(&probe).is_err();
        let _ = std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o644));
        let _ = std::fs::remove_file(&probe);
        enforced
    }

    #[test]
    fn album_dir_shares_one_directory_for_names_that_sanitise_together() {
        // A deliberate limit: the sanitised components are the ones the library
        // path is built from, and the library already collapses these two titles
        // to one folder, so the staged albums share one directory rather than
        // gaining a discriminator the library side would not have.
        let staging = Path::new("/staging");
        assert_eq!(
            album_dir(staging, "Artist", Some("Album?")),
            album_dir(staging, "Artist", Some("Album*")),
        );
        assert_eq!(
            album_dir(staging, "Artist", Some("Album?")),
            Path::new("/staging/Artist/Album")
        );
    }

    #[test]
    fn normalise_layout_renames_a_legacy_slug_into_two_levels() {
        let root = TempDir::new().unwrap();
        let legacy = root.path().join("Artist--Album");
        std::fs::create_dir_all(legacy.join("CD 01")).unwrap();
        std::fs::write(legacy.join("CD 01").join("01 - One.flac"), b"audio").unwrap();
        std::fs::write(legacy.join("cover.jpg"), b"image").unwrap();
        write_top_level_audio(&legacy);

        let report = normalise_layout(root.path()).unwrap();

        assert_eq!(
            report.migrated,
            vec![legacy.clone()],
            "a legacy slug with no nested destination is renamed wholesale"
        );
        assert!(!legacy.exists(), "the legacy directory must be gone");
        assert_eq!(
            std::fs::read(root.path().join("Artist").join("Album").join("cover.jpg")).unwrap(),
            b"image"
        );
        assert!(
            root.path()
                .join("Artist")
                .join("Album")
                .join("CD 01")
                .join("01 - One.flac")
                .exists(),
            "a nested disc folder keeps its shape"
        );
    }

    #[test]
    fn normalise_layout_prunes_an_empty_top_level_directory() {
        let root = TempDir::new().unwrap();
        let empty = root.path().join("Empty Artist");
        std::fs::create_dir_all(&empty).unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert_eq!(report.pruned_artist_dirs, vec![empty.clone()]);
        assert!(!empty.exists(), "an empty artist level must not accumulate");
    }

    #[test]
    fn normalise_layout_leaves_a_populated_artist_directory_alone() {
        let root = TempDir::new().unwrap();
        let artist = root.path().join("Artist");
        std::fs::create_dir_all(artist.join("Album")).unwrap();
        std::fs::write(artist.join("Album").join("01 - One.flac"), b"audio").unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert!(report.pruned_artist_dirs.is_empty());
        assert!(report.migrated.is_empty());
        assert!(artist.join("Album").join("01 - One.flac").exists());
    }

    #[test]
    fn normalise_layout_creates_a_missing_staging_root() {
        let root = TempDir::new().unwrap();
        let missing = root.path().join("not-created-yet");

        let report = prepare(&missing, &[]).unwrap();

        assert!(missing.is_dir(), "prepare must create the staging root");
        assert!(report.migrated.is_empty());
        assert!(report.failed.is_empty());
    }

    #[test]
    fn prepare_over_an_already_nested_tree_changes_nothing() {
        let root = TempDir::new().unwrap();
        let album = root.path().join("Artist").join("Album");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01 - One.flac"), b"audio").unwrap();

        let first = prepare(root.path(), &[]).unwrap();
        let second = prepare(root.path(), &[]).unwrap();

        assert!(first.migrated.is_empty() && first.merged.is_empty());
        assert!(second.migrated.is_empty() && second.merged.is_empty());
        assert!(
            album.join("01 - One.flac").exists(),
            "prepare is idempotent"
        );
    }

    #[test]
    fn normalise_layout_merges_into_an_existing_destination_without_overwriting() {
        let root = TempDir::new().unwrap();
        let legacy = root.path().join("Artist--Album");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("collision.flac"), b"legacy copy").unwrap();
        std::fs::write(legacy.join("only-legacy.flac"), b"moves").unwrap();
        write_top_level_audio(&legacy);

        let destination = root.path().join("Artist").join("Album");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("collision.flac"), b"destination copy").unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert_eq!(
            std::fs::read(destination.join("collision.flac")).unwrap(),
            b"destination copy",
            "the destination file must win"
        );
        assert_eq!(
            std::fs::read(destination.join("only-legacy.flac")).unwrap(),
            b"moves",
            "an entry absent at the destination still moves"
        );
        assert_eq!(
            report.left_in_place,
            vec![(legacy.clone(), legacy.join("collision.flac"))],
            "a directory still holding a skipped entry is reported, with the entry that stopped it"
        );
        assert!(
            legacy.join("collision.flac").exists(),
            "the skipped legacy copy is left where it is"
        );
        assert!(
            report.merged.is_empty(),
            "an incomplete merge is not a merge"
        );
    }

    #[test]
    fn normalise_layout_removes_a_legacy_directory_the_merge_emptied() {
        let root = TempDir::new().unwrap();
        let legacy = root.path().join("Artist--Album");
        std::fs::create_dir_all(legacy.join("CD 01")).unwrap();
        std::fs::write(legacy.join("CD 01").join("01 - One.flac"), b"audio").unwrap();
        std::fs::write(legacy.join("cover.jpg"), b"image").unwrap();
        write_top_level_audio(&legacy);

        let destination = root.path().join("Artist").join("Album");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("notes.txt"), b"notes").unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert_eq!(report.merged, vec![legacy.clone()]);
        assert!(!legacy.exists(), "an emptied legacy directory is removed");
        assert!(
            destination.join("CD 01").join("01 - One.flac").exists(),
            "a nested disc folder keeps its shape through the merge"
        );
        assert_eq!(
            std::fs::read(destination.join("cover.jpg")).unwrap(),
            b"image"
        );
    }

    #[test]
    fn normalise_layout_leaves_a_byte_invalid_name_alone() {
        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;

            let root = TempDir::new().unwrap();
            let invalid = root
                .path()
                .join(OsString::from_vec(b"Artist\xff--Album".to_vec()));
            std::fs::create_dir_all(&invalid).unwrap();

            let report = normalise_layout(root.path()).unwrap();

            assert_eq!(report.non_utf8, vec![invalid.clone()]);
            assert!(invalid.exists(), "a lossy rename could orphan an album");
        }
    }

    #[test]
    fn normalise_layout_reports_a_top_level_directory_holding_only_files() {
        let root = TempDir::new().unwrap();
        let ambiguous = root.path().join("stray-files");
        std::fs::create_dir_all(&ambiguous).unwrap();
        std::fs::write(ambiguous.join("01 - One.flac"), b"audio").unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert_eq!(report.unrecognised, vec![ambiguous.clone()]);
        assert!(
            ambiguous.exists(),
            "deleting real content is worse than leaving junk"
        );
    }

    use crate::db::Database;

    #[test]
    fn scan_resolves_a_sanitised_directory_name_to_the_raw_row() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("Artist").join("Gold Greatest Hits")).unwrap();
        let db = Database::open_in_memory().unwrap();
        db.mark_album_processed("Artist", "Gold: Greatest Hits", "success")
            .unwrap();

        let staged = scan(root.path(), &db).unwrap();

        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].dir_album, "Gold Greatest Hits");
        let identity = staged[0].identity.as_ref().expect("a matching row");
        assert_eq!(
            identity.artist, "Artist",
            "the raw name is what recovery needs"
        );
        assert_eq!(identity.album, "Gold: Greatest Hits");
        assert_eq!(identity.status, "success");
    }

    #[test]
    fn scan_matches_case_insensitively_like_the_database_lookup() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("artist").join("album")).unwrap();
        let db = Database::open_in_memory().unwrap();
        db.mark_album_processed("ARTIST", "ALBUM", "failed")
            .unwrap();

        let staged = scan(root.path(), &db).unwrap();

        assert!(
            staged[0].identity.is_some(),
            "SQLite COLLATE NOCASE is ASCII-insensitive"
        );
    }

    #[test]
    fn scan_returns_no_identity_for_an_untracked_album_directory() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("_").join("Album")).unwrap();
        let db = Database::open_in_memory().unwrap();

        let staged = scan(root.path(), &db).unwrap();

        assert_eq!(staged.len(), 1, "an album-only leftover is still an album");
        assert!(staged[0].identity.is_none());
        assert_eq!(staged[0].dir_album, "Album");
    }

    #[test]
    fn scan_prefers_the_highest_id_when_several_rows_match() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("Artist").join("Album")).unwrap();
        let db = Database::open_in_memory().unwrap();
        // Two raw names that sanitise to the same two directory names, with the
        // same status, so only the id can decide between them.
        db.mark_album_processed("Artist", "Album", "failed")
            .unwrap();
        db.mark_album_processed("Artist", "Album?", "failed")
            .unwrap();

        let staged = scan(root.path(), &db).unwrap();

        let identity = staged[0].identity.as_ref().expect("a matching row");
        assert_eq!(
            identity.album, "Album?",
            "among equal statuses the highest id is the most recently tried album"
        );
    }

    #[test]
    fn normalise_layout_leaves_an_ambiguous_legacy_named_tree_in_place() {
        // Only disc-named children and no file at the top level: a legacy
        // multi-disc album directory and an artist level whose albums are all
        // named like disc folders are the same shape, so neither may be renamed.
        let root = TempDir::new().unwrap();
        let ambiguous = root.path().join("AC--DC");
        std::fs::create_dir_all(ambiguous.join("Gold (Disc 1)")).unwrap();
        std::fs::write(
            ambiguous.join("Gold (Disc 1)").join("01 - One.flac"),
            b"audio",
        )
        .unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert_eq!(report.ambiguous, vec![ambiguous.clone()]);
        assert!(report.migrated.is_empty(), "got {report:?}");
        assert!(
            ambiguous
                .join("Gold (Disc 1)")
                .join("01 - One.flac")
                .exists(),
            "an ambiguous tree must not be moved"
        );
        assert!(!root.path().join("AC").exists());
    }

    #[test]
    fn scan_skips_an_artist_level_named_with_the_separator() {
        // An artist level whose own name carries the separator is skipped as well:
        // its albums are left in place rather than recovered, because a child that
        // is not an album (a `Scans/` folder the operator added) would otherwise be
        // matched to no row and deleted as debris.
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("AC--DC").join("Great Hits")).unwrap();
        std::fs::create_dir_all(root.path().join("AC--DC").join("Scans")).unwrap();
        std::fs::write(
            root.path().join("AC--DC").join("Scans").join("cover.jpg"),
            b"image",
        )
        .unwrap();
        let db = Database::open_in_memory().unwrap();
        db.mark_album_processed("AC--DC", "Great Hits", "success")
            .unwrap();

        assert!(scan(root.path(), &db).unwrap().is_empty());
        assert!(
            root.path()
                .join("AC--DC")
                .join("Scans")
                .join("cover.jpg")
                .exists(),
            "nothing below a separator-named level may be attributed or cleaned"
        );
    }

    #[test]
    fn normalise_layout_merges_into_an_existing_empty_nested_directory() {
        // An existing destination *directory* is recursed into, so a legacy file
        // inside it still moves rather than being abandoned. The top-level file is
        // what identifies the tree as a legacy album directory at all.
        let root = TempDir::new().unwrap();
        let legacy = root.path().join("Artist--Album");
        std::fs::create_dir_all(legacy.join("CD 01")).unwrap();
        std::fs::write(legacy.join("CD 01").join("01 - One.flac"), b"audio").unwrap();
        std::fs::write(legacy.join("cover.jpg"), b"image").unwrap();
        write_top_level_audio(&legacy);
        let destination = root.path().join("Artist").join("Album").join("CD 01");
        std::fs::create_dir_all(&destination).unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert_eq!(report.merged, vec![legacy.clone()]);
        assert!(
            destination.join("01 - One.flac").exists(),
            "a file inside an existing nested directory still moves"
        );
        assert_eq!(
            std::fs::read(root.path().join("Artist").join("Album").join("cover.jpg")).unwrap(),
            b"image"
        );
        assert!(!legacy.exists());
    }

    #[test]
    fn scan_prefers_a_success_row_over_a_newer_failed_one() {
        // The success row is the album that was already written to the library.
        // Deleting its directory loses the album, so status outranks recency.
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("Artist").join("Album")).unwrap();
        let db = Database::open_in_memory().unwrap();
        db.mark_album_processed("Artist", "Album", "success")
            .unwrap();
        db.mark_album_processed("Artist", "Album?", "failed")
            .unwrap();

        let staged = scan(root.path(), &db).unwrap();

        let identity = staged[0].identity.as_ref().expect("a matching row");
        assert_eq!(
            identity.status, "success",
            "a success row must outrank a newer failure"
        );
        assert_eq!(identity.album, "Album");
    }

    #[test]
    fn scan_skips_a_directory_the_migration_left_unmigrated() {
        // A blocked or failed migration leaves a legacy directory in place. Its
        // children are that album's own files or disc folders, not staged albums
        // of the current layout, so resolving them against the table and cleaning
        // them would destroy the album.
        let root = TempDir::new().unwrap();
        let legacy = root.path().join("Artist--Album");
        std::fs::create_dir_all(legacy.join("CD 01")).unwrap();
        std::fs::write(legacy.join("CD 01").join("01 - One.flac"), b"audio").unwrap();
        let db = Database::open_in_memory().unwrap();

        assert!(scan(root.path(), &db).unwrap().is_empty());
        assert!(legacy.join("CD 01").join("01 - One.flac").exists());
    }

    #[test]
    fn scan_reports_every_album_level_under_one_artist() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("Artist").join("One")).unwrap();
        std::fs::create_dir_all(root.path().join("Artist").join("Two")).unwrap();
        let db = Database::open_in_memory().unwrap();

        let mut names: Vec<String> = scan(root.path(), &db)
            .unwrap()
            .into_iter()
            .map(|album| album.dir_album)
            .collect();
        names.sort();

        assert_eq!(names, vec!["One".to_string(), "Two".to_string()]);
    }

    #[test]
    fn scan_tolerates_a_missing_staging_root() {
        let root = TempDir::new().unwrap();
        let db = Database::open_in_memory().unwrap();

        assert!(scan(&root.path().join("absent"), &db).unwrap().is_empty());
    }

    #[test]
    fn normalise_layout_tolerates_a_missing_root() {
        // A missing root is the normal state of a first run, not an error.
        let root = TempDir::new().unwrap();

        let report = normalise_layout(&root.path().join("absent")).unwrap();

        assert!(report.migrated.is_empty());
        assert!(report.failed.is_empty());
    }

    #[test]
    fn normalise_layout_propagates_a_root_that_is_not_a_directory() {
        // Only a missing root is tolerated; any other read failure is real.
        let root = TempDir::new().unwrap();
        let file = root.path().join("not-a-directory");
        std::fs::write(&file, b"contents").unwrap();

        assert!(normalise_layout(&file).is_err());
    }

    #[test]
    fn normalise_layout_merges_a_nested_collision_without_overwriting() {
        // The collision sits one level below the album directory, so only the
        // recursive walk finds it.
        let root = TempDir::new().unwrap();
        let legacy = root.path().join("Artist--Album");
        std::fs::create_dir_all(legacy.join("CD 01")).unwrap();
        std::fs::write(legacy.join("CD 01").join("01 - One.flac"), b"legacy").unwrap();
        std::fs::write(legacy.join("keep-me.flac"), b"moves").unwrap();
        write_top_level_audio(&legacy);
        let destination = root.path().join("Artist").join("Album").join("CD 01");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("01 - One.flac"), b"destination").unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert_eq!(
            std::fs::read(destination.join("01 - One.flac")).unwrap(),
            b"destination",
            "a nested collision keeps the destination copy"
        );
        assert_eq!(
            std::fs::read(
                root.path()
                    .join("Artist")
                    .join("Album")
                    .join("keep-me.flac")
            )
            .unwrap(),
            b"moves",
            "an entry with no collision still leaves the legacy tree"
        );
        assert_eq!(report.left_in_place.len(), 1, "got {report:?}");
        assert!(
            report.failed.is_empty(),
            "a skipped entry is not a failed rewrite: {report:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn normalise_layout_reports_an_unreadable_legacy_named_directory() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().unwrap();
        if !permissions_are_enforced(root.path()) {
            return;
        }
        let locked = root.path().join("Artist--Album");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let report = normalise_layout(root.path()).unwrap();

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(report.failed.len(), 1, "got {report:?}");
        assert!(report.failed[0].1.contains("unreadable"));
        assert!(
            report.ambiguous.is_empty(),
            "an unreadable directory is a failure, not an ambiguous shape"
        );
    }

    #[test]
    fn prepare_logs_every_anomaly_kind_without_aborting() {
        // One run past a fixture that trips all four startup warnings, so the
        // operator-facing log path is executed and not merely the report built.
        let root = TempDir::new().unwrap();
        // 1. A legacy directory whose artist level is blocked by a file.
        std::fs::write(root.path().join("Blocked Artist"), b"not a directory").unwrap();
        let blocked = root.path().join("Blocked Artist--Album");
        std::fs::create_dir_all(&blocked).unwrap();
        write_top_level_audio(&blocked);
        // 2. A merge that leaves an entry behind.
        let legacy = root.path().join("Merge Artist--Album");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("same.flac"), b"legacy").unwrap();
        write_top_level_audio(&legacy);
        let destination = root.path().join("Merge Artist").join("Album");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("same.flac"), b"destination").unwrap();
        // 3. A top-level directory that is neither a legacy slug nor an artist level.
        let stray = root.path().join("stray-files");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(stray.join("01 - One.flac"), b"audio").unwrap();
        // 4. A name that is not valid UTF-8.
        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;

            std::fs::create_dir_all(root.path().join(OsString::from_vec(b"Bad\xff".to_vec())))
                .unwrap();
        }

        let capture = crate::test_support::LogCapture::start();
        let report = prepare(root.path(), &[]).unwrap();
        let logs = capture.text();

        assert_eq!(report.failed.len(), 1, "got {report:?}");
        assert_eq!(report.left_in_place.len(), 1, "got {report:?}");
        assert_eq!(report.unrecognised, vec![stray.clone()]);
        #[cfg(unix)]
        assert_eq!(report.non_utf8.len(), 1, "got {report:?}");
        #[cfg(not(unix))]
        assert!(report.non_utf8.is_empty());
        // The operator-facing half: a report nobody logs is not a warning, so
        // assert the wording each anomaly produces at startup.
        assert!(
            logs.contains("could not rewrite"),
            "missing failed warning:\n{logs}"
        );
        assert!(
            logs.contains("already exists at the destination"),
            "missing left-in-place warning:\n{logs}"
        );
        assert!(
            logs.contains("neither a legacy artist--album directory"),
            "missing unrecognised warning:\n{logs}"
        );
        #[cfg(unix)]
        assert!(
            logs.contains("not valid UTF-8"),
            "missing non-UTF-8 warning:\n{logs}"
        );
    }

    #[test]
    fn normalise_layout_leaves_an_artist_level_whose_albums_are_all_disc_shaped() {
        // A stray sidecar is not evidence of a legacy album directory: the tree
        // below is an artist level whose albums are named like disc folders, and
        // renaming it would fold the artist's whole staged tree into a fabricated
        // album directory that recovery then deletes.
        let root = TempDir::new().unwrap();
        let artist = root.path().join("AC--DC");
        std::fs::create_dir_all(artist.join("Gold (Disc 1)")).unwrap();
        std::fs::write(artist.join("Gold (Disc 1)").join("01 - One.flac"), b"audio").unwrap();
        std::fs::write(artist.join(".DS_Store"), b"sidecar").unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert_eq!(report.ambiguous, vec![artist.clone()], "got {report:?}");
        assert!(report.migrated.is_empty(), "got {report:?}");
        assert!(artist.join("Gold (Disc 1)").join("01 - One.flac").exists());
        assert!(!root.path().join("AC").exists());
    }

    #[test]
    fn normalise_layout_leaves_an_artist_level_whose_name_contains_the_separator() {
        // An artist whose sanitised name contains `--` is written by the current
        // layout. Renaming it would push the album a level deeper and hand it to
        // recovery as untracked debris.
        let root = TempDir::new().unwrap();
        let artist = root.path().join("AC--DC");
        std::fs::create_dir_all(artist.join("Album")).unwrap();
        std::fs::write(artist.join("Album").join("01 - One.flac"), b"audio").unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert!(report.migrated.is_empty(), "got {report:?}");
        assert!(artist.join("Album").join("01 - One.flac").exists());
        assert!(
            !root.path().join("AC").exists(),
            "nothing may be reinterpreted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn normalise_layout_records_every_rewrite_it_cannot_perform() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().unwrap();
        if !permissions_are_enforced(root.path()) {
            return;
        }
        // An empty artist level that cannot be pruned without write access.
        let empty = root.path().join("Empty Artist");
        std::fs::create_dir_all(&empty).unwrap();
        // A merge whose entry cannot be renamed out of the legacy tree.
        let legacy = root.path().join("Merge Artist--Album");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("only.flac"), b"blocked").unwrap();
        write_top_level_audio(&legacy);
        std::fs::create_dir_all(root.path().join("Merge Artist").join("Album")).unwrap();

        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let report = normalise_layout(root.path()).unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(report.failed.len(), 2, "got {report:?}");
        assert!(report.pruned_artist_dirs.is_empty());
        assert!(
            root.path()
                .join("Merge Artist")
                .join("Album")
                .join("only.flac")
                .exists(),
            "the merge moves every entry before the removal can fail"
        );
        assert!(
            legacy.exists(),
            "an emptied legacy directory that cannot be removed stays on disk"
        );
        assert!(
            report.failed.iter().all(|(_, error)| !error.is_empty()),
            "every failure names its cause"
        );
    }

    #[test]
    fn scan_ignores_entries_that_are_not_albums() {
        // A loose file at the staging root, a loose file inside an artist
        // level, and a name that is not valid UTF-8 are all skipped rather than
        // resolved against a row they could never match.
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("loose.flac"), b"audio").unwrap();
        let artist = root.path().join("Artist");
        std::fs::create_dir_all(&artist).unwrap();
        std::fs::write(artist.join("cover.jpg"), b"image").unwrap();
        std::fs::create_dir_all(artist.join("Album")).unwrap();
        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;

            std::fs::create_dir_all(artist.join(OsString::from_vec(b"Bad\xff".to_vec()))).unwrap();
            std::fs::create_dir_all(root.path().join(OsString::from_vec(b"Bad\xff".to_vec())))
                .unwrap();
        }
        let db = Database::open_in_memory().unwrap();

        let albums = scan(root.path(), &db).unwrap();

        assert_eq!(albums.len(), 1, "got {albums:?}");
        assert_eq!(albums[0].dir_album, "Album");
    }

    #[test]
    fn scan_propagates_a_staging_root_that_is_not_a_directory() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("not-a-directory");
        std::fs::write(&file, b"contents").unwrap();
        let db = Database::open_in_memory().unwrap();

        assert!(scan(&file, &db).is_err());
    }

    #[test]
    fn normalise_layout_removes_an_empty_legacy_named_directory() {
        // Nothing to lose, and renaming it would leave an empty nested pair that
        // no later pass prunes.
        let root = TempDir::new().unwrap();
        let empty = root.path().join("Artist--Album");
        std::fs::create_dir_all(&empty).unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert_eq!(report.pruned_artist_dirs, vec![empty.clone()]);
        assert!(!empty.exists());
        assert!(!root.path().join("Artist").exists());
    }

    #[test]
    fn normalise_layout_leaves_a_legacy_named_directory_holding_only_sidecars() {
        // Audio is the evidence. A directory of sidecars could equally be an
        // artist level with a stray cover, so it is not renamed into a fabricated
        // album folder.
        let root = TempDir::new().unwrap();
        let sidecars = root.path().join("Artist--Album");
        std::fs::create_dir_all(&sidecars).unwrap();
        std::fs::write(sidecars.join("cover.jpg"), b"image").unwrap();
        std::fs::write(sidecars.join("notes.txt"), b"notes").unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert_eq!(report.ambiguous, vec![sidecars.clone()], "got {report:?}");
        assert!(report.migrated.is_empty(), "got {report:?}");
        assert!(sidecars.join("cover.jpg").exists());
    }

    #[test]
    fn prepare_never_touches_a_configured_library_root() {
        // staging_dir as the parent of a library root is the configuration the
        // README forbids, and the migration must not be what makes it destructive:
        // the root is neither pruned nor renamed into.
        let staging = TempDir::new().unwrap();
        let library = staging.path().join("Music");
        std::fs::create_dir_all(&library).unwrap();
        let legacy = staging.path().join("Music--Legacy");
        std::fs::create_dir_all(&legacy).unwrap();
        write_top_level_audio(&legacy);

        let report = prepare(staging.path(), &[library.to_string_lossy().into_owned()]).unwrap();

        assert!(library.exists(), "an empty library root must not be pruned");
        assert!(
            !report.protected.contains(&library),
            "the root itself is skipped silently rather than warned about: {report:?}"
        );
        assert!(
            report.protected.contains(&legacy),
            "a legacy directory whose destination is the library root is protected: {report:?}"
        );
        assert!(
            legacy.exists(),
            "nothing may be renamed into the library root"
        );
    }

    #[cfg(unix)]
    #[test]
    fn normalise_layout_never_writes_through_a_symlinked_destination_entry() {
        // A symlink at the destination is treated as a name collision: renaming
        // would replace the link, and recursing would follow it into an unrelated
        // tree. Nothing is written through it.
        let root = TempDir::new().unwrap();
        let legacy = root.path().join("Artist--Album");
        std::fs::create_dir_all(legacy.join("CD 01")).unwrap();
        std::fs::write(legacy.join("CD 01").join("01 - One.flac"), b"audio").unwrap();
        std::fs::write(legacy.join("cover.jpg"), b"image").unwrap();
        write_top_level_audio(&legacy);
        let destination = root.path().join("Artist").join("Album");
        std::fs::create_dir_all(&destination).unwrap();
        std::os::unix::fs::symlink("nowhere", destination.join("CD 01")).unwrap();

        let report = normalise_layout(root.path()).unwrap();

        assert_eq!(report.left_in_place.len(), 1, "got {report:?}");
        assert!(
            report.failed.is_empty(),
            "a refused collision is not a failure: {report:?}"
        );
        assert!(destination.join("CD 01").is_symlink());
        assert!(legacy.join("CD 01").join("01 - One.flac").exists());
    }

    #[cfg(unix)]
    #[test]
    fn normalise_layout_reports_an_unreadable_artist_directory() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().unwrap();
        if !permissions_are_enforced(root.path()) {
            return;
        }
        let artist = root.path().join("Locked Artist");
        std::fs::create_dir_all(&artist).unwrap();
        std::fs::set_permissions(&artist, std::fs::Permissions::from_mode(0o000)).unwrap();

        let report = normalise_layout(root.path()).unwrap();

        std::fs::set_permissions(&artist, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(report.failed.len(), 1, "got {report:?}");
        assert!(report.failed[0].1.contains("unreadable"));
    }
}
