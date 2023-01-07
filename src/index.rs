use super::utils;
use std::collections::{ HashMap, VecDeque };
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
pub type Index = VecDeque<IdxRecord>;
pub type SubIndex = Vec<IdxRecord>;

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

	let tmpfile0 = directory.join(utils::temp_filename(".reduce0."));
	let tmpfile1 = directory.join(utils::temp_filename(".reduce1."));

	if fs::File::create(&tmpfile0).is_err() {
		eprintln!("Directory \x1b[0;1m{}\x1b[0m is not writable.",
			directory.to_string_lossy());
		return false;
	}

	if ! args.hardlinks {
		if ! utils::make_reflink(&tmpfile0, &tmpfile1) {
			fs::remove_file(&tmpfile0).unwrap();
			eprintln!("Underlying filesystem for \x1b[0;1m{}\x1b[0m {}",
				directory.to_string_lossy(), "does not support reflinks.");
			return false;
		}
		fs::remove_file(&tmpfile1).unwrap();
	}

	fs::remove_file(&tmpfile0).unwrap();
	return true;
}

pub fn scandir(index: &mut Index, directory: &Path) {
	let metadata = directory.metadata().unwrap();

	if let Ok(iter) = directory.read_dir() {
		for path in iter {
			let path = path.unwrap().path();

			if path.is_dir() {

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
						path: path,
						size: submetadata.len(),
						mtime: mtime,
						hash: None,
						longhash: None,
					};

					index_insert(index, record);
				}

			}
		}
	}
}

fn index_insert(index: &mut Index, record: IdxRecord) {
	let mut step = (index.len() + 1) / 2;
	let mut i = step;
	loop {
		if i == 0 || i == index.len() {
			break;
		}
		if index[i-1].size <= record.size && record.size <= index[i].size {
			break;
		}
		if step > 1 {
			step /= 2;
		}
		if record.size < index[i].size {
			i -= step;
		} else {
			i += step;
		}
	}
	index.insert(i, record);
}

pub fn deindex_unique_sizes(index: &mut Index) {
	if index.len() > 2 {
		for i in (1 .. index.len() - 1).rev() {
			if index[i-1].size != index[i].size && index[i].size != index[i+1].size {
				index.remove(i);
			}
		}
	}
	if index.len() > 1 && index[0].size != index[1].size {
		index.pop_front();
	}
	let l = index.len();
	if l > 1 && index[l-1].size != index[l-2].size {
		index.pop_back();
	}
	if index.len() == 1 {
		index.pop_back();
	}
}

pub fn make_file_hashes(index: &mut Index,
	directory: &Path, indexfile: &IndexFile, args: &utils::Args) {

	for i in 0 .. index.len() {
		if ! args.paranoic {
			let path = index[i].path.strip_prefix(directory).unwrap()
				.to_path_buf().into_os_string().into_vec();
			let record = indexfile.get(&path);
			if let Some(record) = record {
				if index[i].size == record.size && index[i].mtime == record.mtime {
					index[i].hash = record.hash;
				}
			}
		}

		if index[i].hash == None {
			let f = fs::File::open(&index[i].path).unwrap();
			let mut reader = BufReader::with_capacity(8192, f);
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
			index[i].hash = Some(hash);

			if args.paranoic {
				let mut longhash = [0; 128];
				let mut longhash_reader = hasher.finalize_xof();
				longhash_reader.fill(&mut longhash);
				index[i].longhash = Some(longhash[32..].try_into().unwrap());
			}
		}
	}
}

pub fn subindex_linkable(index: &mut Index) -> SubIndex {
	let mut subindex: SubIndex = Vec::new();
	subindex.push(index.pop_front().unwrap());

	let mut i = 0;
	while i < index.len() {
		if subindex[0].size == index[i].size && subindex[0].hash == index[i].hash
			&& subindex[0].longhash == index[i].longhash {

			subindex.push(index.remove(i).unwrap());
		} else {
			i += 1;
		}
	}

	return subindex;
}

pub fn make_links(subindex: &SubIndex, args: &utils::Args) -> u64 {
	let mut saved_bytes = 0;

	for i in 1 .. subindex.len() {
		if ! utils::already_linked(&subindex[0].path, &subindex[i].path) {
			utils::make_link(&subindex[0].path,
				&subindex[i].path, subindex[0].size, args);
			saved_bytes += subindex[i].size;
		}
	}

	return saved_bytes;
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

	return (cdb_r, cdb_w);
}

pub fn indexfile_get(cdb_r: &cdb::CDB, directory: &Path) -> IndexFile {
	let directory = directory.canonicalize().unwrap().into_os_string().into_vec();
	if let Some(msgpack) = cdb_r.get(&directory) {
		let msgpack = msgpack.unwrap();
		return rmp_serde::from_slice(&msgpack).unwrap();
	}
	return HashMap::new();
}

pub fn indexfile_set(cdb_w: &mut cdb::CDBWriter, directory: &Path, index: &Index) {
	let mut indexfile: IndexFile = HashMap::new();

	for idxfile in index {
		let path = idxfile.path.strip_prefix(directory).unwrap()
			.to_path_buf().into_os_string().into_vec();
		let record = IdxFileRecord {
			size: idxfile.size,
			mtime: idxfile.mtime,
			hash: idxfile.hash,
		};
		indexfile.insert(path, record);
	}

	let directory = directory.canonicalize().unwrap().into_os_string().into_vec();
	let msgpack = rmp_serde::to_vec_named(&indexfile).unwrap();
	cdb_w.add(&directory, &msgpack).unwrap();
}
