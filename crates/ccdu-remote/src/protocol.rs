//! Frames on the wire.
//!
//! Every frame is a one-byte tag, a four-byte length, then that many bytes. Control messages are
//! JSON, which keeps the protocol readable when something goes wrong; the tree is sent as ccdu's
//! native export, which is compact and needs no re-encoding at either end.

use std::io::{self, Read, Write};

use ccdu_core::exec::{ExecEvent, Outcome, RunState};
use ccdu_core::plan::{Ident, Plan};
use serde::{Deserialize, Serialize};

/// Bumped when the messages change incompatibly. Both ends check on connect, so a mismatch is a
/// clear refusal rather than a confusing failure halfway through a scan.
pub const VERSION: u32 = 1;

/// A frame larger than this is refused before allocating. The tree of a very large filesystem is
/// the biggest legitimate payload, and 4 GiB is far past it.
const MAX_FRAME: u32 = u32::MAX / 2;

pub const TAG_MESSAGE: u8 = 0;
pub const TAG_TREE: u8 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Request {
    Hello {
        version: u32,
    },
    Scan {
        path: String,
        one_file_system: bool,
        threads: usize,
        exclude: Vec<String>,
    },
    /// Store a plan on the remote, where its paths mean something.
    SavePlan {
        plan: Box<Plan>,
    },
    /// Read the identity of some paths, so a tree fetched from here can be staged against.
    /// Staging records what an entry looked like, and only the machine holding it can say.
    Identify {
        paths: Vec<String>,
    },
    /// Run a stored plan. Progress is streamed back as it happens.
    Apply {
        id: String,
        dry_run: bool,
    },
    Bye,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Response {
    Hello {
        version: u32,
        ccdu: String,
        host: String,
    },
    Progress {
        dirs: u64,
        entries: u64,
        disk: u64,
    },
    /// Followed immediately by a [`TAG_TREE`] frame holding the tree itself.
    Scanning,
    PlanSaved {
        id: String,
        path: String,
    },
    /// One entry per requested path, `None` where it could not be read.
    Identities {
        idents: Vec<Option<Ident>>,
    },
    Exec {
        event: ExecEvent,
    },
    ExecDone {
        outcome: Outcome,
        state: RunState,
    },
    Error {
        message: String,
    },
}

pub fn write_message(out: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    let body = serde_json::to_vec(value)?;
    write_frame(out, TAG_MESSAGE, &body)
}

pub fn write_frame(out: &mut impl Write, tag: u8, body: &[u8]) -> io::Result<()> {
    if body.len() as u64 > MAX_FRAME as u64 {
        return Err(io::Error::other(format!("frame of {} bytes is too large", body.len())));
    }
    out.write_all(&[tag])?;
    out.write_all(&(body.len() as u32).to_le_bytes())?;
    out.write_all(body)?;
    out.flush()
}

/// Read one frame. Returns `None` at a clean end of stream.
pub fn read_frame(input: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    match input.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }

    let len = u32::from_le_bytes(header[1..5].try_into().expect("fixed width"));
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("peer announced a {len}-byte frame"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    input.read_exact(&mut body)?;
    Ok(Some((header[0], body)))
}

pub fn parse<T: for<'de> Deserialize<'de>>(body: &[u8]) -> io::Result<T> {
    serde_json::from_slice(body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad message: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let mut buffer = Vec::new();
        let sent = Request::Scan {
            path: "/data".into(),
            one_file_system: true,
            threads: 4,
            exclude: vec![".git".into()],
        };
        write_message(&mut buffer, &sent).unwrap();

        let (tag, body) = read_frame(&mut io::Cursor::new(&buffer)).unwrap().unwrap();
        assert_eq!(tag, TAG_MESSAGE);
        assert_eq!(parse::<Request>(&body).unwrap(), sent);
    }

    #[test]
    fn several_frames_come_back_in_order() {
        let mut buffer = Vec::new();
        write_message(
            &mut buffer,
            &Response::Hello { version: VERSION, ccdu: "0.1.0".into(), host: "server".into() },
        )
        .unwrap();
        write_message(&mut buffer, &Response::Progress { dirs: 1, entries: 2, disk: 3 }).unwrap();
        write_frame(&mut buffer, TAG_TREE, b"CCDU-ish bytes").unwrap();

        let mut cursor = io::Cursor::new(&buffer);
        let first = read_frame(&mut cursor).unwrap().unwrap();
        assert!(matches!(parse::<Response>(&first.1).unwrap(), Response::Hello { .. }));
        let second = read_frame(&mut cursor).unwrap().unwrap();
        assert!(matches!(parse::<Response>(&second.1).unwrap(), Response::Progress { .. }));
        let third = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(third, (TAG_TREE, b"CCDU-ish bytes".to_vec()));
        assert!(read_frame(&mut cursor).unwrap().is_none(), "clean end of stream");
    }

    #[test]
    fn execution_events_cross_the_wire() {
        let mut buffer = Vec::new();
        write_message(
            &mut buffer,
            &Response::Exec { event: ExecEvent::Finished { index: 3, freed: 4096 } },
        )
        .unwrap();
        write_message(
            &mut buffer,
            &Response::ExecDone {
                outcome: Outcome { done: 3, freed: 12288, ..Default::default() },
                state: RunState::Finished,
            },
        )
        .unwrap();

        let mut cursor = io::Cursor::new(&buffer);
        let (_, body) = read_frame(&mut cursor).unwrap().unwrap();
        match parse::<Response>(&body).unwrap() {
            Response::Exec { event: ExecEvent::Finished { index, freed } } => {
                assert_eq!((index, freed), (3, 4096))
            }
            other => panic!("{other:?}"),
        }
        let (_, body) = read_frame(&mut cursor).unwrap().unwrap();
        match parse::<Response>(&body).unwrap() {
            Response::ExecDone { outcome, state } => {
                assert_eq!(outcome.freed, 12288);
                assert_eq!(state, RunState::Finished);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_truncated_frame_is_an_error_not_a_hang() {
        let mut buffer = Vec::new();
        write_message(&mut buffer, &Request::Bye).unwrap();
        buffer.truncate(buffer.len() - 2);

        let err = read_frame(&mut io::Cursor::new(&buffer)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn an_absurd_length_is_refused_before_allocating() {
        // Five bytes claiming a payload of nearly four gigabytes.
        let mut header = vec![TAG_TREE];
        header.extend_from_slice(&u32::MAX.to_le_bytes());

        let err = read_frame(&mut io::Cursor::new(&header)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("announced"), "{err}");
    }

    #[test]
    fn an_empty_stream_ends_cleanly() {
        assert!(read_frame(&mut io::Cursor::new(Vec::new())).unwrap().is_none());
    }

    #[test]
    fn a_garbled_message_body_is_reported() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, TAG_MESSAGE, b"{not json").unwrap();
        let (_, body) = read_frame(&mut io::Cursor::new(&buffer)).unwrap().unwrap();
        assert!(parse::<Request>(&body).is_err());
    }
}
