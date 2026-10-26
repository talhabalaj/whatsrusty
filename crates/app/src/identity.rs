//! App name and on-disk locations. Debug builds ("WhatsRusty Dev", `me.balaj.WhatsRusty.dev`)
//! keep their data apart from Release.

use std::path::{Path, PathBuf};

pub const APP_NAME: &str = if cfg!(debug_assertions) { "WhatsRusty Dev" } else { "WhatsRusty" };
pub const RELEASE_BUNDLE_ID: &str = "me.balaj.WhatsRusty";
const DIR_NAME: &str = if cfg!(debug_assertions) { "WhatsRustyDev" } else { "WhatsRusty" };
/// Folder name used before the rename, by both Debug and Release builds.
const LEGACY_DIR_NAME: &str = "WhatsApp GPUI";

fn logs_parent() -> PathBuf {
    dirs::home_dir().unwrap_or_else(std::env::temp_dir).join("Library/Logs")
}

/// `~/Library/Application Support/WhatsRusty` (Debug: `WhatsRustyDev`).
pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join(DIR_NAME)
}

/// `~/Library/Logs/WhatsRusty` (Debug: `WhatsRustyDev`).
pub fn log_dir() -> PathBuf {
    logs_parent().join(DIR_NAME)
}

/// Release only: rename the legacy `WhatsApp GPUI` data and log folders when the new ones
/// don't exist yet. A rename keeps the contents, their permissions and the linked session.
pub fn migrate_legacy_dirs() {
    if cfg!(debug_assertions) {
        return;
    }
    let parents = [dirs::data_dir(), Some(logs_parent())];
    for parent in parents.into_iter().flatten() {
        match migrate_dir(&parent, LEGACY_DIR_NAME, DIR_NAME) {
            Ok(true) => eprintln!("moved {LEGACY_DIR_NAME} to {}", parent.join(DIR_NAME).display()),
            Ok(false) => {}
            Err(e) => eprintln!("could not move {}: {e}", parent.join(LEGACY_DIR_NAME).display()),
        }
    }
}

/// Returns whether `parent/legacy` was moved to `parent/current`.
fn migrate_dir(parent: &Path, legacy: &str, current: &str) -> std::io::Result<bool> {
    let (from, to) = (parent.join(legacy), parent.join(current));
    let is_real_dir = from.symlink_metadata().is_ok_and(|m| m.is_dir());
    if !is_real_dir || to.symlink_metadata().is_ok() {
        return Ok(false);
    }
    std::fs::rename(&from, &to)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn temp_parent(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("whatsrusty-migrate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn moves_legacy_folder_with_contents_and_permissions() {
        let parent = temp_parent("move");
        let legacy = parent.join(LEGACY_DIR_NAME);
        std::fs::create_dir_all(legacy.join("media")).unwrap();
        std::fs::write(legacy.join("session.db"), b"keys").unwrap();
        std::fs::set_permissions(legacy.join("session.db"), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(migrate_dir(&parent, LEGACY_DIR_NAME, "WhatsRusty").unwrap());
        let moved = parent.join("WhatsRusty");
        assert!(!legacy.exists());
        assert_eq!(std::fs::read(moved.join("session.db")).unwrap(), b"keys");
        assert!(moved.join("media").is_dir());
        assert_eq!(mode(&moved), 0o700);
        assert_eq!(mode(&moved.join("session.db")), 0o600);
        std::fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn keeps_both_folders_when_destination_exists() {
        let parent = temp_parent("exists");
        std::fs::create_dir_all(parent.join(LEGACY_DIR_NAME)).unwrap();
        std::fs::create_dir_all(parent.join("WhatsRusty")).unwrap();
        std::fs::write(parent.join("WhatsRusty/messages.db"), b"new").unwrap();

        assert!(!migrate_dir(&parent, LEGACY_DIR_NAME, "WhatsRusty").unwrap());
        assert!(parent.join(LEGACY_DIR_NAME).is_dir());
        assert_eq!(std::fs::read(parent.join("WhatsRusty/messages.db")).unwrap(), b"new");
        std::fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn ignores_missing_legacy_folder_and_symlinks() {
        let parent = temp_parent("missing");
        assert!(!migrate_dir(&parent, LEGACY_DIR_NAME, "WhatsRusty").unwrap());
        assert!(!parent.join("WhatsRusty").exists());

        let target = parent.join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, parent.join(LEGACY_DIR_NAME)).unwrap();
        assert!(!migrate_dir(&parent, LEGACY_DIR_NAME, "WhatsRusty").unwrap());
        assert!(!parent.join("WhatsRusty").exists());
        std::fs::remove_dir_all(&parent).unwrap();
    }
}
