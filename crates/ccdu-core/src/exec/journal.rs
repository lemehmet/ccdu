//! The append-only execution journal.
//!
//! Every record is written and flushed to disk *before* the syscall it describes. That ordering is
//! the whole guarantee: the journal may claim an operation that never happened, but it can never
//! omit one that did. A resumed run re-checks reality, so a claim that turns out to be premature
//! costs one redundant check; the reverse would cost a file.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const JOURNAL_FILE: &str = "journal.jsonl";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub seq: u64,
    /// Unix seconds.
    pub ts: i64,
    #[serde(flatten)]
    pub event: Event,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Event {
    RunBegin {
        plan: String,
        pid: u32,
        ops: usize,
    },
    OpBegin {
        op: usize,
    },
    OpDone {
        op: usize,
        freed: u64,
    },
    OpFailed {
        op: usize,
        error: String,
    },
    OpSkipped {
        op: usize,
        reason: String,
    },
    /// Stopped part way. `freed` is what the unfinished operation had already reclaimed, which
    /// the resumed run cannot measure because those entries are already gone.
    Paused {
        at: usize,
        freed: u64,
    },
    RunEnd {
        done: usize,
        failed: usize,
        skipped: usize,
        freed: u64,
    },
}

impl Event {
    /// The operation this record concerns, if any.
    pub fn op(&self) -> Option<usize> {
        match self {
            Event::OpBegin { op }
            | Event::OpDone { op, .. }
            | Event::OpFailed { op, .. }
            | Event::OpSkipped { op, .. } => Some(*op),
            _ => None,
        }
    }

    /// True when the operation reached a state a resumed run should not revisit.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Event::OpDone { .. } | Event::OpFailed { .. } | Event::OpSkipped { .. })
    }
}

/// An open journal, positioned at the end.
pub struct Journal {
    file: File,
    path: PathBuf,
    seq: u64,
}

impl Journal {
    /// Open (or create) the journal in `dir`, continuing the sequence already recorded there.
    pub fn open(dir: &Path) -> io::Result<Journal> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(JOURNAL_FILE);
        let seq = read(&path)?.last().map(|r| r.seq).unwrap_or(0);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Journal { file, path, seq })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append a record and flush it to stable storage. Returns its sequence number.
    ///
    /// The `sync_data` is the point of the exercise; without it the journal is a log, not a
    /// recovery mechanism.
    pub fn append(&mut self, event: Event) -> io::Result<u64> {
        self.seq += 1;
        let record = Record { seq: self.seq, ts: now_secs(), event };
        let mut line = serde_json::to_string(&record)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.sync_data()?;
        Ok(self.seq)
    }
}

/// Read a journal, tolerating a torn final line.
///
/// A crash during `write_all` can leave the last line incomplete. That line describes something
/// that had not happened yet — the write had not returned, so the syscall it precedes had not been
/// issued — so dropping it is correct. A malformed line anywhere else means real corruption and is
/// reported rather than guessed at.
pub fn read(path: &Path) -> io::Result<Vec<Record>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let lines: Vec<String> = BufReader::new(file).lines().collect::<io::Result<_>>()?;
    let mut out = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Record>(line) {
            Ok(record) => out.push(record),
            Err(e) if i + 1 == lines.len() => {
                // Torn tail: expected after a crash, and it cannot describe completed work.
                let _ = e;
                break;
            }
            Err(e) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: line {}: {e}", path.display(), i + 1),
                ))
            }
        }
    }
    Ok(out)
}

pub fn read_dir(dir: &Path) -> io::Result<Vec<Record>> {
    read(&dir.join(JOURNAL_FILE))
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Seek;

    #[test]
    fn records_round_trip_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        journal.append(Event::RunBegin { plan: "p1".into(), pid: 7, ops: 2 }).unwrap();
        journal.append(Event::OpBegin { op: 0 }).unwrap();
        journal.append(Event::OpDone { op: 0, freed: 4096 }).unwrap();

        let records = read_dir(dir.path()).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records.iter().map(|r| r.seq).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(records[2].event, Event::OpDone { op: 0, freed: 4096 });
    }

    #[test]
    fn reopening_continues_the_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        journal.append(Event::OpBegin { op: 0 }).unwrap();
        drop(journal);

        let mut journal = Journal::open(dir.path()).unwrap();
        assert_eq!(journal.append(Event::OpDone { op: 0, freed: 1 }).unwrap(), 2);
        assert_eq!(read_dir(dir.path()).unwrap().len(), 2);
    }

    #[test]
    fn a_torn_final_line_is_dropped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        journal.append(Event::OpBegin { op: 0 }).unwrap();
        journal.append(Event::OpDone { op: 0, freed: 8 }).unwrap();
        drop(journal);

        // Simulate a crash part-way through writing the next record.
        let path = dir.path().join(JOURNAL_FILE);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"seq":3,"ts":1,"ev":"op_be"#).unwrap();
        drop(file);

        let records = read(&path).unwrap();
        assert_eq!(records.len(), 2, "torn tail should be dropped");
        assert_eq!(records[1].event, Event::OpDone { op: 0, freed: 8 });

        // And the journal reopens cleanly, continuing after the last intact record.
        let mut journal = Journal::open(dir.path()).unwrap();
        assert_eq!(journal.append(Event::Paused { at: 1, freed: 0 }).unwrap(), 3);
    }

    #[test]
    fn corruption_in_the_middle_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(JOURNAL_FILE);
        std::fs::write(
            &path,
            "{\"seq\":1,\"ts\":1,\"ev\":\"op_begin\",\"op\":0}\nGARBAGE\n\
             {\"seq\":3,\"ts\":1,\"ev\":\"op_begin\",\"op\":1}\n",
        )
        .unwrap();

        let err = read(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("line 2"), "{err}");
    }

    #[test]
    fn an_absent_journal_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_dir(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn each_record_is_on_disk_before_append_returns() {
        // Not a durability proof — that needs a power cut — but it does catch the mistake of
        // buffering records in userspace, which would silently break recovery.
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        journal.append(Event::OpBegin { op: 0 }).unwrap();

        let mut probe = File::open(dir.path().join(JOURNAL_FILE)).unwrap();
        probe.seek(io::SeekFrom::Start(0)).unwrap();
        let mut text = String::new();
        io::Read::read_to_string(&mut probe, &mut text).unwrap();
        assert!(text.contains("op_begin"), "record was still buffered: {text:?}");
    }

    #[test]
    fn terminal_states_are_the_ones_resume_should_skip() {
        assert!(Event::OpDone { op: 0, freed: 0 }.is_terminal());
        assert!(Event::OpFailed { op: 0, error: String::new() }.is_terminal());
        assert!(Event::OpSkipped { op: 0, reason: String::new() }.is_terminal());
        assert!(!Event::OpBegin { op: 0 }.is_terminal());
        assert!(!Event::Paused { at: 0, freed: 0 }.is_terminal());
    }
}
