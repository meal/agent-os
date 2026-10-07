//! Descriptor-relative filesystem operations: no deletion resolves an absolute ancestor.
use rustix::fs::{
    AtFlags, CWD, FileType, Mode, OFlags, RenameFlags, Stat, StatxAttributes, StatxFlags, mkdirat,
    openat, renameat_with, statat, statx, unlinkat,
};
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};

/// Deepest directory level the collector checks or removes. The remover holds one
/// descriptor per level, so this also bounds its descriptor use.
pub(super) const DEPTH_LIMIT: usize = 64;

/// A mount root inside data the collector would otherwise remove, or a kernel that cannot
/// say whether there is one. Always an integrity refusal.
#[derive(Debug)]
pub(super) struct MountBoundary;
impl std::fmt::Display for MountBoundary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("mount boundary or unavailable mount-root detection; data retained")
    }
}
impl std::error::Error for MountBoundary {}

pub(super) fn path(dir: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
}
pub(super) fn open_dir(parent: &File, name: &OsStr) -> io::Result<File> {
    Ok(openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?
    .into())
}
/// `name` in `dir` without following a final symlink; `None` when it does not exist.
pub(super) fn stat(dir: &File, name: &OsStr) -> io::Result<Option<Stat>> {
    match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => Ok(Some(st)),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub(super) fn file_type(st: &Stat) -> FileType {
    FileType::from_raw_mode(st.st_mode)
}
pub(super) fn identity_of(st: &Stat) -> (u64, u64) {
    (st.st_dev, st.st_ino)
}
pub(super) fn dir_identity(dir: &File) -> io::Result<(u64, u64)> {
    Ok(identity_of(&rustix::fs::fstat(dir)?))
}
pub(super) fn directory(parent: &File, name: &OsStr) -> io::Result<File> {
    match mkdirat(parent, name, Mode::RWXU) {
        Ok(()) => parent.sync_all()?,
        Err(rustix::io::Errno::EXIST) => {}
        Err(e) => return Err(e.into()),
    }
    open_dir(parent, name)
}
pub(super) fn move_entry(parent: &File, name: &OsStr, staging: &File) -> io::Result<()> {
    renameat_with(parent, name, staging, "data", RenameFlags::NOREPLACE)?;
    // The new durable deletion state must precede removing source directory entries.
    staging.sync_all()?;
    parent.sync_all()
}
/// Moves staged data back to where it came from; never replaces anything there.
pub(super) fn restore(staging: &File, parent: &File, name: &OsStr) -> io::Result<()> {
    renameat_with(staging, "data", parent, name, RenameFlags::NOREPLACE)?;
    parent.sync_all()?;
    staging.sync_all()
}
/// Removes an empty directory or a file `name` of `dir`; a missing one is fine.
pub(super) fn unlink_if_present(dir: &File, name: &OsStr, directory: bool) -> io::Result<()> {
    let flags = if directory {
        AtFlags::REMOVEDIR
    } else {
        AtFlags::empty()
    };
    match unlinkat(dir, name, flags) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => dir.sync_all(),
        Err(e) => Err(e.into()),
    }
}

/// Files/symlinks are unlinked themselves; directories are opened without following
/// links and traversed using their pinned descriptor. Never traverse a symlink.
pub(super) fn remove(parent: &File, name: &OsStr, remaining: &mut usize) -> io::Result<()> {
    remove_at(parent, name, remaining, 0)
}

fn remove_at(parent: &File, name: &OsStr, remaining: &mut usize, depth: usize) -> io::Result<()> {
    if *remaining == 0 || depth > DEPTH_LIMIT {
        return Err(io::Error::other("deletion exceeds the tree limits"));
    }
    *remaining -= 1;
    refuse_mount_at(parent, name)?;
    let meta = stat(parent, name)?.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    if file_type(&meta) == FileType::Directory {
        let child = open_dir(parent, name)?;
        refuse_mount_at(&child, OsStr::new("."))?;
        for entry in std::fs::read_dir(path(&child))? {
            let entry = entry?;
            remove_at(&child, &entry.file_name(), remaining, depth + 1)?;
        }
        child.sync_all()?;
        unlinkat(parent, name, AtFlags::REMOVEDIR)?;
    } else {
        // unlinkat never follows the final component, including a raced-in symlink.
        unlinkat(parent, name, AtFlags::empty())?;
    }
    parent.sync_all()
}

/// The directory holding `relative`, opened component by component from `root` without
/// following any symlink.
pub(super) fn parent(root: &File, relative: &Path) -> io::Result<File> {
    let mut current = root.try_clone()?;
    for component in relative
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?
        .components()
    {
        if let Component::Normal(name) = component {
            current = open_dir(&current, name)?;
        } else {
            return Err(io::Error::other("non-owned relative path"));
        }
    }
    Ok(current)
}

/// Bind mounts can expose foreign data without a symlink or a different device id.
/// Fail closed on kernels that cannot identify mount roots.
pub(super) fn refuse_mount(path: &Path) -> io::Result<()> {
    mount_check(statx(
        CWD,
        path,
        AtFlags::NO_AUTOMOUNT | AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::empty(),
    )?)
}
pub(super) fn refuse_mount_at(dir: &File, name: &OsStr) -> io::Result<()> {
    mount_check(statx(
        dir,
        name,
        AtFlags::NO_AUTOMOUNT | AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::empty(),
    )?)
}
fn mount_check(stat: rustix::fs::Statx) -> io::Result<()> {
    if !stat
        .stx_attributes_mask
        .contains(StatxAttributes::MOUNT_ROOT)
        || stat.stx_attributes.contains(StatxAttributes::MOUNT_ROOT)
    {
        return Err(io::Error::other(MountBoundary));
    }
    Ok(())
}
