//! Where this Mac keeps what opens each drive.
//!
//! The server lists no drives and holds no key: a drive is an id and a tree
//! it cannot read. So the list of drives is here, one entry per drive with
//! its name, the server it lives on, its id and the key of its root.
//!
//! This is a JSON file readable by its owner only (mode 0600), a stand-in
//! until there is sign-in and keys are wrapped for people. Whoever can read
//! the file can read and change every drive in it, and a drive whose entry
//! is lost cannot be opened by anyone.
//!
//! The app and `ludy` share the file, and may both be running. So the file
//! is what counts, not what a process read from it earlier: a change takes
//! a lock that every process honours, reads the file, changes what it read
//! and writes that; and a listing reads the file again when it has been
//! replaced. A key another process added is never written over.

use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::{anyhow, bail, Context, Result};
use ludy_proto::crypt::{self, NodeId, NodeKey};
use serde::{Deserialize, Serialize};

use crate::sealed_client::SealedClient;
use crate::sealed_drive::DriveKeys;

/// What a drive key starts with when it is shown to a person.
const KEY_PREFIX: &str = "ludy1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    name: String,
    /// The server the drive lives on, as a base URL without a trailing slash.
    server: String,
    drive: String,
    root: String,
    root_key: String,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    drives: Vec<Entry>,
}

/// A drive this Mac can open.
#[derive(Clone, Debug)]
pub struct SavedDrive {
    pub name: String,
    pub keys: DriveKeys,
}

pub struct Keystore {
    path: PathBuf,
    server: String,
    loaded: Mutex<Loaded>,
}

/// What the file held when it was last read, and which file that was.
struct Loaded {
    entries: Vec<Entry>,
    stamp: Option<Stamp>,
}

/// Tells one writing of the file from the next: every save is a new file
/// renamed into place, so the inode changes even within one clock tick.
type Stamp = (u64, std::time::SystemTime, u64);

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.ino(), meta.modified().ok()?, meta.len()))
}

/// The file's entries. A file that is not there is an empty keystore; one
/// that does not parse is an error, so that it is never taken for empty.
fn read(path: &Path) -> Result<Vec<Entry>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice::<File>(&bytes).with_context(|| format!("{} is not a Ludy keystore; move it away to start a new one", path.display()))?.drives),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != 2 * N || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

fn keys_of(e: &Entry) -> Option<DriveKeys> {
    Some(DriveKeys { drive: e.drive.clone(), root: NodeId(unhex(&e.root)?), root_key: NodeKey::from_bytes(unhex(&e.root_key)?) })
}

/// A name that can be a folder under the mount root and an NFS export.
pub fn valid_drive_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name != "." && name != ".." && !name.starts_with('.') && !name.contains(['/', ':', '\0'])
}

impl Keystore {
    /// Open the keystore at `path`, for the drives on `server`. A file that
    /// is not there yet is an empty keystore; one that does not parse is an
    /// error, so that it is never overwritten by an empty one.
    pub fn open(path: &Path, server: &str) -> Result<Keystore> {
        let at = stamp(path);
        let entries = read(path)?;
        Ok(Keystore { path: path.to_path_buf(), server: server.trim_end_matches('/').to_string(), loaded: Mutex::new(Loaded { entries, stamp: at }) })
    }

    /// What the file holds now. It is read again only when it has been
    /// replaced since the last look, by this process or another. A file
    /// that cannot be read at this moment leaves what was read before: a
    /// listing has nobody to tell, and the next change will say so.
    fn current(&self) -> MutexGuard<'_, Loaded> {
        let mut loaded = self.loaded.lock().unwrap();
        let at = stamp(&self.path);
        if at != loaded.stamp {
            if let Ok(entries) = read(&self.path) {
                *loaded = Loaded { entries, stamp: at };
            }
        }
        loaded
    }

    /// Change the file: under a lock every Ludy process takes, read it,
    /// let `edit` change what was read, and write that. So two processes
    /// changing it at once both keep their change, and neither writes an
    /// old list over a key the other added. The system lets go of the lock
    /// when the process ends, however it ends.
    fn change(&self, edit: impl FnOnce(&mut Vec<Entry>) -> Result<()>) -> Result<()> {
        let mut loaded = self.loaded.lock().unwrap();
        let dir = self.path.parent().ok_or_else(|| anyhow!("the keystore has no folder"))?;
        std::fs::create_dir_all(dir)?;
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(self.path.with_extension("lock")).with_context(|| format!("cannot lock {}", self.path.display()))?;
        lock.lock().with_context(|| format!("cannot lock {}", self.path.display()))?;
        let at = stamp(&self.path);
        let mut entries = read(&self.path)?;
        *loaded = Loaded { entries: entries.clone(), stamp: at };
        edit(&mut entries)?;
        self.save(&entries)?;
        *loaded = Loaded { entries, stamp: stamp(&self.path) };
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Replace the file, whole, by a rename: a reader sees the old file or
    /// the new one. Only [`Keystore::change`] calls this, with the lock held.
    fn save(&self, entries: &[Entry]) -> Result<()> {
        let tmp = self.path.with_extension("tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp).with_context(|| format!("cannot write {}", tmp.display()))?;
        file.write_all(&serde_json::to_vec_pretty(&File { drives: entries.to_vec() })?)?;
        file.sync_all()?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// The drives on this keystore's server, by name.
    pub fn list(&self) -> Vec<SavedDrive> {
        let loaded = self.current();
        let mut out: Vec<SavedDrive> = loaded.entries.iter().filter(|e| e.server == self.server).filter_map(|e| Some(SavedDrive { name: e.name.clone(), keys: keys_of(e)? })).collect();
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        out
    }

    pub fn get(&self, name: &str) -> Option<SavedDrive> {
        self.list().into_iter().find(|d| d.name == name)
    }

    /// Remember a drive under `name`. Refused if the name is taken on this
    /// server or this drive is already here under another name.
    pub fn add(&self, name: &str, keys: &DriveKeys) -> Result<()> {
        if !valid_drive_name(name) {
            bail!("{name:?} cannot be a drive's name: no slash or colon, no leading dot, at most 64 bytes");
        }
        self.change(|entries| {
            if let Some(e) = entries.iter().find(|e| e.server == self.server && (e.name == name || e.drive == keys.drive)) {
                if e.name == name {
                    bail!("there is already a drive called {name}");
                }
                bail!("that drive is already here, as {}", e.name);
            }
            entries.push(Entry { name: name.to_string(), server: self.server.clone(), drive: keys.drive.clone(), root: crypt::hex(&keys.root.0), root_key: crypt::hex(keys.root_key.expose()) });
            Ok(())
        })
    }

    /// Give a drive another name on this Mac. The name sealed in the cloud
    /// is not changed.
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        if !valid_drive_name(to) {
            bail!("{to:?} cannot be a drive's name: no slash or colon, no leading dot, at most 64 bytes");
        }
        self.change(|entries| {
            if entries.iter().any(|e| e.server == self.server && e.name == to) {
                bail!("there is already a drive called {to}");
            }
            let entry = entries.iter_mut().find(|e| e.server == self.server && e.name == from).ok_or_else(|| anyhow!("no drive called {from}"))?;
            entry.name = to.to_string();
            Ok(())
        })
    }

    /// Forget a drive's key. The drive stays on the server, and without its
    /// key nobody can read it.
    pub fn remove(&self, name: &str) -> Result<()> {
        self.change(|entries| {
            let before = entries.len();
            entries.retain(|e| !(e.server == self.server && e.name == name));
            if entries.len() == before {
                bail!("no drive called {name}");
            }
            Ok(())
        })
    }

    /// A drive's key as one line of text, to carry to another Mac. Whoever
    /// has it can read and change everything in the drive.
    pub fn export(&self, name: &str) -> Result<String> {
        let d = self.get(name).ok_or_else(|| anyhow!("no drive called {name}"))?;
        Ok(format!("{KEY_PREFIX}-{}-{}", d.keys.drive, crypt::hex(d.keys.root_key.expose())))
    }
}

/// Turn an exported key back into what opens the drive, asking `client`'s
/// server for the drive's root and checking that the key is that root's.
/// Returns the keys and the name the drive was created with.
pub async fn import(client: &SealedClient, key: &str) -> Result<(DriveKeys, String)> {
    let parts: Vec<&str> = key.trim().split('-').collect();
    let [KEY_PREFIX, drive, root_key] = parts[..] else { bail!("not a drive key: it should look like {KEY_PREFIX}-<32 hex>-<64 hex>") };
    let (Some(_), Some(root_key)) = (unhex::<16>(drive), unhex::<32>(root_key)) else { bail!("not a drive key: the id or the key is not hex of the right length") };
    let root_key = NodeKey::from_bytes(root_key);
    let head = client.head(drive).await?.accepted("drive").context("this server does not have that drive")?;
    let root = NodeId(unhex(&head.root).ok_or_else(|| anyhow!("the server named a root that is not an id"))?);
    let unhex_blob = |s: &str| -> Vec<u8> { (0..s.len() / 2).filter_map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()).collect() };
    // The root's key is stored wrapped by itself: opening it proves the key.
    let unwrapped = root_key.unwrap_child(&root, &unhex_blob(&head.key)).map_err(|_| anyhow!("that key does not open this drive"))?;
    if unwrapped.expose() != root_key.expose() {
        bail!("that key does not open this drive");
    }
    let name = root_key.open_name(&root, &unhex_blob(&head.name)).unwrap_or_default();
    Ok((DriveKeys { drive: drive.to_string(), root, root_key }, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(n: u8) -> DriveKeys {
        DriveKeys { drive: crypt::hex(&[n; 16]), root: NodeId([n + 1; 16]), root_key: NodeKey::from_bytes([n + 2; 32]) }
    }

    #[test]
    fn drives_are_kept_per_server_and_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/drives.json");
        let here = Keystore::open(&path, "http://127.0.0.1:8080/").unwrap();
        here.add("Footage", &keys(1)).unwrap();
        here.add("Archive", &keys(2)).unwrap();
        assert!(here.add("Footage", &keys(3)).is_err(), "a name is taken once");
        assert!(here.add("Again", &keys(1)).is_err(), "a drive is listed once");
        assert!(here.add("a/b", &keys(4)).is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

        let elsewhere = Keystore::open(&path, "http://127.0.0.1:8787").unwrap();
        assert!(elsewhere.list().is_empty(), "another server's drives are not listed");
        elsewhere.add("Footage", &keys(5)).unwrap();

        let again = Keystore::open(&path, "http://127.0.0.1:8080").unwrap();
        let names: Vec<String> = again.list().into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["Archive", "Footage"]);
        let d = again.get("Footage").unwrap();
        assert_eq!((d.keys.drive.as_str(), d.keys.root, d.keys.root_key.expose()), (keys(1).drive.as_str(), keys(1).root, keys(1).root_key.expose()));

        again.rename("Footage", "Rushes").unwrap();
        assert!(again.rename("Rushes", "Archive").is_err());
        again.remove("Archive").unwrap();
        assert!(again.remove("Archive").is_err());
        let names: Vec<String> = Keystore::open(&path, "http://127.0.0.1:8080").unwrap().list().into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["Rushes"]);
        assert_eq!(Keystore::open(&path, "http://127.0.0.1:8787").unwrap().list().len(), 1, "the other server's drive is untouched");
    }

    /// Two `Keystore`s on one file stand in for the app and `ludy` running
    /// at the same time: each has its own copy of what it read.
    #[test]
    fn a_change_keeps_what_another_process_added() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drives.json");
        let app = Keystore::open(&path, "http://x").unwrap();
        app.add("Footage", &keys(1)).unwrap();
        let cli = Keystore::open(&path, "http://x").unwrap();
        cli.add("FromCli", &keys(2)).unwrap();

        // The app sees it without being reopened, and its own changes,
        // made from a copy read before the CLI's, keep the CLI's drive.
        let names = |k: &Keystore| k.list().into_iter().map(|d| d.name).collect::<Vec<_>>();
        assert_eq!(names(&app), ["Footage", "FromCli"]);
        app.add("FromApp", &keys(3)).unwrap();
        app.rename("Footage", "Rushes").unwrap();
        app.remove("FromApp").unwrap();
        assert_eq!(names(&cli), ["FromCli", "Rushes"]);
        assert_eq!(names(&Keystore::open(&path, "http://x").unwrap()), ["FromCli", "Rushes"]);

        // What the other took or removed is refused, not written over.
        assert!(app.add("FromCli", &keys(4)).is_err(), "the name is the CLI's");
        assert!(app.add("Again", &keys(2)).is_err(), "the drive is already here");
        cli.remove("Rushes").unwrap();
        assert!(app.rename("Rushes", "Gone").is_err());
        assert!(app.get("Rushes").is_none());
        assert_eq!(names(&app), ["FromCli"]);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn changes_from_many_at_once_are_all_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drives.json");
        let (writers, each) = (8u8, 12u8);
        let threads: Vec<_> = (0..writers)
            .map(|w| {
                let path = path.clone();
                std::thread::spawn(move || {
                    // A keystore of its own, opened before the others wrote.
                    let store = Keystore::open(&path, "http://x").unwrap();
                    for i in 0..each {
                        store.add(&format!("w{w}-{i}"), &keys(w * each + i)).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let all = Keystore::open(&path, "http://x").unwrap().list();
        assert_eq!(all.len(), usize::from(writers) * usize::from(each));
    }

    /// The same from real processes: this test starts the test binary
    /// again, several times at once, each as [`a_writer_in_its_own_process`].
    #[test]
    fn changes_from_other_processes_are_all_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drives.json");
        let mine = Keystore::open(&path, "http://x").unwrap();
        let (writers, each) = (6u8, 10u8);
        let children: Vec<_> = (0..writers)
            .map(|w| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "keystore::tests::a_writer_in_its_own_process", "--nocapture"])
                    .env("LUDY_TEST_KEYSTORE", &path)
                    .env("LUDY_TEST_WRITER", format!("{w},{each}"))
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for i in 0..each {
            mine.add(&format!("mine-{i}"), &keys(200 + i)).unwrap();
        }
        for mut child in children {
            assert!(child.wait().unwrap().success(), "a writer failed");
        }
        assert_eq!(mine.list().len(), usize::from(writers + 1) * usize::from(each));
        assert_eq!(Keystore::open(&path, "http://x").unwrap().list().len(), usize::from(writers + 1) * usize::from(each));
    }

    /// Does nothing by itself; see [`changes_from_other_processes_are_all_kept`].
    #[test]
    fn a_writer_in_its_own_process() {
        let (Ok(path), Ok(job)) = (std::env::var("LUDY_TEST_KEYSTORE"), std::env::var("LUDY_TEST_WRITER")) else { return };
        let (w, each) = job.split_once(',').unwrap();
        let (w, each): (u8, u8) = (w.parse().unwrap(), each.parse().unwrap());
        let store = Keystore::open(Path::new(&path), "http://x").unwrap();
        for i in 0..each {
            store.add(&format!("w{w}-{i}"), &keys(w * each + i)).unwrap();
        }
    }

    #[test]
    fn a_file_that_does_not_parse_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drives.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(Keystore::open(&path, "http://x").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"not json");

        // Nor when it stops parsing under a keystore that is already open:
        // the change is refused, and a listing keeps what it last read.
        let good = dir.path().join("good.json");
        let store = Keystore::open(&good, "http://x").unwrap();
        store.add("Footage", &keys(1)).unwrap();
        std::fs::write(&good, b"not json").unwrap();
        assert!(store.add("Archive", &keys(2)).is_err());
        assert!(store.remove("Footage").is_err());
        assert_eq!(std::fs::read(&good).unwrap(), b"not json");
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn an_exported_key_names_the_drive_and_its_root_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = Keystore::open(&dir.path().join("drives.json"), "http://x").unwrap();
        store.add("Footage", &keys(1)).unwrap();
        let key = store.export("Footage").unwrap();
        assert_eq!(key, format!("ludy1-{}-{}", "01".repeat(16), "03".repeat(32)));
    }
}
