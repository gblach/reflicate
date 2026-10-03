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
use wincode::{SchemaRead, SchemaWrite};
use xxhash_rust::xxh3;

#[derive(Debug)]
pub struct IdxRecord {
    path: PathBuf,
    size: u64,
    mtime: i128,
    ctime: i128,
    inode: (u64, u64),
    blake3: Option<[u8; 32]>,
    xxh3: Option<u128>,
}
pub type SubIndex = Vec<IdxRecord>;
pub type Index = HashMap<u64, SubIndex>;

#[derive(Serialize, Deserialize, SchemaRead, SchemaWrite, Debug)]
pub struct IdxFileRecord {
    size: u64,
    mtime: i128,
    ctime: i128,
    hash: Option<[u8; 32]>,
}
pub type IndexFile = HashMap<Vec<u8>, IdxFileRecord>;

const HEAD_SIZE: u64 = 65536;

// Nanoseconds since the epoch. ctime is included because, unlike mtime, it can't be set back by
// tools such as touch.
fn file_times(metadata: &fs::Metadata) -> (i128, i128) {
    let ns = |sec: i64, nsec: i64| sec as i128 * 1_000_000_000 + nsec as i128;
    (
        ns(metadata.mtime(), metadata.mtime_nsec()),
        ns(metadata.ctime(), metadata.ctime_nsec()),
    )
}

pub fn scandir_checks(directory: &Path, args: &utils::Args) -> bool {
    match directory.metadata() {
        Ok(metadata) => {
            if !metadata.is_dir() {
                eprintln!(
                    "File {} is not a directory.",
                    utils::bold(directory.to_string_lossy())
                );
                return false;
            }
        }
        Err(_) => {
            eprintln!(
                "Directory {} does not exist.",
                utils::bold(directory.to_string_lossy())
            );
            return false;
        }
    }

    let tmpfile0 = directory.join(utils::temp_filename(".reflicate0."));
    let tmpfile1 = directory.join(utils::temp_filename(".reflicate1."));

    if fs::File::create(&tmpfile0).is_err() {
        eprintln!(
            "Directory {} is not writable.",
            utils::bold(directory.to_string_lossy())
        );
        return false;
    }

    if !args.hardlinks {
        let result = utils::make_reflink(&tmpfile0, &tmpfile1);
        let _ = fs::remove_file(&tmpfile1);
        if result.is_err() {
            let _ = fs::remove_file(&tmpfile0);
            eprintln!(
                concat!(
                    "Underlying filesystem for {}",
                    " does not support reflinks."
                ),
                utils::bold(directory.to_string_lossy())
            );
            return false;
        }
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
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    eprintln!("Warning: failed to read directory entry: {err}");
                    continue;
                }
            };

            // The file type usually comes with the directory listing, so symlinks and special
            // files are skipped without a stat call. It doesn't follow symlinks, nor does
            // entry.metadata().
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if !file_type.is_dir() && !file_type.is_file() {
                continue;
            }
            let submetadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let path = entry.path();

            if file_type.is_dir() && metadata.dev() == submetadata.dev() {
                scandir_inner(index, basedir, &path, pb);
            } else if file_type.is_file() && submetadata.len() > 0 {
                let path = match path.strip_prefix(basedir) {
                    Ok(p) => p.to_path_buf(),
                    Err(_) => continue,
                };

                let (mtime, ctime) = file_times(&submetadata);

                let record = IdxRecord {
                    path,
                    size: submetadata.len(),
                    mtime,
                    ctime,
                    inode: (submetadata.dev(), submetadata.ino()),
                    blake3: None,
                    xxh3: None,
                };

                index.entry(record.size).or_default().push(record);
                pb.inc(1);
            }
        }
    }
}

fn open_file(path: &Path, pb: &ProgressBar) -> Option<fs::File> {
    match fs::File::open(path) {
        Ok(f) => Some(f),
        Err(ref err) if err.kind() == ErrorKind::PermissionDenied => None,
        Err(err) => {
            pb.suspend(|| eprintln!("Warning: skipping {}: {err}", path.display()));
            None
        }
    }
}

fn hash_head(path: &Path, pb: &ProgressBar) -> Option<u128> {
    let f = open_file(path, pb)?;
    let mut buffer = Vec::with_capacity(HEAD_SIZE as usize);
    if let Err(err) = f.take(HEAD_SIZE).read_to_end(&mut buffer) {
        pb.suspend(|| eprintln!("Warning: failed to read {}: {err}", path.display()));
        return None;
    }
    Some(xxh3::xxh3_128(&buffer))
}

fn hash_file(record: &mut IdxRecord, path: &Path, args: &utils::Args, pb: &ProgressBar) {
    let f = match open_file(path, pb) {
        Some(f) => f,
        None => return,
    };

    let mut reader = BufReader::with_capacity(4 << 20, f);
    let mut hasher_b3 = blake3::Hasher::new();
    let mut hasher_xx = xxh3::Xxh3::new();

    loop {
        let buffer = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(err) => {
                pb.suspend(|| eprintln!("Warning: failed to read {}: {err}", path.display()));
                return;
            }
        };
        let length = buffer.len();
        if length == 0 {
            break;
        }
        hasher_b3.update_rayon(buffer);
        if args.paranoid {
            hasher_xx.update(buffer);
        }
        reader.consume(length);
    }

    record.blake3 = Some(hasher_b3.finalize().into());
    if args.paranoid {
        record.xxh3 = Some(hasher_xx.digest128());
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

    index.par_iter_mut().for_each(|(&size, subindex)| {
        if !args.paranoid {
            for record in subindex.iter_mut() {
                let path = record.path.to_path_buf().into_os_string().into_vec();
                if let Some(filerecord) = indexfile.get(&path)
                    && record.size == filerecord.size
                    && record.mtime == filerecord.mtime
                    && record.ctime == filerecord.ctime
                {
                    record.blake3 = filerecord.hash;
                }
            }
        }

        // Hardlinked names share one inode, so only the first name of each inode is read and the
        // others get its hash. A name added since the last run has no cached hash of its own.
        let mut owners = HashMap::new();
        let first: Vec<usize> = subindex
            .iter()
            .enumerate()
            .map(|(i, r)| *owners.entry(r.inode).or_insert(i))
            .collect();
        for (i, &f) in first.iter().enumerate() {
            if subindex[f].blake3.is_none() {
                subindex[f].blake3 = subindex[i].blake3;
            }
        }

        // Files whose starts differ can't be identical, so only files whose start matches
        // another one are fully hashed. Files with cached hashes are read too, otherwise a new
        // file could not be matched with them.
        let mut needs_hash: Vec<bool> = subindex
            .iter()
            .enumerate()
            .map(|(i, r)| first[i] == i && r.blake3.is_none())
            .collect();
        if size > HEAD_SIZE && needs_hash.contains(&true) {
            let heads: Vec<Option<u128>> = subindex
                .par_iter()
                .enumerate()
                .map(|(i, r)| {
                    (first[i] == i)
                        .then(|| hash_head(&directory.join(&r.path), &pb))
                        .flatten()
                })
                .collect();
            for (i, needed) in needs_hash.iter_mut().enumerate() {
                *needed &=
                    heads[i].is_some() && heads.iter().filter(|&&h| h == heads[i]).count() > 1;
            }
        }

        subindex
            .par_iter_mut()
            .zip(needs_hash)
            .for_each(|(record, needed)| {
                if needed {
                    hash_file(record, &directory.join(&record.path), args, &pb);
                }
                pb.inc(1);
            });

        for (i, &f) in first.iter().enumerate() {
            (subindex[i].blake3, subindex[i].xxh3) = (subindex[f].blake3, subindex[f].xxh3);
        }
    });

    pb.finish();
}

fn make_links(linkindex: &mut [IdxRecord], directory: &Path, args: &utils::Args) -> (u64, bool) {
    let mut saved_bytes = 0;
    let mut failed = false;

    // Use the file that already shares data with the most others as the source. Otherwise those
    // others get relinked for nothing and counted as saved.
    let extents: Vec<_> = linkindex
        .iter()
        .map(|r| utils::first_extent(&directory.join(&r.path)))
        .collect();
    let sharing =
        |i: usize| extents[i].map_or(0, |e| extents.iter().filter(|&&o| o == Some(e)).count());
    if let Some(best) = (0..linkindex.len()).rev().max_by_key(|&i| sharing(i)) {
        linkindex.swap(0, best);
    }

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
                            "{}{} => {} [{}]",
                            directory.to_string_lossy(),
                            utils::bold(linkindex[0].path.to_string_lossy()),
                            utils::bold(linkindex[i].path.to_string_lossy()),
                            utils::size_to_string(linkindex[0].size)
                        );
                    }
                }
                Err(err) => {
                    failed = true;
                    eprintln!(
                        "Warning: failed to link {} => {}: {err}",
                        src.display(),
                        dest.display()
                    );
                }
            }
        }
    }

    // Linking changes ctime (and mtime for hardlinks), also of files sharing an inode with the
    // linked ones, so refresh the whole group to keep the index from forcing a rehash.
    if saved_bytes > 0 && !args.dry_run {
        for record in linkindex.iter_mut() {
            if let Ok(metadata) = directory.join(&record.path).metadata() {
                (record.mtime, record.ctime) = file_times(&metadata);
            }
        }
    }

    (saved_bytes, failed)
}

pub fn mainloop(index: &mut Index, directory: &Path, args: &utils::Args) -> (u64, bool) {
    let mut saved_bytes: u64 = 0;
    let mut failed = false;

    for subindex in index.values_mut() {
        subindex.sort_unstable_by_key(|r| (r.blake3, r.xxh3));

        for group in subindex
            .chunk_by_mut(|a, b| a.blake3.is_some() && a.blake3 == b.blake3 && a.xxh3 == b.xxh3)
        {
            if group.len() > 1 {
                let (group_saved, group_failed) = make_links(group, directory, args);
                saved_bytes += group_saved;
                failed |= group_failed;
            }
        }
    }

    (saved_bytes, failed)
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
                "Index file {} is corrupted, ignoring cached hashes.",
                utils::bold(indexfile)
            );
            None
        }
        Err(e) if e.kind() != ErrorKind::NotFound => {
            eprintln!(
                "Index file {} is corrupted, ignoring cached hashes.",
                utils::bold(indexfile)
            );
            None
        }
        Err(_) => None,
    };
    let mut cdb_w = None;

    if !args.dry_run {
        cdb_w = cdb2::CDBWriter::create(indexfile).ok();
        if cdb_w.is_none() {
            eprintln!("Index file {} is not writable.", utils::bold(indexfile));
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
            let path = record.path.to_path_buf().into_os_string().into_vec();
            let filerecord = IdxFileRecord {
                size: record.size,
                mtime: record.mtime,
                ctime: record.ctime,
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
