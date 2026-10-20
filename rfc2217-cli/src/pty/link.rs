//! `--link PATH`: a symlink to the pty, removed when the bridge ends.
//!
//! A link left behind by a bridge that was killed is dangerous: the kernel
//! reuses pty numbers, so before long it points at an unrelated terminal,
//! perhaps someone's shell, and a tool writing to it writes there. So:
//! - [`Link::create`] replaces only a symlink that is dangling or points at a
//!   pseudo-terminal: a stale link from an earlier bridge, whether or not its
//!   number was reused since. It refuses anything else, so it never deletes
//!   a file, a directory, or a link to anything but a pseudo-terminal.
//! - [`Link::remove`] deletes the link only while it still points at this
//!   bridge's pty: another bridge may have replaced it since.
//! - `remove` takes `&self`, is idempotent and is safe to call from any
//!   thread, so a signal-handling thread can call it (through an
//!   `Arc<Link>`) before the process exits, and `Drop` calls it otherwise.
//!   It locks a mutex and makes ordinary syscalls: fine on a normal thread,
//!   not in a signal handler.

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use rustix::fs::FileType;

/// A symlink to the pty at a path the user chose.
#[derive(Debug)]
pub(crate) struct Link {
    /// Absolute, so a later change of directory cannot redirect the removal.
    path: PathBuf,
    target: PathBuf,
    replaced: Option<PathBuf>,
    /// Whether `remove` has finished with the link (removed it or found it
    /// was no longer ours). The lock also keeps two threads from removing
    /// at once.
    done: Mutex<bool>,
}

/// What [`Link::remove`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Removal {
    /// The link pointed at our pty and is gone now.
    Removed,
    /// An earlier call already finished with it; nothing was touched.
    AlreadyDone,
    /// Nothing is at the path any more.
    Gone,
    /// Something else is at the path now (another link, or not a link); it
    /// was left alone.
    NotOurs,
}

impl Link {
    /// Creates a symlink at `path` pointing at `device` (the pty's path), with
    /// `symlink(2)`, which never replaces anything. If a symlink is in the
    /// way, it is replaced when it is dangling or points at a pseudo-terminal
    /// (a character device with the same major number as `device`): it is then
    /// unlinked and the new link created, and [`replaced`](Self::replaced)
    /// says where it pointed. Anything else fails with
    /// [`io::ErrorKind::AlreadyExists`].
    ///
    /// There is a moment between unlinking a stale link and creating the new
    /// one; if something else appears at the path then, creating fails
    /// rather than replacing it.
    pub(crate) fn create(path: &Path, device: &Path) -> io::Result<Self> {
        let path = std::path::absolute(path).map_err(context("the link path", path))?;
        let replaced = if occupied(&path, device)? {
            let old = fs::read_link(&path).map_err(context("reading the old link", &path))?;
            fs::remove_file(&path).map_err(context("removing the old link", &path))?;
            Some(old)
        } else {
            None
        };
        symlink(device, &path).map_err(context("creating the link", &path))?;
        Ok(Self {
            path,
            target: device.to_owned(),
            replaced,
            done: Mutex::new(false),
        })
    }

    /// Fails as [`create`](Self::create) would if it ran now, without
    /// changing anything: to refuse a path in the way before connecting,
    /// and create the link only once the bridge has answered.
    pub(crate) fn check(path: &Path, device: &Path) -> io::Result<()> {
        let path = std::path::absolute(path).map_err(context("the link path", path))?;
        occupied(&path, device).map(drop)
    }

    /// The link's own path (absolute).
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Where a stale link that [`create`](Self::create) replaced pointed.
    pub(crate) fn replaced(&self) -> Option<&Path> {
        self.replaced.as_deref()
    }

    /// Deletes the link if it still points at our pty. Idempotent: once a
    /// call has removed it, or found it was no longer ours, later calls
    /// touch nothing. After an error a later call tries again.
    pub(crate) fn remove(&self) -> io::Result<Removal> {
        // A thread that panicked while holding the lock left a plain bool:
        // its value is still meaningful.
        let mut done = self.done.lock().unwrap_or_else(PoisonError::into_inner);
        if *done {
            return Ok(Removal::AlreadyDone);
        }
        let removal = match fs::read_link(&self.path) {
            Ok(target) if target == self.target => {
                fs::remove_file(&self.path).map_err(context("removing the link", &self.path))?;
                Removal::Removed
            }
            Ok(_) => Removal::NotOurs,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Removal::Gone,
            // EINVAL: not a symlink any more.
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => Removal::NotOurs,
            Err(e) => return Err(context("reading the link", &self.path)(e)),
        };
        *done = true;
        Ok(removal)
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        match self.remove() {
            Ok(Removal::Removed | Removal::AlreadyDone | Removal::Gone) => {}
            Ok(Removal::NotOurs) => crate::ui::warn(format_args!(
                "left {} alone: it no longer points at {}",
                self.path.display(),
                self.target.display()
            )),
            Err(e) => crate::ui::warn(e),
        }
    }
}

/// What is at `path`: nothing (`false`), or a stale link that may be
/// replaced (`true`); anything else is an [`io::ErrorKind::AlreadyExists`].
fn occupied(path: &Path, device: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(context("checking the link path", path)(e)),
        Ok(meta) if meta.file_type().is_symlink() && is_stale(path, device) => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} already exists and is not a stale link to a pseudo-terminal; remove it or \
                 choose another path",
                path.display()
            ),
        )),
    }
}

/// Whether the symlink at `path` is safe to replace: it leads nowhere, or to
/// a pseudo-terminal like `device`.
fn is_stale(path: &Path, device: &Path) -> bool {
    match rustix::fs::stat(path) {
        Err(rustix::io::Errno::NOENT) => true,
        Err(_) => false,
        Ok(target) => {
            FileType::from_raw_mode(target.st_mode) == FileType::CharacterDevice
                && rustix::fs::stat(device).is_ok_and(|device| {
                    rustix::fs::major(device.st_rdev) == rustix::fs::major(target.st_rdev)
                })
        }
    }
}

/// Wraps an [`io::Error`] with what was being done and to which path.
fn context(what: &'static str, path: &Path) -> impl FnOnce(io::Error) -> io::Error {
    let path = path.display().to_string();
    move |e| io::Error::new(e.kind(), format!("{what} {path}: {e}"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::pty::sys::Pty;
    use crate::pty::testing::{TempDir, settings};

    fn pty() -> Pty {
        Pty::open(&settings(9600)).unwrap()
    }

    #[test]
    fn creates_and_removes() {
        let dir = TempDir::new();
        let pty = pty();
        let path = dir.path().join("ttyRFC0");
        let link = Link::create(&path, pty.path()).unwrap();
        assert_eq!(fs::read_link(&path).unwrap(), pty.path());
        assert_eq!(link.path(), path);
        assert_eq!(link.replaced(), None);
        assert_eq!(link.remove().unwrap(), Removal::Removed);
        assert!(fs::symlink_metadata(&path).is_err());
        assert_eq!(link.remove().unwrap(), Removal::AlreadyDone);
    }

    #[test]
    fn drop_removes() {
        let dir = TempDir::new();
        let pty = pty();
        let path = dir.path().join("ttyRFC0");
        drop(Link::create(&path, pty.path()).unwrap());
        assert!(fs::symlink_metadata(&path).is_err());
    }

    #[test]
    fn refuses_what_is_not_a_stale_link() {
        let dir = TempDir::new();
        let pty = pty();
        let file = dir.path().join("file");
        fs::write(&file, "keep").unwrap();
        let err = Link::create(&file, pty.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep");

        let to_file = dir.path().join("to-file");
        symlink(&file, &to_file).unwrap();
        let err = Link::create(&to_file, pty.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_link(&to_file).unwrap(), file);

        let subdir = dir.path().join("dir");
        fs::create_dir(&subdir).unwrap();
        assert!(Link::create(&subdir, pty.path()).is_err());
        assert!(subdir.is_dir());

        // Not a pseudo-terminal, though a character device.
        let to_null = dir.path().join("to-null");
        symlink("/dev/null", &to_null).unwrap();
        assert!(Link::create(&to_null, pty.path()).is_err());
        assert_eq!(fs::read_link(&to_null).unwrap(), Path::new("/dev/null"));
    }

    #[test]
    fn replaces_a_dangling_link() {
        let dir = TempDir::new();
        let pty = pty();
        let path = dir.path().join("ttyRFC0");
        let gone = dir.path().join("gone");
        symlink(&gone, &path).unwrap();
        let link = Link::create(&path, pty.path()).unwrap();
        assert_eq!(link.replaced(), Some(gone.as_path()));
        assert_eq!(fs::read_link(&path).unwrap(), pty.path());
    }

    #[test]
    fn replaces_a_link_to_another_pty() {
        // As after a bridge was killed and its pty number reused.
        let dir = TempDir::new();
        let (ours, other) = (pty(), pty());
        let path = dir.path().join("ttyRFC0");
        symlink(other.path(), &path).unwrap();
        let link = Link::create(&path, ours.path()).unwrap();
        assert_eq!(link.replaced(), Some(other.path()));
        assert_eq!(fs::read_link(&path).unwrap(), ours.path());
    }

    #[test]
    fn removes_only_its_own_link() {
        let dir = TempDir::new();
        let (ours, other) = (pty(), pty());
        let path = dir.path().join("ttyRFC0");
        let link = Link::create(&path, ours.path()).unwrap();
        // Another bridge took the path over.
        fs::remove_file(&path).unwrap();
        symlink(other.path(), &path).unwrap();
        assert_eq!(link.remove().unwrap(), Removal::NotOurs);
        assert_eq!(fs::read_link(&path).unwrap(), other.path());
        drop(link);
        assert_eq!(fs::read_link(&path).unwrap(), other.path());

        let link = Link::create(&dir.path().join("gone"), ours.path()).unwrap();
        fs::remove_file(link.path()).unwrap();
        assert_eq!(link.remove().unwrap(), Removal::Gone);

        let link = Link::create(&dir.path().join("file"), ours.path()).unwrap();
        fs::remove_file(link.path()).unwrap();
        fs::write(link.path(), "").unwrap();
        assert_eq!(link.remove().unwrap(), Removal::NotOurs);
        assert!(link.path().is_file());
    }

    #[test]
    fn removal_from_another_thread() {
        let dir = TempDir::new();
        let pty = pty();
        let link = Arc::new(Link::create(&dir.path().join("ttyRFC0"), pty.path()).unwrap());
        let cleanup = {
            let link = Arc::clone(&link);
            move || link.remove()
        };
        let removal = std::thread::spawn(cleanup).join().unwrap().unwrap();
        assert_eq!(removal, Removal::Removed);
        assert_eq!(link.remove().unwrap(), Removal::AlreadyDone);
    }

    #[test]
    fn a_relative_path_is_made_absolute() {
        let dir = TempDir::new();
        let pty = pty();
        let cwd = std::env::current_dir().unwrap();
        let relative = pathdiff(dir.path(), &cwd).join("ttyRFC0");
        let link = Link::create(&relative, pty.path()).unwrap();
        assert!(link.path().is_absolute());
        assert_eq!(fs::read_link(link.path()).unwrap(), pty.path());
    }

    /// `to` relative to `from`, through `..` as needed.
    fn pathdiff(to: &Path, from: &Path) -> PathBuf {
        let common = to
            .components()
            .zip(from.components())
            .take_while(|(a, b)| a == b)
            .count();
        let mut out = PathBuf::new();
        for _ in from.components().skip(common) {
            out.push("..");
        }
        out.extend(to.components().skip(common));
        out
    }
}
