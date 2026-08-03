//! Where plans live on disk.
//!
//! One directory per plan, holding `plan.json` today and the execution journal from M4 onward.
//! Writes are atomic: a plan file is either the old one or the new one, never a truncated mix.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::Plan;

/// A directory holding plans.
pub struct Store {
    dir: PathBuf,
}

/// Enough of a plan to list it without parsing every operation.
#[derive(Clone, Debug)]
pub struct Summary {
    pub id: String,
    pub created: i64,
    pub host: String,
    pub root: PathBuf,
    pub ops: usize,
    pub delete_bytes: u64,
    pub move_bytes: u64,
}

impl Store {
    /// The user's plan store, honouring `CCDU_STATE_DIR` when set.
    pub fn open_default() -> Store {
        Store::at(state_dir().join("plans"))
    }

    pub fn at(dir: PathBuf) -> Store {
        Store { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn dir_for(&self, id: &str) -> io::Result<PathBuf> {
        // Ids reach us from the command line; a `..` here would let a typo escape the store.
        if id.is_empty() || id.contains('/') || id.contains('\\') || id.contains("..") {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("bad plan id {id:?}")));
        }
        Ok(self.dir.join(id))
    }

    pub fn save(&self, plan: &Plan) -> io::Result<PathBuf> {
        let dir = self.dir_for(&plan.id)?;
        fs::create_dir_all(&dir)?;
        let final_path = dir.join("plan.json");

        // Write beside the target and rename over it, so a crash mid-write cannot leave a plan
        // that parses as half the operations the user reviewed.
        let tmp = dir.join("plan.json.new");
        let mut file = fs::File::create(&tmp)?;
        serde_json::to_writer_pretty(&mut file, plan)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &final_path)?;
        sync_dir(&dir);

        Ok(final_path)
    }

    pub fn load(&self, id: &str) -> io::Result<Plan> {
        read_plan(&self.dir_for(id)?.join("plan.json"))
    }

    pub fn remove(&self, id: &str) -> io::Result<()> {
        fs::remove_dir_all(self.dir_for(id)?)
    }

    /// Every readable plan, newest first. Unreadable entries are skipped rather than fatal: one
    /// corrupt plan should not hide the rest.
    pub fn list(&self) -> io::Result<Vec<Summary>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };

        let mut out = Vec::new();
        for entry in entries.flatten() {
            let Ok(plan) = read_plan(&entry.path().join("plan.json")) else { continue };
            out.push(Summary {
                id: plan.id.clone(),
                created: plan.created,
                host: plan.host.clone(),
                root: plan.root.clone(),
                ops: plan.ops.len(),
                delete_bytes: plan.delete_bytes(),
                move_bytes: plan.move_bytes(),
            });
        }
        out.sort_by(|a, b| b.created.cmp(&a.created).then(b.id.cmp(&a.id)));
        Ok(out)
    }
}

pub fn read_plan(path: &Path) -> io::Result<Plan> {
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))
}

/// Where ccdu keeps state that should survive a reboot but is not configuration.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CCDU_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/ccdu")
    } else if let Some(xdg) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        PathBuf::from(xdg).join("ccdu")
    } else {
        home.join(".local/state/ccdu")
    }
}

/// Rename is only durable once the directory entry itself is on disk.
fn sync_dir(dir: &Path) {
    if let Ok(handle) = fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{EntryKind, Ident, Op};

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("plans"));
        (dir, store)
    }

    fn sample(root: &str) -> Plan {
        let mut plan = Plan::new(PathBuf::from(root));
        plan.ops = vec![Op::Delete {
            path: PathBuf::from(root).join("junk"),
            ident: Ident { dev: 1, ino: 2, size: 3, mtime: 4, kind: EntryKind::File },
            est_bytes: 4096,
        }];
        plan
    }

    #[test]
    fn saves_and_loads_a_plan() {
        let (_d, store) = store();
        let plan = sample("/data");
        let path = store.save(&plan).unwrap();

        assert!(path.ends_with("plan.json"));
        assert_eq!(store.load(&plan.id).unwrap(), plan);
    }

    #[test]
    fn saving_twice_leaves_no_temporary_behind() {
        let (_d, store) = store();
        let mut plan = sample("/data");
        store.save(&plan).unwrap();
        plan.ops.clear();
        store.save(&plan).unwrap();

        let dir = store.dir_for(&plan.id).unwrap();
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["plan.json"]);
        assert!(store.load(&plan.id).unwrap().ops.is_empty());
    }

    #[test]
    fn lists_newest_first_and_skips_rubbish() {
        let (_d, store) = store();
        let mut old = sample("/data/old");
        old.created = 1_000;
        let mut new = sample("/data/new");
        new.created = 2_000;
        store.save(&old).unwrap();
        store.save(&new).unwrap();

        // A directory that is not a plan, and a plan that will not parse.
        fs::create_dir_all(store.dir().join("not-a-plan")).unwrap();
        let broken = store.dir().join("broken");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join("plan.json"), "{ this is not json").unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert_eq!(listed[0].root, PathBuf::from("/data/new"));
        assert_eq!(listed[1].root, PathBuf::from("/data/old"));
        assert_eq!(listed[0].delete_bytes, 4096);
    }

    #[test]
    fn listing_an_absent_store_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("never-created"));
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn plan_ids_cannot_escape_the_store() {
        let (_d, store) = store();
        for bad in ["../evil", "a/b", "..", ""] {
            assert!(store.dir_for(bad).is_err(), "accepted {bad:?}");
        }
        assert!(store.dir_for("20260803T201530-3f2a9c1e").is_ok());
    }

    #[test]
    fn remove_deletes_the_whole_plan_directory() {
        let (_d, store) = store();
        let plan = sample("/data");
        store.save(&plan).unwrap();
        store.remove(&plan.id).unwrap();

        assert!(store.load(&plan.id).is_err());
        assert!(store.list().unwrap().is_empty());
    }
}
