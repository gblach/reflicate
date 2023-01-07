use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
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

	/// use longer hashes and do not trust precomputed hashes from indexfile
	#[argh(switch, short='p')]
	pub paranoic: bool,

	/// be quiet
	#[argh(switch, short='q')]
	pub quiet: bool,

	/// directories to deduplicate
	#[argh(positional)]
	pub directories: Vec<String>,
}

extern {
	fn C_is_reflink(src_fd: i32, dest_fd: i32) -> i32;
	fn C_make_reflink(src_fd: i32, dest_fd: i32) -> i32;
}

pub fn temp_filename(prefix: &str) -> OsString {
	let chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
	let mut rand = [0u8; 8];

	getrandom::getrandom(&mut rand).unwrap();
	for i in 0 .. rand.len() {
		rand[i] &= 0x3f;
		rand[i] = chars.chars().nth(rand[i] as usize).unwrap() as u8;
	}

	let mut filename = OsString::with_capacity(prefix.len() + rand.len());
	filename.push(prefix);
	filename.push(OsString::from_vec(rand.to_vec()));
	return filename;
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

	if f == 0 {
		return format!("{} {}", s, sfx[i]);
	} else {
		let f = (f as f64 / 100.0).round();
		return format!("{}.{} {}", s, f, sfx[i]);
	}
}

fn print_feedback(src: &Path, dest: &Path, size: u64) {
	println!("\x1b[0;1m{}\x1b[0m => \x1b[0;1m{}\x1b[0m [{}]",
		src.to_string_lossy(), dest.to_string_lossy(), size_to_string(size));
}

pub fn already_linked(src: &Path, dest: &Path) -> bool {
	let srcfile = fs::File::open(&src).unwrap();
	let destfile = fs::File::open(&dest).unwrap();
	unsafe {
		let rc = C_is_reflink(srcfile.as_raw_fd(), destfile.as_raw_fd());
		return rc == 0 || rc == 2;
	}
}

pub fn make_reflink(src: &Path, dest: &Path) -> bool {
	let metadata = dest.metadata();
	let srcfile = fs::File::open(&src).unwrap();
	let destfile = fs::File::create(&dest).unwrap();
	unsafe {
		let rc = C_make_reflink(srcfile.as_raw_fd(), destfile.as_raw_fd());
		if rc != 0 {
			return false;
		}
	}
	if let Ok(metadata) = metadata {
		fs::set_permissions(dest, metadata.permissions()).unwrap();
	}
	return true;
}

fn make_hardlink(src: &Path, dest: &Path) {
	if dest.metadata().is_ok() {
		fs::remove_file(dest).unwrap();
	}
	fs::hard_link(src, dest).unwrap();
}

pub fn make_link(src: &Path, dest: &Path, size: u64, args: &Args) {
	if ! args.dryrun {
		if ! args.hardlinks {
			make_reflink(src, dest);
		} else {
			make_hardlink(src, dest)
		}
	}
	if ! args.quiet {
		print_feedback(src, dest, size);
	}
}
