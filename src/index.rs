use super::utils;
use std::collections::HashMap;
use std::fs;
use std::io::{ BufRead, BufReader };
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{ Path, PathBuf };
use std::time::UNIX_EPOCH;
use serde::{Serialize, Deserialize};
use sha2::Digest;

#[derive(Debug)]
pub struct IdxRecord {
	path: PathBuf,
	size: u64,
	mtime: i64,
	blake3: Option<[u8; 32]>,
	sha2: Option<[u8; 32]>,
}
pub type SubIndex = Vec<IdxRecord>;
pub type Index = HashMap<u64, SubIndex>;

#[derive(Serialize, Deserialize, Debug)]
pub struct IdxFileRecord {
	size: u64,
	mtime: i64,
	hash: Option<[u8; 32]>,
}
pub type IndexFile = HashMap<Vec<u8>, IdxFileRecord>;

pub fn scandir_checks(directory: &Path, args: &utils::Args) -> bool {
	match directory.metadata() {
		Ok(metadata) => {
			if ! metadata.is_dir() {
				eprintln!("File \x1b[0;1m{}\x1b[0m is not a directory.",
					directory.to_string_lossy());
				return false;
			}
		},
		Err(_) => {
			eprintln!("Directory \x1b[0;1m{}\x1b[0m does not exist.",
				directory.to_string_lossy());
			return false;
		},
	}

	let tmpfile0 = directory.join(utils::temp_filename(".reflicate0."));
	let tmpfile1 = directory.join(utils::temp_filename(".reflicate1."));

	if fs::File::create(&tmpfile0).is_err() {
		eprintln!("Directory \x1b[0;1m{}\x1b[0m is not writable.",
			directory.to_string_lossy());
		return false;
	}

	if ! args.hardlinks {
		if ! utils::make_reflink(&tmpfile0, &tmpfile1) {
			fs::remove_file(&tmpfile0).unwrap();
			eprintln!(concat!("Underlying filesystem for \x1b[0;1m{}\x1b[0m",
				" does not support reflinks."), directory.to_string_lossy());
			return false;
		}
		fs::remove_file(&tmpfile1).unwrap();
	}

	fs::remove_file(&tmpfile0).unwrap();
	true
}

pub fn scandir(index: &mut Index, basedir: &Path, directory: &Path) {
	let metadata = directory.metadata().unwrap();

	if let Ok(iter) = directory.read_dir() {
		for path in iter {
			let path = path.unwrap().path();

			if ! path.is_symlink() {
				let submetadata = path.metadata().unwrap();

				if path.is_dir() && metadata.dev() == submetadata.dev() {
					scandir(index, basedir, &path);
				} else if path.is_file() && submetadata.len() > 0 {
					let path = path.strip_prefix(basedir).unwrap()
						.to_path_buf();

					let mtime = submetadata.modified().unwrap()
						.duration_since(UNIX_EPOCH).unwrap()
						.as_secs() as i64;

					let record = IdxRecord {
						path,
						size: submetadata.len(),
						mtime,
						blake3: None,
						sha2: None,
					};

					index.entry(record.size).or_insert_with(Vec::new);
					index.get_mut(&record.size).unwrap().push(record);
				}
			}
		}
	}
}

pub fn make_file_hashes(index: &mut Index,
	directory: &Path, indexfile: &IndexFile, args: &utils::Args) {

	for subindex in index.values_mut() {
		for record in subindex {
			if ! args.paranoid {
				let path = record.path.to_path_buf().into_os_string().into_vec();
				let filerecord = indexfile.get(&path);
				if let Some(filerecord) = filerecord {
					if record.size == filerecord.size
						&& record.mtime == filerecord.mtime {

						record.blake3 = filerecord.hash;
					}
				}
			}

			if record.blake3.is_none() {
				let mut path = PathBuf::from(directory);
				path.push(&record.path);

				let f = fs::File::open(path).unwrap();
				let mut reader = BufReader::with_capacity(32768, f);
				let mut hasher_b3 = blake3::Hasher::new();
				let mut hasher_s2 = sha2::Sha256::new();

				loop {
					let buffer = reader.fill_buf().unwrap();
					let length = buffer.len();
					if length == 0 {
						break;
					}
					hasher_b3.update(buffer);
					if args.paranoid {
						hasher_s2.update(buffer);
					}
					reader.consume(length);
				}

				let blake3: [u8; 32] = hasher_b3.finalize().into();
				record.blake3 = Some(blake3);

				if args.paranoid {
					let sha2: [u8; 32] = hasher_s2.finalize().into();
					record.sha2 = Some(sha2);
				}
			}
		}
	}
}

fn subindex_linkable(subindex: &mut SubIndex) -> SubIndex {
	let mut linkindex: SubIndex = Vec::new();
	linkindex.push(subindex.pop().unwrap());

	let mut i = 0;
	while i < subindex.len() {
		if linkindex[0].size == subindex[i].size
			&& linkindex[0].blake3 == subindex[i].blake3
			&& linkindex[0].sha2 == subindex[i].sha2 {

			linkindex.push(subindex.remove(i));
		} else {
			i += 1;
		}
	}

	linkindex
}

fn make_links(linkindex: &SubIndex, directory: &Path, args: &utils::Args) -> u64 {
	let mut saved_bytes = 0;

	let mut src = PathBuf::from(directory);
	src.push(&linkindex[0].path);

	for i in 1 .. linkindex.len() {
		let mut dest = PathBuf::from(directory);
		dest.push(&linkindex[i].path);

		if ! utils::already_linked(&src, &dest) {
			utils::make_link(&src, &dest, args);
			saved_bytes += linkindex[0].size;

			if ! args.quiet {
				println!("{}\x1b[0;1m{}\x1b[0m => \x1b[0;1m{}\x1b[0m [{}]",
					directory.to_string_lossy(),
					linkindex[0].path.to_string_lossy(),
					linkindex[i].path.to_string_lossy(),
					utils::size_to_string(linkindex[0].size));
			}
		}
	}

	saved_bytes
}

pub fn mainloop(index: &mut Index, directory: &Path, args: &utils::Args) -> u64 {
	let mut saved_bytes: u64 = 0;

	for subindex in index.values_mut() {
		while subindex.len() > 1 {
			let linkindex = subindex_linkable(subindex);
			if linkindex.len() > 1 {
				saved_bytes += make_links(&linkindex, directory, args);
			}
		}
	}

	saved_bytes
}

pub fn indexfile_open(indexfile: &String, args: &utils::Args)
	-> (Option<cdb::CDB>, Option<cdb::CDBWriter>) {

	let cdb_r = match cdb::CDB::open(indexfile) {
		Ok(cdb_r) => Some(cdb_r),
		Err(_) => None,
	};
	let mut cdb_w = None;

	if ! args.dryrun {
		cdb_w = match cdb::CDBWriter::create(indexfile) {
			Ok(cdb_r) => Some(cdb_r),
			Err(_) => None,
		};
		if cdb_w.is_none() {
			eprintln!("Index file \x1b[0;1m{indexfile}\x1b[0m is not writable.");
		}
	}

	(cdb_r, cdb_w)
}

pub fn indexfile_get(cdb_r: &cdb::CDB, directory: &Path) -> IndexFile {
	let directory = directory.canonicalize().unwrap().into_os_string().into_vec();
	if let Some(msgpack) = cdb_r.get(&directory) {
		let msgpack = msgpack.unwrap();
		return rmp_serde::from_slice(&msgpack).unwrap();
	}
	HashMap::new()
}

pub fn indexfile_set(cdb_w: &mut cdb::CDBWriter, directory: &Path, index: &Index) {
	let mut indexfile: IndexFile = HashMap::new();

	for subindex in index.values() {
		for record in subindex {
			let path = record.path.strip_prefix(directory).unwrap_or(&record.path)
				.to_path_buf().into_os_string().into_vec();
			let filerecord = IdxFileRecord {
				size: record.size,
				mtime: record.mtime,
				hash: record.blake3,
			};
			indexfile.insert(path, filerecord);
		}
	}

	let directory = directory.canonicalize().unwrap().into_os_string().into_vec();
	let msgpack = rmp_serde::to_vec_named(&indexfile).unwrap();
	cdb_w.add(&directory, &msgpack).unwrap();
}
