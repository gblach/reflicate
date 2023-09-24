# Reflicate

Deduplicate data by creating reflinks between identical files.

## Install

```
$ cargo install reflicate
$ export PATH=$PATH:~/.cargo/bin
```

## Disclaimer

This is an alpha quality software.
Feel free to test this program on your system and report bugs.
But remember to make a backup first.

## Synopsis

```
reflicate [<directories...>] [-d] [-h] [-i <indexfile>] [-p] [-q]  
  
Positional Arguments:
  directories       directories to deduplicate

Options:
  -d, --dryrun      do not make any filesystem changes
  -h, --hardlinks   make hardlinks instead of reflinks
  -i, --indexfile   store computed hashes in indexfile and use them in subsequent runs
  -p, --paranoid    compute sha256 hashes in addition to blake3 hashes
                    and do not trust precomputed hashes from indexfile
  -q, --quiet       be quiet
```

## Description

**Reflicate** scans the specified directories for identical files and reflinks them together.
Files are considered identical when they have the same size and equal blake3 hash.
Reflinked files share the same disk blocks, so disk space is only occupied once.
On edit a file is copied into different blocks,
so it's safe to reflink files that currently have the same content but may differ in the future.

### Hardlinks

Hardlinks differ from reflinks in two ways:
- Hardlinks are supported by virtually all posix filesystems, while reflinks are only supported by a few, eg XFS, BTRFS, OCFS2.
- Hardlinks share the same inode, so hardlinked files are always edited together.

### Indexfile

**Reflicate** stores four values in the indexfile: file paths, file sizes, modification times, and blake3 hashes.
On subsequent runs, it computes hashes only for files that have different size or modification time.
This mean the program can run faster when indexfile is used.

Internally indexfile is combination of CDB (constant database) and msgpack.
This means that indexfile will be overwritten on subsequent runs,
so if you **reflicate** different directories, use a different indexfile.

### Paranoid mode

By default **reflicate** computes and compares blake3 hashes, but in paranoid mode sha256 hashes are used as well.
Additionally, in paranoid mode all hashes are always computed because it is possible to manipulate file modification time.

## Systemd

Systemd timer can be used to run periodically.
To do this, you need to run the following commands:

```
$ mkdir -p ~/.config/systemd/user/
$ cp systemd/* ~/.config/systemd/user/
$ systemctl --user daemon-reload
$ systemctl --user enable reflicate.timer
```

By default, the periodic task runs weekly and **reflicate** your home directory.
You can adjust this to your needs by editing the `reflicate.service` and `reflicate.timer` files.

## Showcase

At the beginning let's create an XFS file system, mount it, and create a test directory.
```
$ dd if=/dev/zero of=test.img bs=1M count=100
$ mkfs.xfs test.img
$ sudo mount -o loop test.img /mnt
$ sudo mkdir /mnt/test
$ sudo chown `id -u` /mnt/test
```

Then create two identical files and two different one.
```
$ dd if=/dev/zero of=/mnt/test/file1 bs=1M count=10
$ dd if=/dev/zero of=/mnt/test/file2 bs=1M count=10
$ dd if=/dev/zero of=/mnt/test/file3 bs=1M count=12
$ dd if=/dev/zero of=/mnt/test/file4 bs=1M count=15
```

Now we see that 53 MiB of disk space is occupied (including metadata).
```
$ df -h /mnt
Filesystem      Size  Used Avail Use% Mounted on
/dev/loop0       95M   53M   42M  56% /mnt
```

Let's **reflicate** the test directory.
```
$ reflicate /mnt/test/
/mnt/test/file2 => /mnt/test/file1 [10 MiB]
10 MiB saved
```

And we see that currently only 43 MiB of disk space is occupied.
```
$ df -h /mnt
Filesystem      Size  Used Avail Use% Mounted on
/dev/loop0       95M   43M   52M  46% /mnt
```

Let's break the reflink and create file2 with the same content as file3.
```
$ dd if=/dev/zero of=/mnt/test/file2 bs=1M count=12

$ df -h /mnt
Filesystem      Size  Used Avail Use% Mounted on
/dev/loop0       95M   55M   40M  59% /mnt
```

Then **reflicate** the test directory again.
```
$ reflicate /mnt/test/
/mnt/test/file3 => /mnt/test/file2 [12 MiB]
12 MiB saved

$ df -h /mnt
Filesystem      Size  Used Avail Use% Mounted on
/dev/loop0       95M   43M   52M  46% /mnt
```

At the end, let's remove the test filesystem.
```
$ sudo umount /mnt
$ rm test.img
```
