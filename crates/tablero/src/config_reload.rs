//! Poll both the app document and selected theme, including a failed candidate's
//! dependency. Directory/inode changes and same-mtime saves are observed on Linux.
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::config::{Config, ConfigError, resolve_theme_file};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp(Option<(u64, u64, u64, i64, i64, i64, i64)>);

fn stamp(path: &Path) -> Stamp {
    Stamp(fs::metadata(path).ok().map(|m| {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    }))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    config: Stamp,
    themes: Vec<(PathBuf, Stamp)>,
}

pub(crate) struct ConfigWatcher {
    path: PathBuf,
    active_theme: Option<PathBuf>,
    candidate_theme: Option<PathBuf>,
    scanned: Option<Stamp>,
    observed: Option<Snapshot>,
    pending: Option<(Snapshot, Instant)>,
}

impl ConfigWatcher {
    pub(crate) fn new(path: PathBuf, active: &Config) -> Self {
        let active_theme = active
            .appearance
            .theme_file
            .as_deref()
            .and_then(|value| resolve_theme_file(value, &path).ok());
        Self {
            path,
            active_theme,
            candidate_theme: None,
            scanned: None,
            observed: None,
            pending: None,
        }
        // First poll validates the on-disk state too: edits between initial
        // loading and event-loop startup must not be silently marked applied.
    }

    fn snapshot(&mut self) -> Snapshot {
        let config = stamp(&self.path);
        if self.scanned.as_ref() != Some(&config) {
            self.candidate_theme = fs::read_to_string(&self.path)
                .ok()
                .and_then(|text| text.parse::<toml::Table>().ok())
                .and_then(|raw| {
                    raw.get("appearance")?
                        .get("theme_file")?
                        .as_str()
                        .map(str::to_owned)
                })
                .and_then(|value| resolve_theme_file(&value, &self.path).ok());
            self.scanned = Some(config.clone());
        }
        let mut paths: Vec<_> = self
            .active_theme
            .iter()
            .chain(self.candidate_theme.iter())
            .cloned()
            .collect();
        paths.sort();
        paths.dedup();
        Snapshot {
            config,
            themes: paths
                .into_iter()
                .map(|path| {
                    let state = stamp(&path);
                    (path, state)
                })
                .collect(),
        }
    }

    /// Returns a fully validated candidate only after a stable 400ms interval.
    /// The caller replaces UI state only on Ok; errors are emitted once per edit.
    pub(crate) fn poll(&mut self, now: Instant) -> Option<Result<Config, ConfigError>> {
        let snapshot = self.snapshot();
        if self.observed.as_ref() == Some(&snapshot) {
            self.pending = None;
            return None;
        }
        match &self.pending {
            Some((pending, since)) if pending == &snapshot => {
                if now.duration_since(*since) < Duration::from_millis(400) {
                    return None;
                }
            }
            _ => {
                self.pending = Some((snapshot, now));
                return None;
            }
        }
        let candidate = Config::load_for_reload(&self.path);
        // Do not accept a mixed read if either file changed during validation.
        if self.snapshot() != snapshot {
            self.pending = None;
            return None;
        }
        if let Ok(config) = &candidate {
            self.active_theme = config
                .appearance
                .theme_file
                .as_deref()
                .and_then(|value| resolve_theme_file(value, &self.path).ok());
        }
        // Preserve the stamps that were actually validated. Only discard the
        // former active dependency after a successful switch.
        let mut observed = snapshot;
        observed.themes.retain(|(path, _)| {
            Some(path) == self.active_theme.as_ref() || Some(path) == self.candidate_theme.as_ref()
        });
        self.observed = Some(observed);
        self.pending = None;
        Some(candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const THEME: &str = include_str!("../tests/fixtures/swatches.toml");

    fn settled(watcher: &mut ConfigWatcher, now: &mut Instant) -> Result<Config, ConfigError> {
        *now += Duration::from_secs(1);
        assert!(watcher.poll(*now).is_none());
        assert!(watcher.poll(*now + Duration::from_millis(399)).is_none());
        watcher
            .poll(*now + Duration::from_millis(400))
            .expect("candidate after settling")
    }

    #[test]
    fn theme_only_edits_replacements_and_invalid_saves_recover() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let theme = dir.path().join("theme.toml");
        fs::write(&path, "[appearance]\ntheme_file = 'theme.toml'").unwrap();
        fs::write(&theme, THEME).unwrap();
        let mut active = Config::load_from_path(&path).unwrap();
        let mut watcher = ConfigWatcher::new(path.clone(), &active);
        let mut now = Instant::now();
        assert_eq!(settled(&mut watcher, &mut now).unwrap(), active);
        let replacement = dir.path().join("replacement");
        fs::write(&replacement, THEME.replace("#80D4FF", "#010203")).unwrap();
        fs::rename(&replacement, &theme).unwrap();
        active = settled(&mut watcher, &mut now).unwrap();
        assert_eq!(active.theme.accent.to_rgba(), (1, 2, 3, 255));
        for bad in ["", "version = 999", "incomplete ="] {
            fs::write(&theme, bad).unwrap();
            assert!(settled(&mut watcher, &mut now).is_err());
            assert_eq!(active.theme.accent.to_rgba(), (1, 2, 3, 255));
            now += Duration::from_secs(1);
            assert!(
                watcher.poll(now).is_none(),
                "no repeated error until an edit"
            );
        }
        fs::remove_file(&theme).unwrap();
        assert!(settled(&mut watcher, &mut now).is_err());
        fs::write(&theme, THEME).unwrap();
        active = settled(&mut watcher, &mut now).unwrap();
        assert_eq!(active.theme.accent.to_rgba(), (128, 212, 255, 255));
        fs::write(&path, "").unwrap();
        assert!(settled(&mut watcher, &mut now).is_err());
        fs::remove_file(&path).unwrap();
        assert!(settled(&mut watcher, &mut now).is_err());
        fs::write(&path, "height = 30").unwrap();
        active = settled(&mut watcher, &mut now).unwrap();
        assert_eq!(active.theme, Config::default().theme);
        fs::write(&theme, "no longer selected").unwrap();
        now += Duration::from_secs(1);
        assert!(watcher.poll(now).is_none());
    }

    #[test]
    fn failed_switch_recovers_when_only_new_theme_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "height = 30").unwrap();
        let active = Config::load_from_path(&path).unwrap();
        let mut watcher = ConfigWatcher::new(path.clone(), &active);
        let mut now = Instant::now();
        settled(&mut watcher, &mut now).unwrap();
        fs::write(&path, "[appearance]\ntheme_file = 'new/theme.toml'").unwrap();
        assert!(settled(&mut watcher, &mut now).is_err());
        fs::create_dir(dir.path().join("new")).unwrap();
        fs::write(dir.path().join("new/theme.toml"), THEME).unwrap();
        assert_eq!(
            settled(&mut watcher, &mut now)
                .unwrap()
                .theme
                .accent
                .to_rgba(),
            (128, 212, 255, 255)
        );
    }

    #[test]
    fn symlink_retarget_and_same_mtime_saves_are_seen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let theme = dir.path().join("theme.toml");
        let target = dir.path().join("target.toml");
        fs::write(&path, "[appearance]\ntheme_file = 'theme.toml'").unwrap();
        fs::write(&target, THEME).unwrap();
        std::os::unix::fs::symlink(&target, &theme).unwrap();
        let active = Config::load_from_path(&path).unwrap();
        let mut watcher = ConfigWatcher::new(path, &active);
        let mut now = Instant::now();
        settled(&mut watcher, &mut now).unwrap();
        let mtime = fs::metadata(&target).unwrap().modified().unwrap();
        fs::write(&target, THEME.replace("#80D4FF", "#020304")).unwrap();
        fs::File::options()
            .write(true)
            .open(&target)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(
            settled(&mut watcher, &mut now)
                .unwrap()
                .theme
                .accent
                .to_rgba(),
            (2, 3, 4, 255)
        );
        let other = dir.path().join("other.toml");
        fs::write(&other, THEME).unwrap();
        fs::remove_file(&theme).unwrap();
        std::os::unix::fs::symlink(other, theme).unwrap();
        assert_eq!(
            settled(&mut watcher, &mut now).unwrap().theme.accent,
            active.theme.accent
        );
    }
}
