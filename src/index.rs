use super::utils;
use std::collections::HashMap;
use std::fs;
use std::io::{ BufRead, BufReader };
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{ Path, PathBuf };
use std::time::UNIX_EPOCH;
use serde::{Serialize, Deserialize};

#[derive(Debug)]
pub struct IdxRecord {
	path: PathBuf,
	size: u64,
	mtime: i64,
	hash: Option<[u8; 32]>,
	longhash: Option<[u8; 96]>,
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

pub fn scandir(index: &mut Index, directory: &Path) {
	let metadata = directory.metadata().unwrap();

	if let Ok(iter) = directory.read_dir() {
		for path in iter {
			let path = path.unwrap().path();

			if path.is_dir() && ! path.is_symlink() {

				let submetadata = path.metadata().unwrap();
				if metadata.dev() == submetadata.dev() {
					scandir(index, &path);
				}

			} else if path.is_file() {

				let submetadata = path.metadata().unwrap();
				if submetadata.len() > 0 {
					let mtime = submetadata.modified().unwrap()
						.duration_since(UNIX_EPOCH).unwrap()
						.as_secs() as i64;

					let record = IdxRecord {
						path,
						size: submetadata.len(),
						mtime,
						hash: None,
						longhash: None,
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
		for i in 0..subindex.len() {
			if ! args.paranoic {
				let path = subindex[i].path.strip_prefix(directory).unwrap()
					.to_path_buf().into_os_string().into_vec();
				let record = indexfile.get(&path);
				if let Some(record) = record {
					if subindex[i].size == record.size
						&& subindex[i].mtime == record.mtime {

						subindex[i].hash = record.hash;
					}
				}
			}

			if subindex[i].hash.is_none() {
				let f = fs::File::open(&subindex[i].path).unwrap();
				let mut reader = BufReader::with_capacity(32768, f);
				let mut hasher = blake3::Hasher::new();

				loop {
					let buffer = reader.fill_buf().unwrap();
					let length = buffer.len();
					if length == 0 {
						break;
					}
					hasher.update(buffer);
					reader.consume(length);
				}

				let hash: [u8; 32] = hasher.finalize().into();
				subindex[i].hash = Some(hash);

				if args.paranoic {
					let mut longhash = [0; 128];
					let mut longhash_reader = hasher.finalize_xof();
					longhash_reader.fill(&mut longhash);
					subindex[i].longhash =
						Some(longhash[32..].try_into().unwrap());
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
		if linkindex[0].size == subindex[i].size && linkindex[0].hash == subindex[i].hash
			&& linkindex[0].longhash == subindex[i].longhash {

			linkindex.push(subindex.remove(i));
		} else {
			i += 1;
		}
	}

	linkindex
}

fn make_links(linkindex: &SubIndex, directory: &Path, args: &utils::Args) -> u64 {
	let mut saved_bytes = 0;

	for i in 1 .. linkindex.len() {
		if ! utils::already_linked(&linkindex[0].path, &linkindex[i].path) {
			utils::make_link(&linkindex[0].path, &linkindex[i].path, args);
			saved_bytes += linkindex[0].size;

			if ! args.quiet {
				let src = linkindex[0].path.strip_prefix(directory).unwrap();
				let dest = linkindex[i].path.strip_prefix(directory).unwrap();
				println!("{}\x1b[0;1m{}\x1b[0m => \x1b[0;1m{}\x1b[0m [{}]",
					directory.to_string_lossy(),
					src.to_string_lossy(),
					dest.to_string_lossy(),
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
			eprintln!("Index file \x1b[0;1m{}\x1b[0m is not writable.", indexfile);
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
			let path = record.path.strip_prefix(directory).unwrap()
				.to_path_buf().into_os_string().into_vec();
			let filerecord = IdxFileRecord {
				size: record.size,
				mtime: record.mtime,
				hash: record.hash,
			};
			indexfile.insert(path, filerecord);
		}
	}

	let directory = directory.canonicalize().unwrap().into_os_string().into_vec();
	let msgpack = rmp_serde::to_vec_named(&indexfile).unwrap();
	cdb_w.add(&directory, &msgpack).unwrap();
}
