//! Poll both the app document and selected theme, including a failed candidate's
//! dependency. Directory/inode changes and same-mtime saves are observed on Linux.
//!
//! Polling is event-driven: inotify watches on the files' directories say *when*
//! to look, and the stamp comparison below still decides *whether* anything
//! changed, so an idle bar never wakes to `stat` its config. Where a watch cannot
//! be placed the watcher asks to be polled on a timer instead.
use std::{
    collections::{BTreeSet, HashMap},
    fs, io,
    os::fd::{AsFd, OwnedFd},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use rustix::fs::inotify::{self, CreateFlags, WatchFlags};

use crate::config::{Config, ConfigError, resolve_theme_file};

/// How long a changed snapshot must hold still before it is validated.
const SETTLE: Duration = Duration::from_millis(400);
/// Poll cadence while an edit is settling.
const SETTLE_POLL: Duration = Duration::from_millis(200);
/// Poll cadence when inotify cannot cover every watched file.
const FALLBACK_POLL: Duration = Duration::from_millis(500);

/// inotify watches on the directories holding the config and theme files.
///
/// Directories rather than files: editors save by renaming a new file over the
/// old one, which a watch on the old inode would never report.
struct DirWatches {
    fd: OwnedFd,
    watched: HashMap<PathBuf, i32>,
    /// Whether every requested directory is currently watched.
    complete: bool,
}

impl DirWatches {
    fn new() -> io::Result<Self> {
        Ok(Self {
            fd: inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?,
            watched: HashMap::new(),
            complete: false,
        })
    }

    /// Watch exactly `dirs`. Re-adding is idempotent in the kernel, and doing it
    /// every time revives a watch whose directory was deleted and recreated.
    fn sync(&mut self, dirs: BTreeSet<PathBuf>) {
        let flags = WatchFlags::CLOSE_WRITE
            | WatchFlags::CREATE
            | WatchFlags::DELETE
            | WatchFlags::MOVE
            | WatchFlags::ATTRIB
            | WatchFlags::DELETE_SELF
            | WatchFlags::MOVE_SELF
            | WatchFlags::ONLYDIR;
        let mut watched = HashMap::new();
        self.complete = true;
        for dir in dirs {
            match inotify::add_watch(&self.fd, &dir, flags) {
                Ok(wd) => {
                    watched.insert(dir, wd);
                }
                Err(error) => {
                    log::debug!("cannot watch {}: {error}", dir.display());
                    self.complete = false;
                }
            }
        }
        for wd in self.watched.values() {
            if !watched.values().any(|kept| kept == wd) {
                // Already gone if the directory was removed; nothing to do then.
                let _ = inotify::remove_watch(&self.fd, *wd);
            }
        }
        self.watched = watched;
    }
}

/// Discard every queued inotify event on `fd`. The events only say "look now";
/// what changed is decided by [`ConfigWatcher::poll`].
pub(crate) fn drain_events(fd: impl AsFd) {
    let mut buf = [0_u8; 4096];
    while matches!(rustix::io::read(&fd, &mut buf), Ok(n) if n > 0) {}
}

/// The closest existing directory at or above `path`'s parent, so a file in a
/// directory that does not exist yet is noticed when that directory appears.
fn watchable_dir(path: &Path) -> PathBuf {
    let mut dir = path.parent().unwrap_or(path);
    while !dir.as_os_str().is_empty() && !dir.is_dir() {
        dir = dir.parent().unwrap_or(Path::new(""));
    }
    if dir.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        dir.to_path_buf()
    }
}

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
    /// `None` when inotify is unavailable; the watcher is then timer-polled.
    watches: Option<DirWatches>,
}

impl ConfigWatcher {
    pub(crate) fn new(path: PathBuf, active: &Config) -> Self {
        let active_theme = active
            .appearance
            .theme_file
            .as_deref()
            .and_then(|value| resolve_theme_file(value, &path).ok());
        let watches = DirWatches::new()
            .inspect_err(|error| log::warn!("inotify unavailable, polling config: {error}"))
            .ok();
        let mut watcher = Self {
            path,
            active_theme,
            candidate_theme: None,
            scanned: None,
            observed: None,
            pending: None,
            watches,
        };
        watcher.sync_watches();
        watcher
        // First poll validates the on-disk state too: edits between initial
        // loading and event-loop startup must not be silently marked applied.
    }

    /// A handle on the inotify queue for the event loop to wait on, readable
    /// whenever a watched directory changes. Drain it with [`drain_events`].
    pub(crate) fn event_fd(&self) -> Option<OwnedFd> {
        self.watches.as_ref()?.fd.try_clone().ok()
    }

    /// When [`poll`](Self::poll) next needs calling without a directory event:
    /// soon while an edit settles, periodically if some file cannot be watched,
    /// and otherwise never — the next event will ask.
    pub(crate) fn next_poll(&self) -> Option<Duration> {
        if self.pending.is_some() {
            Some(SETTLE_POLL)
        } else if self
            .watches
            .as_ref()
            .is_some_and(|watches| watches.complete)
        {
            None
        } else {
            Some(FALLBACK_POLL)
        }
    }

    /// Point the watches at wherever the config and theme files live now. Each
    /// file is watched both where it is named and where it resolves to, so a
    /// retargeted symlink and an edit of its target are both seen.
    fn sync_watches(&mut self) {
        let Some(watches) = &mut self.watches else {
            return;
        };
        let dirs = [
            Some(&self.path),
            self.active_theme.as_ref(),
            self.candidate_theme.as_ref(),
        ]
        .into_iter()
        .flatten()
        .flat_map(|path| [Some(path.clone()), fs::canonicalize(path).ok()])
        .flatten()
        .map(|path| watchable_dir(&path))
        .collect();
        watches.sync(dirs);
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
        // Before looking: a directory event may mean a missing directory now
        // exists or a symlink moved, without any watched file changing yet.
        self.sync_watches();
        let snapshot = self.snapshot();
        if self.observed.as_ref() == Some(&snapshot) {
            self.pending = None;
            return None;
        }
        match &self.pending {
            Some((pending, since)) if pending == &snapshot => {
                if now.duration_since(*since) < SETTLE {
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
            self.sync_watches();
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

    /// A real event loop waiting on the watcher's inotify queue, as the bar does.
    struct Loop {
        event_loop: calloop::EventLoop<'static, u32>,
        wakeups: u32,
    }

    impl Loop {
        fn new(watcher: &ConfigWatcher) -> Self {
            let event_loop = calloop::EventLoop::try_new().unwrap();
            let fd = watcher.event_fd().expect("inotify available in tests");
            let source =
                calloop::generic::Generic::new(fd, calloop::Interest::READ, calloop::Mode::Level);
            event_loop
                .handle()
                .insert_source(source, |_, fd, wakeups| {
                    drain_events(&**fd);
                    *wakeups += 1;
                    Ok(calloop::PostAction::Continue)
                })
                .unwrap();
            Self {
                event_loop,
                wakeups: 0,
            }
        }

        /// Whether a directory event arrives within a short wait.
        fn woke(&mut self) -> bool {
            let before = self.wakeups;
            self.event_loop
                .dispatch(Duration::from_millis(250), &mut self.wakeups)
                .unwrap();
            self.wakeups > before
        }
    }

    #[test]
    fn a_settled_watcher_needs_no_timer_and_an_idle_directory_no_wakeup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "height = 30").unwrap();
        let active = Config::load_from_path(&path).unwrap();
        let mut watcher = ConfigWatcher::new(path, &active);
        let mut events = Loop::new(&watcher);

        // Startup validates the on-disk state, asking for timer polls meanwhile.
        let mut now = Instant::now();
        assert!(watcher.poll(now).is_none());
        assert_eq!(watcher.next_poll(), Some(SETTLE_POLL));
        now += SETTLE;
        watcher.poll(now).expect("startup candidate").unwrap();
        assert_eq!(watcher.next_poll(), None);
        assert!(!events.woke(), "nothing changed on disk");
    }

    #[test]
    fn saves_renames_and_theme_edits_each_wake_the_loop_and_reload_once() {
        let dir = tempfile::tempdir().unwrap();
        let themes = dir.path().join("themes");
        fs::create_dir(&themes).unwrap();
        let path = dir.path().join("config.toml");
        let theme = themes.join("theme.toml");
        fs::write(&path, "[appearance]\ntheme_file = 'themes/theme.toml'").unwrap();
        fs::write(&theme, THEME).unwrap();
        let active = Config::load_from_path(&path).unwrap();
        let mut watcher = ConfigWatcher::new(path.clone(), &active);
        let mut events = Loop::new(&watcher);
        let mut now = Instant::now();
        settled(&mut watcher, &mut now).unwrap();

        // In-place save of the config.
        fs::write(&path, "[appearance]\ntheme_file = 'themes/theme.toml'\n").unwrap();
        assert!(events.woke(), "in-place save");
        // While the edit settles the watcher asks to be polled again soon.
        now += Duration::from_secs(1);
        assert!(watcher.poll(now).is_none());
        assert_eq!(watcher.next_poll(), Some(SETTLE_POLL));
        now += SETTLE;
        watcher.poll(now).expect("settled candidate").unwrap();
        assert_eq!(watcher.next_poll(), None);

        // Editor-style atomic save: write a sibling, rename it over the file.
        let staged = dir.path().join(".config.toml.swp");
        fs::write(
            &staged,
            "[appearance]\ntheme_file = 'themes/theme.toml'\n\n",
        )
        .unwrap();
        fs::rename(&staged, &path).unwrap();
        assert!(events.woke(), "rename-over save");
        settled(&mut watcher, &mut now).unwrap();

        // A theme in another directory than the config.
        fs::write(&theme, THEME.replace("#80D4FF", "#010203")).unwrap();
        assert!(events.woke(), "theme edit");
        let reloaded = settled(&mut watcher, &mut now).unwrap();
        assert_eq!(reloaded.theme.accent.to_rgba(), (1, 2, 3, 255));

        // One reload per edit: nothing further is pending or queued.
        assert!(watcher.poll(now + Duration::from_secs(5)).is_none());
        assert_eq!(watcher.next_poll(), None);
        assert!(!events.woke());
    }

    #[test]
    fn a_theme_in_a_directory_that_does_not_exist_yet_is_noticed_when_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "height = 30").unwrap();
        let active = Config::load_from_path(&path).unwrap();
        let mut watcher = ConfigWatcher::new(path.clone(), &active);
        let mut events = Loop::new(&watcher);
        let mut now = Instant::now();
        settled(&mut watcher, &mut now).unwrap();

        fs::write(&path, "[appearance]\ntheme_file = 'later/theme.toml'").unwrap();
        assert!(events.woke());
        assert!(settled(&mut watcher, &mut now).is_err(), "theme is missing");

        fs::create_dir(dir.path().join("later")).unwrap();
        assert!(events.woke(), "directory creation");
        // The poll that event asks for moves the watch into the new directory.
        now += Duration::from_secs(1);
        assert!(watcher.poll(now).is_none());
        fs::write(dir.path().join("later/theme.toml"), THEME).unwrap();
        assert!(events.woke(), "theme written into the new directory");
        assert!(settled(&mut watcher, &mut now).is_ok());
    }

    #[test]
    fn an_unwatchable_file_falls_back_to_timer_polling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "height = 30").unwrap();
        let active = Config::load_from_path(&path).unwrap();
        let mut watcher = ConfigWatcher::new(path, &active);
        watcher.watches = None;
        let mut now = Instant::now();
        settled(&mut watcher, &mut now).unwrap();
        assert_eq!(watcher.next_poll(), Some(FALLBACK_POLL));
    }

    #[test]
    fn watchable_dir_climbs_to_the_nearest_existing_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(watchable_dir(&dir.path().join("a/b/c.toml")), dir.path());
        assert_eq!(watchable_dir(Path::new("config.toml")), Path::new("."));
    }
}
