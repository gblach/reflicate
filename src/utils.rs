use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use argp::FromArgs;

#[derive(FromArgs)]
/// Deduplicate data by creating reflinks between identical files.
pub struct Args {
	/// do not make any filesystem changes
	#[argp(switch, short='n')]
	pub dry_run: bool,

	/// make hardlinks instead of reflinks
	#[argp(switch, short='h')]
	pub hardlinks: bool,

	/// store computed hashes in indexfile and use them in subsequent runs
	#[argp(option, short='i')]
	pub indexfile: Option<String>,

	/// compute xxhash hashes in addition to blake3 hashes
	/// and do not trust precomputed hashes from indexfile
	#[argp(switch, short='p')]
	pub paranoid: bool,

	/// be quiet
	#[argp(switch, short='q')]
	pub quiet: bool,

	/// directories to deduplicate
	#[argp(positional)]
	pub directories: Vec<String>,
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

pub fn make_reflink(src: &Path, dest: &Path) -> io::Result<()> {
	let srcfile = fs::File::open(src)?;
	let destfile = fs::File::create(dest)?;
	unsafe {
		let rc = libc::ioctl(destfile.as_raw_fd(), libc::FICLONE, srcfile.as_raw_fd());
		if rc == 0 {
			Ok(())
		} else {
			Err(io::Error::last_os_error())
		}
	}
}

fn make_hardlink(src: &Path, dest: &Path) -> io::Result<()> {
	if dest.metadata().is_ok() {
		fs::remove_file(dest)?;
	}
	fs::hard_link(src, dest)?;
	Ok(())
}

pub fn make_link(src: &Path, dest: &Path, args: &Args) -> io::Result<()> {
	if ! args.dry_run {
		if ! args.hardlinks {
			make_reflink(src, dest)?;
		} else {
			make_hardlink(src, dest)?;
		}
	}
	Ok(())
}
