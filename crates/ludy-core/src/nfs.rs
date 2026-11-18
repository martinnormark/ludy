//! Adapter that serves a [`DriveFs`] over NFSv3 (via `nfsserve`), so macOS
//! can mount it from loopback with `mount_nfs`: no driver, no root.

use std::sync::Arc;

use async_trait::async_trait;
use nfsserve::nfs::*;
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};

use crate::drive::{Attr, DriveFs, FsError, ROOT_ID};

pub struct NfsDrive {
    fs: Arc<DriveFs>,
    uid: u32,
    gid: u32,
    /// Distinguishes this mount's file handles from any earlier mount's.
    generation: u64,
    read_only: bool,
}

impl NfsDrive {
    pub fn new(fs: Arc<DriveFs>, read_only: bool) -> NfsDrive {
        // SAFETY: getuid/getgid cannot fail and take no arguments.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        NfsDrive { fs, uid, gid, generation: ludy_cloud::now_ms() << 8 | (std::process::id() as u64 & 0xff), read_only }
    }

    fn fattr(&self, a: &Attr) -> fattr3 {
        let t = nfstime3 { seconds: (a.mtime_ms / 1000) as u32, nseconds: ((a.mtime_ms % 1000) * 1_000_000) as u32 };
        fattr3 {
            ftype: if a.is_dir { ftype3::NF3DIR } else { ftype3::NF3REG },
            mode: if a.is_dir { 0o755 } else { 0o644 },
            nlink: if a.is_dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            size: a.size,
            // "On disk" is what this Mac actually holds: cached or journaled bytes.
            used: a.local_bytes,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: a.id,
            atime: t,
            mtime: t,
            ctime: t,
        }
    }

    fn attr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        self.fs.getattr(id).map(|a| self.fattr(&a)).map_err(stat)
    }
}

fn stat(e: FsError) -> nfsstat3 {
    match e {
        FsError::NotFound => nfsstat3::NFS3ERR_NOENT,
        FsError::NotDir => nfsstat3::NFS3ERR_NOTDIR,
        FsError::IsDir => nfsstat3::NFS3ERR_ISDIR,
        FsError::Exists => nfsstat3::NFS3ERR_EXIST,
        FsError::NotEmpty => nfsstat3::NFS3ERR_NOTEMPTY,
        FsError::Invalid => nfsstat3::NFS3ERR_INVAL,
        FsError::Io(_) => nfsstat3::NFS3ERR_IO,
    }
}

fn name(n: &filename3) -> Result<&str, nfsstat3> {
    std::str::from_utf8(&n.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)
}

#[async_trait]
impl NFSFileSystem for NfsDrive {
    fn capabilities(&self) -> VFSCapabilities {
        if self.read_only {
            VFSCapabilities::ReadOnly
        } else {
            VFSCapabilities::ReadWrite
        }
    }

    fn root_dir(&self) -> fileid3 {
        ROOT_ID
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        self.fs.note_activity();
        self.fs.lookup(dirid, name(filename)?).map_err(stat)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        self.fs.note_activity();
        self.attr(id)
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        self.fs.note_activity();
        if self.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        if let set_size3::size(size) = setattr.size {
            self.fs.set_size(id, size).map_err(stat)?;
        }
        if let set_mtime::SET_TO_CLIENT_TIME(t) = setattr.mtime {
            self.fs.set_mtime(id, t.seconds as u64 * 1000 + t.nseconds as u64 / 1_000_000).map_err(stat)?;
        }
        self.attr(id)
    }

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        self.fs.note_activity();
        self.fs.read(id, offset, count).await.map_err(stat)
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        self.fs.note_activity();
        if self.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        self.fs.write(id, offset, data).map(|a| self.fattr(&a)).map_err(stat)
    }

    async fn create(&self, dirid: fileid3, filename: &filename3, attr: sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        self.fs.note_activity();
        if self.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let id = self.fs.create(dirid, name(filename)?, false).map_err(stat)?;
        if let set_size3::size(size) = attr.size {
            self.fs.set_size(id, size).map_err(stat)?;
        }
        Ok((id, self.attr(id)?))
    }

    async fn create_exclusive(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        self.fs.note_activity();
        if self.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        self.fs.create(dirid, name(filename)?, true).map_err(stat)
    }

    async fn mkdir(&self, dirid: fileid3, dirname: &filename3) -> Result<(fileid3, fattr3), nfsstat3> {
        self.fs.note_activity();
        if self.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let id = self.fs.mkdir(dirid, name(dirname)?).map_err(stat)?;
        Ok((id, self.attr(id)?))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        self.fs.note_activity();
        if self.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        self.fs.remove(dirid, name(filename)?).map_err(stat)
    }

    async fn rename(&self, from_dirid: fileid3, from_filename: &filename3, to_dirid: fileid3, to_filename: &filename3) -> Result<(), nfsstat3> {
        self.fs.note_activity();
        if self.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        self.fs.rename(from_dirid, name(from_filename)?, to_dirid, name(to_filename)?).map_err(stat)
    }

    async fn readdir(&self, dirid: fileid3, start_after: fileid3, max_entries: usize) -> Result<ReadDirResult, nfsstat3> {
        self.fs.note_activity();
        let all = self.fs.readdir(dirid).map_err(stat)?;
        // The cookie is the id of the last entry the client already has.
        let skip = if start_after == 0 { 0 } else { all.iter().position(|(_, a)| a.id == start_after).map(|p| p + 1).unwrap_or(0) };
        let mut entries = Vec::new();
        for (n, a) in all.iter().skip(skip).take(max_entries.max(1)) {
            entries.push(DirEntry { fileid: a.id, name: n.as_bytes().into(), attr: self.fattr(a) });
        }
        let end = skip + entries.len() >= all.len();
        Ok(ReadDirResult { entries, end })
    }

    async fn symlink(&self, _dirid: fileid3, _linkname: &filename3, _symlink: &nfspath3, _attr: &sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn fsinfo(&self, root_fileid: fileid3) -> Result<fsinfo3, nfsstat3> {
        let obj_attributes = match self.attr(root_fileid) {
            Ok(v) => post_op_attr::attributes(v),
            Err(_) => post_op_attr::Void,
        };
        Ok(fsinfo3 {
            obj_attributes,
            rtmax: 1024 * 1024,
            rtpref: 256 * 1024,
            rtmult: 4096,
            wtmax: 1024 * 1024,
            wtpref: 1024 * 1024,
            wtmult: 4096,
            dtpref: 1024 * 1024,
            maxfilesize: 1 << 50,
            time_delta: nfstime3 { seconds: 0, nseconds: 1_000_000 },
            properties: FSF_HOMOGENEOUS | FSF_CANSETTIME,
        })
    }

    fn id_to_fh(&self, id: fileid3) -> nfs_fh3 {
        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&self.generation.to_le_bytes());
        data.extend_from_slice(&id.to_le_bytes());
        nfs_fh3 { data }
    }

    fn fh_to_id(&self, fh: &nfs_fh3) -> Result<fileid3, nfsstat3> {
        if fh.data.len() != 16 {
            return Err(nfsstat3::NFS3ERR_BADHANDLE);
        }
        let generation = u64::from_le_bytes(fh.data[0..8].try_into().expect("8 bytes"));
        if generation != self.generation {
            return Err(nfsstat3::NFS3ERR_STALE);
        }
        Ok(u64::from_le_bytes(fh.data[8..16].try_into().expect("8 bytes")))
    }

    fn serverid(&self) -> cookieverf3 {
        self.generation.to_le_bytes()
    }
}
