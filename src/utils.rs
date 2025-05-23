use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use argh::FromArgs;

#[derive(FromArgs)]
/// Deduplicate data by creating reflinks between identical files.
pub struct Args {
	/// do not make any filesystem changes
	#[argh(switch, short='d')]
	pub dryrun: bool,

	/// make hardlinks instead of reflinks
	#[argh(switch, short='h')]
	pub hardlinks: bool,

	/// store computed hashes in indexfile and use them in subsequent runs
	#[argh(option, short='i')]
	pub indexfile: Option<String>,

	/// compute xxhash hashes in addition to blake3 hashes
	/// and do not trust precomputed hashes from indexfile
	#[argh(switch, short='p')]
	pub paranoid: bool,

	/// be quiet
	#[argh(switch, short='q')]
	pub quiet: bool,

	/// directories to deduplicate
	#[argh(positional)]
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
	let src_metadata = src.metadata().unwrap();
	let dest_metadata = dest.metadata().unwrap();

	if src_metadata.dev() != dest_metadata.dev() {
		return false;
	}

	if src_metadata.ino() == dest_metadata.ino() {
		return true;
	}

	let src_physical = match fiemap::fiemap(src) {
		Ok(mut f) => f.next().unwrap().unwrap().fe_physical,
		Err(_) => return true, // Do not reflink files on error
	};

	let dest_physical = match fiemap::fiemap(dest) {
		Ok(mut f) => f.next().unwrap().unwrap().fe_physical,
		Err(_) => return true, // Do not reflink files on error
	};

	src_physical == dest_physical
}

pub fn make_reflink(src: &Path, dest: &Path) -> bool {
	let srcfile = fs::File::open(src).unwrap();
	let destfile = fs::File::create(dest).unwrap();
	unsafe {
		let rc = libc::ioctl(destfile.as_raw_fd(), libc::FICLONE, srcfile.as_raw_fd());
		rc == 0
	}
}

fn make_hardlink(src: &Path, dest: &Path) {
	if dest.metadata().is_ok() {
		fs::remove_file(dest).unwrap();
	}
	fs::hard_link(src, dest).unwrap();
}

pub fn make_link(src: &Path, dest: &Path, args: &Args) {
	if ! args.dryrun {
		if ! args.hardlinks {
			make_reflink(src, dest);
		} else {
			make_hardlink(src, dest);
		}
	}
}
