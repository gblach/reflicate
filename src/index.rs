use super::utils;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use wincode::{SchemaRead, SchemaWrite};
use xxhash_rust::xxh3;

#[derive(Debug)]
pub struct IdxRecord {
    path: PathBuf,
    size: u64,
    mtime: i64,
    blake3: Option<[u8; 32]>,
    xxh3: Option<u128>,
}
pub type SubIndex = Vec<IdxRecord>;
pub type Index = HashMap<u64, SubIndex>;

#[derive(Serialize, Deserialize, SchemaRead, SchemaWrite, Debug)]
pub struct IdxFileRecord {
    size: u64,
    mtime: i64,
    hash: Option<[u8; 32]>,
}
pub type IndexFile = HashMap<Vec<u8>, IdxFileRecord>;

pub fn scandir_checks(directory: &Path, args: &utils::Args) -> bool {
    match directory.metadata() {
        Ok(metadata) => {
            if !metadata.is_dir() {
                eprintln!(
                    "File \x1b[0;1m{}\x1b[0m is not a directory.",
                    directory.to_string_lossy()
                );
                return false;
            }
        }
        Err(_) => {
            eprintln!(
                "Directory \x1b[0;1m{}\x1b[0m does not exist.",
                directory.to_string_lossy()
            );
            return false;
        }
    }

    let tmpfile0 = directory.join(utils::temp_filename(".reflicate0."));
    let tmpfile1 = directory.join(utils::temp_filename(".reflicate1."));

    if fs::File::create(&tmpfile0).is_err() {
        eprintln!(
            "Directory \x1b[0;1m{}\x1b[0m is not writable.",
            directory.to_string_lossy()
        );
        return false;
    }

    if !args.hardlinks {
        if utils::make_reflink(&tmpfile0, &tmpfile1).is_err() {
            let _ = fs::remove_file(&tmpfile0);
            eprintln!(
                concat!(
                    "Underlying filesystem for \x1b[0;1m{}\x1b[0m",
                    " does not support reflinks."
                ),
                directory.to_string_lossy()
            );
            return false;
        }
        let _ = fs::remove_file(&tmpfile1);
    }

    let _ = fs::remove_file(&tmpfile0);
    true
}

pub fn scandir(index: &mut Index, basedir: &Path, directory: &Path, args: &utils::Args) {
    let pb = ProgressBar::new_spinner();
    if args.quiet || !utils::is_tty() {
        pb.set_draw_target(ProgressDrawTarget::hidden());
    }
    pb.set_style(ProgressStyle::with_template("{spinner:.white} Scanning {pos} files").unwrap());
    scandir_inner(index, basedir, directory, &pb);
    pb.finish();
}

fn scandir_inner(index: &mut Index, basedir: &Path, directory: &Path, pb: &ProgressBar) {
    let metadata = match directory.metadata() {
        Ok(m) => m,
        Err(_) => return,
    };

    if let Ok(iter) = directory.read_dir() {
        for entry in iter {
            let path = match entry {
                Ok(e) => e.path(),
                Err(err) => {
                    eprintln!("Warning: failed to read directory entry: {err}");
                    continue;
                }
            };

            if !path.is_symlink() {
                let submetadata = match path.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                if path.is_dir() && metadata.dev() == submetadata.dev() {
                    scandir_inner(index, basedir, &path, pb);
                } else if path.is_file() && submetadata.len() > 0 {
                    let path = match path.strip_prefix(basedir) {
                        Ok(p) => p.to_path_buf(),
                        Err(_) => continue,
                    };

                    let mtime = submetadata
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);

                    let record = IdxRecord {
                        path,
                        size: submetadata.len(),
                        mtime,
                        blake3: None,
                        xxh3: None,
                    };

                    index.entry(record.size).or_default().push(record);
                    pb.inc(1);
                }
            }
        }
    }
}

pub fn make_file_hashes(
    index: &mut Index,
    directory: &Path,
    indexfile: &IndexFile,
    args: &utils::Args,
) {
    let total = index.values().map(|s| s.len()).sum::<usize>() as u64;

    let pb = ProgressBar::new(total);
    if args.quiet || !utils::is_tty() {
        pb.set_draw_target(ProgressDrawTarget::hidden());
    }
    pb.set_style(
        ProgressStyle::with_template("{pos} / {len} {wide_bar:.white/bright_black}").unwrap(),
    );

    index
        .par_iter_mut()
        .flat_map(|(_, subindex)| subindex.par_iter_mut())
        .for_each(|record| {
            if !args.paranoid {
                let path = record.path.to_path_buf().into_os_string().into_vec();
                if let Some(filerecord) = indexfile.get(&path)
                    && record.size == filerecord.size
                    && record.mtime == filerecord.mtime
                {
                    record.blake3 = filerecord.hash;
                }
            }

            if record.blake3.is_none() {
                let mut path = PathBuf::from(directory);
                path.push(&record.path);

                let f = match fs::File::open(&path) {
                    Ok(f) => f,
                    Err(ref err) if err.kind() == ErrorKind::PermissionDenied => {
                        pb.inc(1);
                        return;
                    }
                    Err(err) => {
                        pb.suspend(|| eprintln!("Warning: skipping {}: {err}", path.display()));
                        pb.inc(1);
                        return;
                    }
                };

                let mut reader = BufReader::with_capacity(32768, f);
                let mut hasher_b3 = blake3::Hasher::new();
                let mut hasher_xx = xxh3::Xxh3::new();
                let mut read_ok = true;

                loop {
                    let buffer = match reader.fill_buf() {
                        Ok(buf) => buf,
                        Err(err) => {
                            pb.suspend(|| {
                                eprintln!("Warning: failed to read {}: {err}", path.display())
                            });
                            read_ok = false;
                            break;
                        }
                    };
                    let length = buffer.len();
                    if length == 0 {
                        break;
                    }
                    hasher_b3.update(buffer);
                    if args.paranoid {
                        hasher_xx.update(buffer);
                    }
                    reader.consume(length);
                }

                if read_ok {
                    record.blake3 = Some(hasher_b3.finalize().into());

                    if args.paranoid {
                        record.xxh3 = Some(hasher_xx.digest128());
                    }
                }
            }

            pb.inc(1);
        });

    pb.finish();
}

fn make_links(linkindex: &[IdxRecord], directory: &Path, args: &utils::Args) -> u64 {
    let mut saved_bytes = 0;

    let mut src = PathBuf::from(directory);
    src.push(&linkindex[0].path);

    for i in 1..linkindex.len() {
        let mut dest = PathBuf::from(directory);
        dest.push(&linkindex[i].path);

        if !utils::already_linked(&src, &dest) {
            match utils::make_link(&src, &dest, args) {
                Ok(()) => {
                    saved_bytes += linkindex[0].size;

                    if !args.quiet {
                        println!(
                            "{}\x1b[0;1m{}\x1b[0m => \x1b[0;1m{}\x1b[0m [{}]",
                            directory.to_string_lossy(),
                            linkindex[0].path.to_string_lossy(),
                            linkindex[i].path.to_string_lossy(),
                            utils::size_to_string(linkindex[0].size)
                        );
                    }
                }
                Err(err) => {
                    eprintln!(
                        "Warning: failed to link {} => {}: {err}",
                        src.display(),
                        dest.display()
                    );
                }
            }
        }
    }

    saved_bytes
}

pub fn mainloop(index: &mut Index, directory: &Path, args: &utils::Args) -> u64 {
    let mut saved_bytes: u64 = 0;

    for subindex in index.values_mut() {
        subindex.sort_unstable_by_key(|r| (r.blake3, r.xxh3));

        for group in
            subindex.chunk_by(|a, b| a.blake3.is_some() && a.blake3 == b.blake3 && a.xxh3 == b.xxh3)
        {
            if group.len() > 1 {
                saved_bytes += make_links(group, directory, args);
            }
        }
    }

    saved_bytes
}

fn cdb_validate(indexfile: &str, cdb: &cdb2::CDB) -> bool {
    let mut buf = [0u8; 4];
    let header_ok = fs::File::open(indexfile)
        .and_then(|mut f| f.read_exact(&mut buf))
        .map(|_| u32::from_le_bytes(buf) >= 2048)
        .unwrap_or(false);
    header_ok && cdb.iter().all(|r| r.is_ok())
}

pub fn indexfile_open(
    indexfile: &String,
    args: &utils::Args,
) -> (Option<cdb2::CDB>, Option<cdb2::CDBWriter>) {
    let cdb_r = match cdb2::CDB::open(indexfile) {
        Ok(cdb) if cdb_validate(indexfile, &cdb) => Some(cdb),
        Ok(_) => {
            eprintln!(
                "Index file \x1b[0;1m{indexfile}\x1b[0m is corrupted, ignoring cached hashes."
            );
            None
        }
        Err(e) if e.kind() != ErrorKind::NotFound => {
            eprintln!(
                "Index file \x1b[0;1m{indexfile}\x1b[0m is corrupted, ignoring cached hashes."
            );
            None
        }
        Err(_) => None,
    };
    let mut cdb_w = None;

    if !args.dry_run {
        cdb_w = cdb2::CDBWriter::create(indexfile).ok();
        if cdb_w.is_none() {
            eprintln!("Index file \x1b[0;1m{indexfile}\x1b[0m is not writable.");
        }
    }

    (cdb_r, cdb_w)
}

pub fn indexfile_get(cdb_r: &cdb2::CDB, directory: &Path) -> IndexFile {
    let directory = match directory.canonicalize() {
        Ok(p) => p.into_os_string().into_vec(),
        Err(err) => {
            eprintln!("Warning: cannot resolve {}: {err}", directory.display());
            return HashMap::new();
        }
    };
    if let Some(data) = cdb_r.get(&directory) {
        match data {
            Ok(bincode_data) => {
                match wincode::config::deserialize::<IndexFile, _>(
                    &bincode_data,
                    wincode::config::Configuration::default().with_varint_encoding(),
                ) {
                    Ok(decoded) => return decoded,
                    Err(_) => {
                        eprintln!("Warning: index file is corrupted, ignoring cached hashes.")
                    }
                }
            }
            Err(err) => eprintln!("Warning: failed to read from index file: {err}"),
        }
    }
    HashMap::new()
}

pub fn indexfile_set(cdb_w: &mut cdb2::CDBWriter, directory: &Path, index: &Index) {
    let mut indexfile: IndexFile = HashMap::new();

    for subindex in index.values() {
        for record in subindex {
            let path = record
                .path
                .strip_prefix(directory)
                .unwrap_or(&record.path)
                .to_path_buf()
                .into_os_string()
                .into_vec();
            let filerecord = IdxFileRecord {
                size: record.size,
                mtime: record.mtime,
                hash: record.blake3,
            };
            indexfile.insert(path, filerecord);
        }
    }

    let directory = match directory.canonicalize() {
        Ok(p) => p.into_os_string().into_vec(),
        Err(err) => {
            eprintln!(
                "Warning: cannot resolve {}: {err}, index not saved.",
                directory.display()
            );
            return;
        }
    };

    let bincode_data = match wincode::config::serialize(
        &indexfile,
        wincode::config::Configuration::default().with_varint_encoding(),
    ) {
        Ok(d) => d,
        Err(err) => {
            eprintln!("Warning: failed to serialize index: {err}");
            return;
        }
    };

    if let Err(err) = cdb_w.add(&directory, &bincode_data) {
        eprintln!("Warning: failed to update index: {err}");
    }
}
