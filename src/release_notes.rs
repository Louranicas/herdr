use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const PENDING_RELEASE_NOTES_PATH: &str = "release-notes.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseNotes {
    pub version: String,
    pub body: String,
    pub preview: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredReleaseNotes {
    version: String,
    body: String,
    #[serde(default = "default_show_on_startup")]
    show_on_startup: bool,
}

fn default_show_on_startup() -> bool {
    true
}

pub fn pending_path() -> PathBuf {
    let mut path = crate::config::config_path();
    path.set_file_name(PENDING_RELEASE_NOTES_PATH);
    path
}

pub fn save_pending(version: &str, body: &str) -> std::io::Result<()> {
    save_pending_to_path(&pending_path(), version, body)
}

fn save_pending_to_path(path: &Path, version: &str, body: &str) -> std::io::Result<()> {
    let body = normalize_body(body);
    if body.is_empty() {
        return clear_pending_at(path);
    }

    write_stored_to_path(
        path,
        &StoredReleaseNotes {
            version: version.to_string(),
            body,
            show_on_startup: true,
        },
    )
}

fn write_stored_to_path(path: &Path, stored: &StoredReleaseNotes) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(stored).map_err(std::io::Error::other)?;
    let tmp_path = path.with_extension(format!("json.tmp.{}", std::process::id()));
    fs::write(&tmp_path, json)?;
    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(())
}

fn load_stored_from_path(path: &Path) -> Option<StoredReleaseNotes> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn load_latest() -> Option<ReleaseNotes> {
    // The FULL build identity, not BASE_VERSION. Comparing on the base alone
    // made every non-stable identity equal, so successive preview builds
    // (0.8.0-preview.123 -> .124) could never show newer notes. Comparing on
    // the display string with `Version::parse` was the opposite failure: it
    // parses to None and disables the check entirely.
    load_latest_from_path(&pending_path(), &crate::build_info::version())
}

fn load_latest_from_path(path: &Path, current_version: &str) -> Option<ReleaseNotes> {
    let stored = load_stored_from_path(path)?;
    release_notes_from_stored(stored, current_version)
}

fn release_notes_from_stored(
    stored: StoredReleaseNotes,
    current_version: &str,
) -> Option<ReleaseNotes> {
    let body = normalize_body(&stored.body);
    if body.is_empty() {
        return None;
    }

    let preview = match (
        crate::update::BuildIdentity::parse(&stored.version),
        crate::update::BuildIdentity::parse(current_version),
    ) {
        // `None` means the two identities are not comparable - different
        // channels at the same release. Not comparable is not "newer".
        (Some(stored_identity), Some(current_identity)) => stored_identity
            .is_newer_than(&current_identity)
            .unwrap_or(false),
        // An unparseable identity on either side is not evidence of newer
        // notes. Refusing here keeps a malformed version from presenting
        // stale notes as an update.
        _ => false,
    };

    Some(ReleaseNotes {
        preview,
        version: stored.version,
        body,
    })
}

pub fn mark_current_version_seen() -> std::io::Result<()> {
    mark_current_version_seen_at(&pending_path(), &crate::build_info::version())
}

fn mark_current_version_seen_at(path: &Path, current_version: &str) -> std::io::Result<()> {
    let Some(mut stored) = load_stored_from_path(path) else {
        return Ok(());
    };
    if stored.version != current_version || !stored.show_on_startup {
        return Ok(());
    }
    stored.show_on_startup = false;
    write_stored_to_path(path, &stored)
}

fn clear_pending_at(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        fs::remove_file(path)
    } else {
        Ok(())
    }
}

pub fn load_preview_from_local_changelog(version: &str) -> Option<ReleaseNotes> {
    let path = Path::new("CHANGELOG.md");
    let content = fs::read_to_string(path).ok()?;
    let body = extract_version_section(&content, version)?;
    Some(ReleaseNotes {
        version: version.to_string(),
        body: normalize_body(&body),
        preview: true,
    })
}

fn extract_version_section(content: &str, version: &str) -> Option<String> {
    let header = format!("## [{version}]");
    let mut collecting = false;
    let mut lines = Vec::new();

    for line in content.lines() {
        if !collecting {
            if line.starts_with(&header) {
                collecting = true;
            }
            continue;
        }

        if line.starts_with("## [") {
            break;
        }

        lines.push(line);
    }

    let body = lines.join("\n").trim().to_string();
    (!body.is_empty()).then_some(body)
}

pub fn normalize_body(body: &str) -> String {
    body.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {

    /// The caller, not the comparator. `BuildIdentity` ordering can be correct
    /// while `release_notes` still hands it the wrong strings — comparing on
    /// BASE_VERSION did exactly that, and every comparator test passed.
    #[test]
    fn a_newer_preview_build_is_reported_as_preview() {
        let stored = super::StoredReleaseNotes {
            version: "0.8.0-preview.124".to_string(),
            body: "### Changed\n- newer preview".to_string(),
            show_on_startup: true,
        };
        let notes = super::release_notes_from_stored(stored, "0.8.0-preview.123")
            .expect("newer notes must be reported");
        assert!(
            notes.preview,
            "0.8.0-preview.124 is newer than the running .123 and must read as preview"
        );
    }

    /// The caller, in the shape this project actually publishes.
    ///
    /// The numeric fixture above passes on a parser that cannot read a real
    /// preview id at all. `preview.yml` emits `<date>-<sha>`, so this is the
    /// case that decides whether a real preview build ever shows its notes.
    #[test]
    fn a_newer_real_dated_preview_is_reported_as_preview() {
        let stored = super::StoredReleaseNotes {
            version: "0.8.0-preview.2026-06-09-fedcba654321".to_string(),
            body: "### Changed\n- newer dated preview".to_string(),
            show_on_startup: true,
        };
        let notes =
            super::release_notes_from_stored(stored, "0.8.0-preview.2026-06-02-abcdef123456")
                .expect("newer notes must be reported");
        assert!(
            notes.preview,
            "a preview built later must read as an available update"
        );
    }

    #[test]
    fn an_older_real_dated_preview_is_not_reported_as_preview() {
        let stored = super::StoredReleaseNotes {
            version: "0.8.0-preview.2026-05-30-aaaaaaaaaaaa".to_string(),
            body: "### Changed\n- older dated preview".to_string(),
            show_on_startup: true,
        };
        let notes =
            super::release_notes_from_stored(stored, "0.8.0-preview.2026-06-02-abcdef123456")
                .expect("notes are still returned");
        assert!(!notes.preview, "an earlier build is not an update");
    }

    #[test]
    fn an_older_preview_build_is_not_reported_as_preview() {
        let stored = super::StoredReleaseNotes {
            version: "0.8.0-preview.122".to_string(),
            body: "### Changed\n- older preview".to_string(),
            show_on_startup: true,
        };
        let notes = super::release_notes_from_stored(stored, "0.8.0-preview.123")
            .expect("notes are still returned");
        assert!(!notes.preview, "an older build is not an update");
    }

    /// Cross-channel at one release is NOT comparable, and not-comparable is
    /// not "newer". Ordering these by channel name would have made the answer
    /// depend on the alphabet.
    #[test]
    fn a_different_channel_at_the_same_release_is_not_preview() {
        let stored = super::StoredReleaseNotes {
            version: "0.8.0-preview.999".to_string(),
            body: "### Changed\n- other channel".to_string(),
            show_on_startup: true,
        };
        let notes = super::release_notes_from_stored(stored, "0.8.0-heb.1")
            .expect("notes are still returned");
        assert!(
            !notes.preview,
            "no order across channels means no update claim"
        );
    }

    /// The regression that motivated the change: a stable release is newer
    /// than any pre-release of the same version.
    #[test]
    fn a_stable_release_outranks_the_running_prerelease() {
        let stored = super::StoredReleaseNotes {
            version: "0.8.0".to_string(),
            body: "### Changed\n- stable".to_string(),
            show_on_startup: true,
        };
        let notes =
            super::release_notes_from_stored(stored, "0.8.0-heb.1").expect("notes are returned");
        assert!(notes.preview, "0.8.0 is newer than 0.8.0-heb.1");
    }
    use super::*;

    #[test]
    fn extracts_version_section() {
        let changelog = "# Changelog\n\n## [0.2.3] - 2026-03-31\n\n### Changed\n- One\n\n## [0.2.2] - 2026-03-30\n\n### Fixed\n- Two\n";
        assert_eq!(
            extract_version_section(changelog, "0.2.3").as_deref(),
            Some("### Changed\n- One")
        );
    }

    #[test]
    fn preserves_headings() {
        assert_eq!(
            normalize_body("### Changed\n- One\n\n### Fixed\n- Two"),
            "### Changed\n- One\n\n### Fixed\n- Two"
        );
    }

    #[test]
    fn load_latest_keeps_future_version_previewable_before_restart() {
        let path = std::env::temp_dir().join(format!(
            "herdr-release-notes-{}-{}.json",
            std::process::id(),
            "preview"
        ));
        let _ = clear_pending_at(&path);
        save_pending_to_path(&path, "0.3.2", "### Changed\n- One").unwrap();

        let notes = load_latest_from_path(&path, "0.3.1").expect("latest notes");
        assert_eq!(notes.version, "0.3.2");
        assert_eq!(notes.body, "### Changed\n- One");
        assert!(notes.preview);

        clear_pending_at(&path).unwrap();
    }

    #[test]
    fn load_latest_does_not_mark_older_saved_version_as_preview() {
        let path = std::env::temp_dir().join(format!(
            "herdr-release-notes-{}-{}.json",
            std::process::id(),
            "stale"
        ));
        let _ = clear_pending_at(&path);
        save_pending_to_path(&path, "0.3.0", "### Changed\n- One").unwrap();

        let notes = load_latest_from_path(&path, "0.3.1").expect("latest notes");
        assert_eq!(notes.version, "0.3.0");
        assert_eq!(notes.body, "### Changed\n- One");
        assert!(!notes.preview);

        clear_pending_at(&path).unwrap();
    }

    #[test]
    fn marking_current_version_seen_preserves_latest_notes() {
        let path = std::env::temp_dir().join(format!(
            "herdr-release-notes-{}-{}.json",
            std::process::id(),
            "seen"
        ));
        let _ = clear_pending_at(&path);
        save_pending_to_path(&path, "0.3.1", "### Changed\n- One").unwrap();

        mark_current_version_seen_at(&path, "0.3.1").unwrap();

        let stored = load_stored_from_path(&path).expect("stored notes");
        assert!(!stored.show_on_startup);
        let latest = load_latest_from_path(&path, "0.3.1").expect("latest notes");
        assert_eq!(latest.version, "0.3.1");
        assert!(!latest.preview);

        clear_pending_at(&path).unwrap();
    }

    #[test]
    fn legacy_notes_without_show_on_startup_remain_available_as_latest() {
        let path = std::env::temp_dir().join(format!(
            "herdr-release-notes-{}-{}.json",
            std::process::id(),
            "legacy"
        ));
        let _ = clear_pending_at(&path);
        fs::write(
            &path,
            "{\n  \"version\": \"0.3.1\",\n  \"body\": \"### Changed\\n- One\"\n}",
        )
        .unwrap();

        assert!(load_latest_from_path(&path, "0.3.1").is_some());

        clear_pending_at(&path).unwrap();
    }
}
