//! Descriptor-relative filesystem operations: no deletion resolves an absolute ancestor.
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, mkdirat, openat, renameat_with, unlinkat};
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

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

/// Files/symlinks are unlinked themselves; directories are opened without following
/// links and traversed using their pinned descriptor. Never traverse a symlink.
pub(super) fn remove(parent: &File, name: &OsStr, remaining: &mut usize) -> io::Result<()> {
    if *remaining == 0 {
        return Err(io::Error::other("deletion exceeds tree limit"));
    }
    *remaining -= 1;
    refuse_mount(&path(parent).join(name))?;
    let meta = std::fs::symlink_metadata(path(parent).join(name))?;
    if meta.is_dir() {
        let child = open_dir(parent, name)?;
        refuse_mount(&path(&child).join("."))?;
        for entry in std::fs::read_dir(path(&child))? {
            let entry = entry?;
            remove(&child, &entry.file_name(), remaining)?;
        }
        child.sync_all()?;
        unlinkat(parent, name, AtFlags::REMOVEDIR)?;
    } else {
        // unlinkat never follows the final component, including a raced-in symlink.
        unlinkat(parent, name, AtFlags::empty())?;
    }
    parent.sync_all()
}

pub(super) fn parent(root: &File, relative: &Path) -> io::Result<File> {
    let mut current = root.try_clone()?;
    for component in relative
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?
        .components()
    {
        if let std::path::Component::Normal(name) = component {
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
    use rustix::fs::{CWD, StatxAttributes, StatxFlags, statx};
    let stat = statx(
        CWD,
        path,
        AtFlags::NO_AUTOMOUNT | AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::empty(),
    )?;
    if !stat
        .stx_attributes_mask
        .contains(StatxAttributes::MOUNT_ROOT)
        || stat.stx_attributes.contains(StatxAttributes::MOUNT_ROOT)
    {
        return Err(io::Error::other(
            "mount boundary or unavailable mount-root detection; data retained",
        ));
    }
    Ok(())
}
