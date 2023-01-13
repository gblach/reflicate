mod index;
mod utils;
use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
	let args: utils::Args = argh::from_env();

	for directory in args.directories.iter() {
		let directory = Path::new(directory);
		if ! index::scandir_checks(&directory, &args) {
			return ExitCode::from(1);
		}
	}

	let mut cdb_r: Option<cdb::CDB> = None;
	let mut cdb_w: Option<cdb::CDBWriter> = None;

	if let Some(indexfile) = &args.indexfile {
		(cdb_r, cdb_w) = index::indexfile_open(indexfile, &args);
	}

	let mut saved_bytes: u64 = 0;

	for directory in args.directories.iter() {
		let directory = Path::new(directory);
		let mut index: index::Index = HashMap::new();
		let mut indexfile: index::IndexFile = HashMap::new();

		if let Some(cdb_r) = &cdb_r {
			indexfile = index::indexfile_get(cdb_r, &directory);
		}

		index::scandir(&mut index, &directory);
		index.retain(|_, v| v.len() > 1);
		index::make_file_hashes(&mut index, &directory, &indexfile, &args);

		if let Some(cdb_w) = &mut cdb_w {
			index::indexfile_set(cdb_w, &directory, &index);
		}

		for mut subindex in index.values_mut() {
			while subindex.len() > 1 {
				let linkindex = index::subindex_linkable(&mut subindex);
				if linkindex.len() > 1 {
					saved_bytes +=
						index::make_links(&linkindex, &directory, &args);
				}
			}
		}
	}

	if let Some(cdb_w) = cdb_w {
		cdb_w.finish().unwrap();
	}

	println!("\x1b[0;1m{}\x1b[0m saved", utils::size_to_string(saved_bytes));

	return ExitCode::from(0);
}
