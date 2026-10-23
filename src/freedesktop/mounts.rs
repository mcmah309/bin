use eros::{Context, Result, bail};
use std::{
    ffi::OsString,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

/// Linux mountinfo preserves bind-mount boundaries, unlike walking st_dev alone.
/// Escaped mount names are bytes; they need not be Unicode.
pub struct Mounts {
    pub roots: Vec<PathBuf>,
    pub trash_roots: Vec<PathBuf>,
}

#[cfg(target_os = "linux")]
pub fn mounted_roots() -> Result<Mounts> {
    parse_mountinfo(&std::fs::read("/proc/self/mountinfo").context("cannot read the mount table")?)
}

#[cfg(target_os = "linux")]
fn parse_mountinfo(bytes: &[u8]) -> Result<Mounts> {
    let mut roots = Vec::new();
    let mut trash_roots = Vec::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let fields: Vec<_> = line.split(|byte| *byte == b' ').collect();
        let field = *fields.get(4).context("invalid mountinfo entry")?;
        let separator = fields
            .iter()
            .position(|field| *field == b"-")
            .context("invalid mountinfo separator")?;
        let kind = *fields
            .get(separator + 1)
            .context("missing mount filesystem type")?;
        let mut decoded = Vec::new();
        let mut offset = 0;
        while offset < field.len() {
            if field[offset] == b'\\' {
                let digits = field
                    .get(offset + 1..offset + 4)
                    .context("invalid escaped mount name")?;
                if !digits.iter().all(|byte| matches!(byte, b'0'..=b'7')) {
                    bail!("invalid mount name escape");
                }
                let value = (digits[0] - b'0') as u16 * 64
                    + (digits[1] - b'0') as u16 * 8
                    + (digits[2] - b'0') as u16;
                decoded.push(u8::try_from(value).context("invalid mount name byte")?);
                offset += 4;
            } else {
                decoded.push(field[offset]);
                offset += 1;
            }
        }
        let root = PathBuf::from(OsString::from_vec(decoded));
        if !root.is_absolute() {
            bail!("mount point is not absolute");
        }
        roots.push(root.clone());
        // Kernel API filesystems cannot contain ordinary user trash. Keep them
        // in boundary detection, without probing protected virtual namespaces.
        if !matches!(
            kind,
            b"proc"
                | b"sysfs"
                | b"devtmpfs"
                | b"devpts"
                | b"securityfs"
                | b"cgroup"
                | b"cgroup2"
                | b"pstore"
                | b"bpf"
                | b"debugfs"
                | b"tracefs"
                | b"configfs"
                | b"fusectl"
                | b"mqueue"
                | b"autofs"
                | b"binfmt_misc"
                | b"hugetlbfs"
        ) {
            trash_roots.push(root);
        }
    }
    roots.sort();
    roots.dedup();
    roots.sort_by_key(|root| std::cmp::Reverse(root.components().count()));
    if roots.is_empty() {
        bail!("mount table is empty");
    }
    trash_roots.sort();
    trash_roots.dedup();
    Ok(Mounts { roots, trash_roots })
}

#[cfg(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
pub fn mounted_roots() -> Result<Mounts> {
    // getmntinfo owns its buffer. Copy names while holding the lock; bin's other
    // dependencies do not call this API. No pointers escape this function.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK
        .lock()
        .map_err(|_| eros::error!("mount table lock was poisoned"))?;
    #[cfg(target_os = "netbsd")]
    let mut buffer: *mut libc::statvfs = std::ptr::null_mut();
    #[cfg(not(target_os = "netbsd"))]
    let mut buffer: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo initializes buffer and returns the number of entries.
    let count = unsafe { libc::getmntinfo(&mut buffer, libc::MNT_WAIT) };
    if count <= 0 || buffer.is_null() {
        bail!("cannot read the mount table");
    }
    // SAFETY: the API returned count initialized entries, locked until copied.
    let entries = unsafe { std::slice::from_raw_parts(buffer, count as usize) };
    let mut roots: Vec<_> = entries
        .iter()
        .filter_map(|entry| {
            let bytes: Vec<_> = entry
                .f_mntonname
                .iter()
                .take_while(|byte| **byte != 0)
                .map(|byte| *byte as u8)
                .collect();
            if bytes.is_empty() {
                None
            } else {
                Some(PathBuf::from(OsString::from_vec(bytes)))
            }
        })
        .collect();
    roots.sort();
    roots.dedup();
    roots.sort_by_key(|root| std::cmp::Reverse(root.components().count()));
    Ok(Mounts {
        trash_roots: roots.clone(),
        roots,
    })
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
pub fn mounted_roots() -> Result<Mounts> {
    bail!("mount discovery is not implemented on this operating system")
}

pub fn containing<'a>(roots: &'a [PathBuf], path: &Path) -> Result<&'a Path> {
    Ok(roots
        .iter()
        .find(|root| path.starts_with(root))
        .map(PathBuf::as_path)
        .context("path has no known mount point")?)
}

pub fn canonical_prefix(path: &Path) -> Result<PathBuf> {
    let mut prefix = path;
    let mut remaining = Vec::new();
    loop {
        match prefix.canonicalize() {
            Ok(mut base) => {
                for name in remaining.into_iter().rev() {
                    base.push(name);
                }
                return Ok(base);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                remaining.push(
                    prefix
                        .file_name()
                        .context("cannot resolve trash location")?
                        .to_os_string(),
                );
                prefix = prefix.parent().context("trash location has no parent")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[test]
    fn mount_names_and_bind_boundaries_are_lossless() {
        let table = parse_mountinfo(b"1 0 0:1 / / rw - tmpfs tmpfs rw\n2 1 0:1 /origin /space\\040and\\134slash rw - tmpfs tmpfs rw\n3 1 0:2 / /binary-\xff rw - tmpfs tmpfs rw\n4 1 0:3 / /sys rw - sysfs sysfs rw\n").unwrap();
        let roots = table.roots;
        assert!(roots.contains(&PathBuf::from("/sys")));
        assert!(!table.trash_roots.contains(&PathBuf::from("/sys")));
        assert_eq!(
            containing(&roots, Path::new("/space and\\slash/file")).unwrap(),
            Path::new("/space and\\slash")
        );
        assert_eq!(
            containing(&roots, Path::new("/space and\\slash-other")).unwrap(),
            Path::new("/")
        );
        assert!(roots.contains(&PathBuf::from(OsString::from_vec(b"/binary-\xff".to_vec()))));
    }
}
