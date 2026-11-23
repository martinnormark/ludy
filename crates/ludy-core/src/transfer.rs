//! Copying files between this Mac's disk and a drive, for the command line
//! and the app's own browser. Through a mount the Finder does this itself.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};

use crate::fs::{is_local_only, FsError, ROOT_ID};
use crate::sealed_drive::SealedDrive;

const CHUNK: usize = 1 << 20;

fn fs_err(what: &str, e: FsError) -> anyhow::Error {
    anyhow!("{what}: {e:?}")
}

/// The folder at `path` from the drive's root, made if it is not there.
/// An empty path is the root.
pub fn ensure_folder(fs: &SealedDrive, path: &str) -> Result<u64> {
    let mut dir = ROOT_ID;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        dir = match fs.lookup(dir, part) {
            Ok(id) if fs.getattr(id).map(|a| a.is_dir).unwrap_or(false) => id,
            Ok(_) => bail!("{part} is a file, not a folder"),
            Err(FsError::NotFound) => fs.mkdir(dir, part).map_err(|e| fs_err(&format!("make folder {part}"), e))?,
            Err(e) => return Err(fs_err(&format!("look up {part}"), e)),
        };
    }
    Ok(dir)
}

/// Copy one file from disk into folder `dir` as `name`, replacing a file of
/// that name. The bytes land in the drive's journal; they are in the cloud
/// once the drive has been flushed. Returns the file's size.
pub fn copy_in(fs: &SealedDrive, dir: u64, name: &str, src: &Path) -> Result<u64> {
    let mut file = std::fs::File::open(src).with_context(|| format!("cannot open {}", src.display()))?;
    let meta = file.metadata()?;
    let id = fs.create(dir, name, false).map_err(|e| fs_err(&format!("create {name}"), e))?;
    fs.set_size(id, 0).map_err(|e| fs_err(&format!("truncate {name}"), e))?;
    let mut buf = vec![0u8; CHUNK];
    let mut offset = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        fs.write(id, offset, &buf[..n]).map_err(|e| fs_err(&format!("write {name}"), e))?;
        offset += n as u64;
    }
    if let Ok(modified) = meta.modified() {
        if let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH) {
            let _ = fs.set_mtime(id, since.as_millis() as u64);
        }
    }
    Ok(offset)
}

/// Copy a file, or a folder and everything in it, into folder `dir` as
/// `name`. Returns how many files and bytes were copied. Names the Finder
/// keeps for itself (`.DS_Store` and the like) are left out, and so is
/// anything that is neither a file nor a folder.
pub fn copy_tree_in(fs: &SealedDrive, dir: u64, name: &str, src: &Path) -> Result<(u64, u64)> {
    let meta = std::fs::metadata(src).with_context(|| format!("cannot read {}", src.display()))?;
    if meta.is_file() {
        return Ok((1, copy_in(fs, dir, name, src)?));
    }
    if !meta.is_dir() {
        return Ok((0, 0));
    }
    let folder = match fs.lookup(dir, name) {
        Ok(id) => id,
        Err(FsError::NotFound) => fs.mkdir(dir, name).map_err(|e| fs_err(&format!("make folder {name}"), e))?,
        Err(e) => return Err(fs_err(&format!("look up {name}"), e)),
    };
    let mut entries: Vec<_> = std::fs::read_dir(src)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    let (mut files, mut bytes) = (0, 0);
    for entry in entries {
        let Some(child) = entry.file_name().to_str().map(str::to_string) else { continue };
        if is_local_only(&child) {
            continue;
        }
        let (f, b) = copy_tree_in(fs, folder, &child, &entry.path())?;
        files += f;
        bytes += b;
    }
    Ok((files, bytes))
}

/// Copy a file out of the drive to `dest`, reading it through the engine
/// (cache, range requests, journal) as an application would.
pub async fn copy_out(fs: &Arc<SealedDrive>, id: u64, dest: &Path) -> Result<u64> {
    use std::io::Write;
    let mut out = std::fs::File::create(dest).with_context(|| format!("cannot write {}", dest.display()))?;
    let mut offset = 0u64;
    loop {
        let (data, eof) = fs.read(id, offset, CHUNK as u32).await.map_err(|e| fs_err("read", e))?;
        out.write_all(&data)?;
        offset += data.len() as u64;
        if eof || data.is_empty() {
            break;
        }
    }
    Ok(offset)
}

/// Copy a file, or a folder and everything in it, out of the drive to
/// `dest`. Returns how many files and bytes were copied.
pub async fn copy_tree_out(fs: &Arc<SealedDrive>, id: u64, dest: &Path) -> Result<(u64, u64)> {
    let mut todo = vec![(id, dest.to_path_buf())];
    let (mut files, mut bytes) = (0, 0);
    while let Some((id, dest)) = todo.pop() {
        if !fs.getattr(id).map_err(|e| fs_err("look up", e))?.is_dir {
            bytes += copy_out(fs, id, &dest).await?;
            files += 1;
            continue;
        }
        std::fs::create_dir_all(&dest).with_context(|| format!("cannot make {}", dest.display()))?;
        for (name, attr) in fs.readdir(id).map_err(|e| fs_err("list", e))? {
            todo.push((attr.id, dest.join(name)));
        }
    }
    Ok((files, bytes))
}
