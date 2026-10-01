use argp::FromArgs;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

#[derive(FromArgs)]
/// Deduplicate data by creating reflinks between identical files.
pub struct Args {
    /// do not make any filesystem changes
    #[argp(switch, short = 'n')]
    pub dry_run: bool,

    /// clone files without letting the kernel verify their contents (faster, but a file
    /// changed after hashing is overwritten)
    #[argp(switch, short = 'f')]
    pub fast: bool,

    /// make hardlinks instead of reflinks
    #[argp(switch, short = 'h')]
    pub hardlinks: bool,

    /// store computed hashes in indexfile and use them in subsequent runs
    #[argp(option, short = 'i')]
    pub indexfile: Option<String>,

    /// compute xxhash hashes in addition to blake3 hashes
    /// and do not trust precomputed hashes from indexfile
    #[argp(switch, short = 'p')]
    pub paranoid: bool,

    /// be quiet
    #[argp(switch, short = 'q')]
    pub quiet: bool,

    /// print version and exit
    #[argp(switch, short = 'V')]
    pub version: bool,

    /// directories to deduplicate
    #[argp(positional)]
    pub directories: Vec<String>,
}

#[repr(C)]
struct FileDedupeRangeInfo {
    dest_fd: i64,
    dest_offset: u64,
    bytes_deduped: u64,
    status: i32,
    reserved: u32,
}

#[repr(C)]
struct FileDedupeRange {
    src_offset: u64,
    src_length: u64,
    dest_count: u16,
    reserved1: u16,
    reserved2: u32,
    info: [FileDedupeRangeInfo; 1],
}

pub fn is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::io::stderr().is_terminal()
}

pub fn temp_filename(prefix: &str) -> OsString {
    let chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut rand = [0u8; 8];
    let mut suffix = Vec::new();

    getrandom::fill(&mut rand).unwrap();

    for char in rand {
        let nth = (char & 0x3f) as usize;
        suffix.push(chars.chars().nth(nth).unwrap() as u8);
    }

    let mut filename = OsString::with_capacity(prefix.len() + rand.len());
    filename.push(prefix);
    filename.push(OsString::from_vec(suffix));
    filename
}

pub fn size_to_string(size: u64) -> String {
    let sfx = ["bytes", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB", "ZiB"];
    let mut s = size;
    let mut f = 0;
    let mut i = 0;

    while s >= 1024 && i < sfx.len() - 1 {
        f = s % 1024;
        s /= 1024;
        i += 1;
    }

    if i == 0 {
        format!("{} {}", s, sfx[0])
    } else {
        format!("{:.1} {}", s as f64 + f as f64 / 1024.0, sfx[i])
    }
}

pub fn first_extent(path: &Path) -> Option<(u64, u64)> {
    let dev = path.metadata().ok()?.dev();
    let physical = fiemap::fiemap(path).ok()?.next()?.ok()?.fe_physical;
    Some((dev, physical))
}

pub fn already_linked(src: &Path, dest: &Path) -> bool {
    let src_metadata = match src.metadata() {
        Ok(m) => m,
        Err(err) => {
            eprintln!("Warning: cannot stat {}: {err}", src.display());
            return true;
        }
    };
    let dest_metadata = match dest.metadata() {
        Ok(m) => m,
        Err(err) => {
            eprintln!("Warning: cannot stat {}: {err}", dest.display());
            return true;
        }
    };

    if src_metadata.dev() != dest_metadata.dev() {
        return false;
    }

    if src_metadata.ino() == dest_metadata.ino() {
        return true;
    }

    let src_physical = match fiemap::fiemap(src) {
        Ok(mut f) => match f.next() {
            Some(Ok(extent)) => extent.fe_physical,
            Some(Err(_)) => return true,
            None => return false,
        },
        Err(_) => return true,
    };

    let dest_physical = match fiemap::fiemap(dest) {
        Ok(mut f) => match f.next() {
            Some(Ok(extent)) => extent.fe_physical,
            Some(Err(_)) => return true,
            None => return false,
        },
        Err(_) => return true,
    };

    src_physical == dest_physical
}

// Unlike FICLONE, the kernel compares the bytes itself and shares extents atomically, so dest is
// never truncated and a file changed since hashing is left alone.
fn make_dedupe(src: &Path, dest: &Path) -> io::Result<()> {
    let srcfile = fs::File::open(src)?;
    let destfile = fs::OpenOptions::new().write(true).open(dest)?;
    let length = srcfile.metadata()?.len();
    let mut offset = 0;

    // Filesystems may dedupe less than requested per call (btrfs caps it at 16 MiB).
    while offset < length {
        let mut range = FileDedupeRange {
            src_offset: offset,
            src_length: length - offset,
            dest_count: 1,
            reserved1: 0,
            reserved2: 0,
            info: [FileDedupeRangeInfo {
                dest_fd: destfile.as_raw_fd() as i64,
                dest_offset: offset,
                bytes_deduped: 0,
                status: 0,
                reserved: 0,
            }],
        };
        // FIDEDUPERANGE = _IOWR(0x94, 54, struct file_dedupe_range), not exported by libc.
        let rc = unsafe { libc::ioctl(srcfile.as_raw_fd(), 0xc0189436, &mut range) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }

        let info = &range.info[0];
        // FILE_DEDUPE_RANGE_DIFFERS
        if info.status == 1 {
            return Err(io::Error::other("file contents differ"));
        }
        if info.status < 0 {
            return Err(io::Error::from_raw_os_error(-info.status));
        }
        if info.bytes_deduped == 0 {
            return Err(io::Error::other("no bytes deduplicated"));
        }
        offset += info.bytes_deduped;
    }
    Ok(())
}

pub fn make_reflink(src: &Path, dest: &Path) -> io::Result<()> {
    // The truncate and FICLONE both update dest's mtime. Restore it so backup tools, make and the
    // index file don't see the file as changed. A dest that doesn't exist yet has none to keep.
    let mtime = dest.metadata().and_then(|m| m.modified()).ok();
    let srcfile = fs::File::open(src)?;
    let destfile = fs::File::create(dest)?;
    let rc = unsafe { libc::ioctl(destfile.as_raw_fd(), libc::FICLONE, srcfile.as_raw_fd()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if let Some(mtime) = mtime {
        destfile.set_times(fs::FileTimes::new().set_modified(mtime))?;
    }
    Ok(())
}

fn make_hardlink(src: &Path, dest: &Path) -> io::Result<()> {
    let tmpfile = dest.with_file_name(temp_filename(".reflicate."));
    fs::hard_link(src, &tmpfile)?;
    if let Err(err) = fs::rename(&tmpfile, dest) {
        let _ = fs::remove_file(&tmpfile);
        return Err(err);
    }
    Ok(())
}

pub fn make_link(src: &Path, dest: &Path, args: &Args) -> io::Result<()> {
    if !args.dry_run {
        if args.hardlinks {
            make_hardlink(src, dest)?;
        } else if args.fast {
            make_reflink(src, dest)?;
        } else {
            make_dedupe(src, dest)?;
        }
    }
    Ok(())
}
