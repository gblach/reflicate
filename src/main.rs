mod index;
mod utils;
use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: utils::Args = argp::parse_args_or_exit(argp::DEFAULT);

    if args.version {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    if args.directories.is_empty() {
        if let Err(argp::EarlyExit::Help(help)) =
            <utils::Args as argp::FromArgs>::from_args(&[env!("CARGO_PKG_NAME")], &["--help"])
        {
            eprintln!("{}", help.generate(argp::DEFAULT));
        }
        return ExitCode::from(1);
    }

    for directory in args.directories.iter() {
        let directory = Path::new(directory);
        if !index::scandir_checks(directory, &args) {
            return ExitCode::from(1);
        }
    }

    let mut cdb_r: Option<cdb2::CDB> = None;
    let mut cdb_w: Option<cdb2::CDBWriter> = None;

    if let Some(indexfile) = &args.indexfile {
        (cdb_r, cdb_w) = index::indexfile_open(indexfile, &args);
    }

    let mut saved_bytes: u64 = 0;
    let mut failed = false;

    for directory in args.directories.iter() {
        let directory = if directory.ends_with('/') {
            directory.to_string()
        } else {
            format!("{directory}/")
        };
        let directory = Path::new(&directory);
        let mut index: index::Index = HashMap::new();
        let mut indexfile: index::IndexFile = HashMap::new();

        if let Some(cdb_r) = &cdb_r {
            indexfile = index::indexfile_get(cdb_r, directory);
        }

        if !args.quiet {
            println!(
                "Scanning {} directory ...",
                utils::bold(directory.to_string_lossy())
            );
        }
        index::scandir(&mut index, directory, directory, &args);
        index.retain(|_, v| v.len() > 1);

        if !args.quiet {
            println!("Computing file hashes ...");
        }
        index::make_file_hashes(&mut index, directory, &indexfile, &args);

        let (dir_saved, dir_failed) = index::mainloop(&mut index, directory, &args);
        saved_bytes += dir_saved;
        failed |= dir_failed;

        if let Some(cdb_w) = &mut cdb_w {
            index::indexfile_set(cdb_w, directory, &index);
        }
    }

    if let Some(cdb_w) = cdb_w
        && let Err(err) = cdb_w.finish()
    {
        eprintln!("Warning: failed to finalize index file: {err}");
    }

    if !args.quiet {
        let verb = if args.dry_run { "would save" } else { "saved" };
        println!("{} {verb}", utils::bold(utils::size_to_string(saved_bytes)));
    }

    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    }
}
