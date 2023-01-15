mod index;
mod utils;
use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
	let args: utils::Args = argh::from_env();

	for directory in args.directories.iter() {
		let directory = Path::new(directory);
		if ! index::scandir_checks(directory, &args) {
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
		let directory = if directory.chars().last().unwrap() != '/' {
			format!("{}/", directory)
		} else {
			directory.to_string()
		};
		let directory = Path::new(&directory);
		let mut index: index::Index = HashMap::new();
		let mut indexfile: index::IndexFile = HashMap::new();

		if let Some(cdb_r) = &cdb_r {
			indexfile = index::indexfile_get(cdb_r, directory);
		}

		if ! args.quiet {
			println!("Scanning \x1b[0;1m{}\x1b[0m directory ...",
				directory.to_string_lossy());
		}
		index::scandir(&mut index, directory, directory);
		index.retain(|_, v| v.len() > 1);

		if ! args.quiet {
			println!("Computing file hashes ...");
		}
		index::make_file_hashes(&mut index, directory, &indexfile, &args);

		if let Some(cdb_w) = &mut cdb_w {
			index::indexfile_set(cdb_w, directory, &index);
		}

		saved_bytes += index::mainloop(&mut index, directory, &args);
	}

	if let Some(cdb_w) = cdb_w {
		cdb_w.finish().unwrap();
	}

	println!("\x1b[0;1m{}\x1b[0m saved", utils::size_to_string(saved_bytes));

	ExitCode::from(0)
}
