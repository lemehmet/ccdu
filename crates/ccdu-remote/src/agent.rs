//! The remote end: `ccdu --agent`.
//!
//! Reads requests on stdin and writes responses on stdout, and nothing else — anything printed to
//! stdout that is not a frame would corrupt the stream, so diagnostics go to stderr where ssh
//! passes them through to the user.

use std::io::{self, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use ccdu_core::exec::{self, Control, ExecEvent, ExecOptions};
use ccdu_core::export;
use ccdu_core::plan::store::Store;
use ccdu_core::plan::Ident;
use ccdu_core::scan::{scan, Progress, ScanOptions};

use crate::protocol::{
    parse, read_frame, write_frame, write_message, Request, Response, TAG_MESSAGE, TAG_TREE,
    VERSION,
};

/// Serve requests until the peer says goodbye or closes the connection.
pub fn serve(input: impl Read, mut out: impl Write) -> io::Result<()> {
    let mut input = BufReader::new(input);
    let mut greeted = false;

    while let Some((tag, body)) = read_frame(&mut input)? {
        if tag != TAG_MESSAGE {
            write_message(
                &mut out,
                &Response::Error { message: format!("unexpected frame tag {tag}") },
            )?;
            continue;
        }
        let request: Request = match parse(&body) {
            Ok(request) => request,
            Err(e) => {
                write_message(&mut out, &Response::Error { message: e.to_string() })?;
                continue;
            }
        };

        match request {
            Request::Hello { version } => {
                // Version is checked before anything else runs, so a mismatch is a refusal rather
                // than a confusing failure part way through a scan.
                if version != VERSION {
                    write_message(
                        &mut out,
                        &Response::Error {
                            message: format!(
                                "agent speaks protocol v{VERSION}, caller speaks v{version}"
                            ),
                        },
                    )?;
                    return Ok(());
                }
                greeted = true;
                write_message(
                    &mut out,
                    &Response::Hello {
                        version: VERSION,
                        ccdu: env!("CARGO_PKG_VERSION").to_string(),
                        host: rustix::system::uname().nodename().to_string_lossy().into_owned(),
                    },
                )?;
            }

            _ if !greeted => {
                write_message(
                    &mut out,
                    &Response::Error { message: "expected a hello first".to_string() },
                )?;
                return Ok(());
            }

            Request::Scan { path, one_file_system, threads, exclude } => {
                serve_scan(&mut out, path, one_file_system, threads, exclude)?
            }

            Request::SavePlan { plan } => {
                let response = match Store::open_default().save(&plan) {
                    Ok(path) => Response::PlanSaved {
                        id: plan.id.clone(),
                        path: path.display().to_string(),
                    },
                    Err(e) => Response::Error { message: e.to_string() },
                };
                write_message(&mut out, &response)?;
            }

            Request::Identify { paths } => {
                let idents =
                    paths.iter().map(|p| Ident::of(std::path::Path::new(p)).ok()).collect();
                write_message(&mut out, &Response::Identities { idents })?;
            }

            Request::Apply { id, dry_run } => serve_apply(&mut out, &id, dry_run)?,

            Request::Bye => return Ok(()),
        }
    }
    Ok(())
}

fn serve_scan(
    out: &mut impl Write,
    path: String,
    one_file_system: bool,
    threads: usize,
    exclude: Vec<String>,
) -> io::Result<()> {
    let root = PathBuf::from(&path);
    let root = match root.canonicalize() {
        Ok(root) => root,
        Err(e) => {
            return write_message(out, &Response::Error { message: format!("{path}: {e}") });
        }
    };

    let opts = ScanOptions {
        one_file_system,
        threads: threads.clamp(1, 32),
        exclude_names: exclude.into_iter().map(|e| e.into_bytes()).collect(),
        ..Default::default()
    };

    write_message(out, &Response::Scanning)?;

    let (tx, rx) = crossbeam_channel::unbounded::<Progress>();
    let cancel = AtomicBool::new(false);

    // Progress is forwarded while the scan runs, so a slow filesystem still shows something at the
    // far end rather than an unexplained silence.
    let tree = std::thread::scope(|scope| {
        let cancel = &cancel;
        let scan_tx = tx.clone();
        let handle = scope.spawn(move || scan(&root, &opts, Some(&scan_tx), Some(cancel)));

        // The only remaining sender now lives in the scan thread, so the loop below ends when the
        // scan does. Holding one here would wait for a sender that never goes away.
        drop(tx);

        for update in &rx {
            let sent = write_message(
                out,
                &Response::Progress {
                    dirs: update.dirs,
                    entries: update.entries,
                    disk: update.disk,
                },
            );
            if sent.is_err() {
                // The caller has gone; stop walking rather than finish a scan nobody wants.
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                break;
            }
        }
        handle.join().unwrap_or_else(|_| Err(io::Error::other("scan thread panicked")))
    });

    match tree {
        Ok(tree) => {
            let mut blob = Vec::new();
            export::write(&tree, &mut blob, export::Format::Native)?;
            write_frame(out, TAG_TREE, &blob)
        }
        Err(e) => write_message(out, &Response::Error { message: e.to_string() }),
    }
}

/// Run a stored plan, forwarding progress as it happens.
///
/// The plan is loaded from this host's own store, so the caller names a plan rather than shipping
/// one: what runs is what was reviewed and saved here, not whatever arrived down the pipe.
fn serve_apply(out: &mut impl Write, id: &str, dry_run: bool) -> io::Result<()> {
    let store = Store::open_default();
    let plan = match store.load(id) {
        Ok(plan) => plan,
        Err(e) => {
            return write_message(out, &Response::Error { message: format!("plan {id}: {e}") })
        }
    };
    let dir = match store.dir_for(id) {
        Ok(dir) => dir,
        Err(e) => return write_message(out, &Response::Error { message: e.to_string() }),
    };

    let (tx, rx) = crossbeam_channel::unbounded::<ExecEvent>();
    let control = Control::new();

    let outcome = std::thread::scope(|scope| {
        let control = &control;
        let plan = &plan;
        let dir = &dir;
        let exec_tx = tx.clone();
        let handle = scope.spawn(move || {
            let opts = ExecOptions { dry_run, ..Default::default() };
            exec::execute(plan, dir, &opts, control, Some(&exec_tx))
        });
        // Same reasoning as the scan: the last sender must live in the worker, or this loop waits
        // for one that never goes away.
        drop(tx);

        for event in &rx {
            if write_message(out, &Response::Exec { event }).is_err() {
                control.pause();
                break;
            }
        }
        handle.join().unwrap_or_else(|_| Err(io::Error::other("executor panicked")))
    });

    match outcome {
        Ok(outcome) => {
            let state = exec::state(&dir).unwrap_or(exec::RunState::Interrupted);
            write_message(out, &Response::ExecDone { outcome, state })
        }
        Err(e) => write_message(out, &Response::Error { message: e.to_string() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::write_message as send;

    /// Drive the agent with a scripted set of requests and collect its responses.
    fn exchange(requests: &[Request]) -> Vec<(u8, Vec<u8>)> {
        let mut input = Vec::new();
        for request in requests {
            send(&mut input, request).unwrap();
        }
        let mut output = Vec::new();
        serve(io::Cursor::new(input), &mut output).unwrap();

        let mut frames = Vec::new();
        let mut cursor = io::Cursor::new(output);
        while let Some(frame) = read_frame(&mut cursor).unwrap() {
            frames.push(frame);
        }
        frames
    }

    fn messages(frames: &[(u8, Vec<u8>)]) -> Vec<Response> {
        frames
            .iter()
            .filter(|(tag, _)| *tag == TAG_MESSAGE)
            .map(|(_, body)| parse::<Response>(body).unwrap())
            .collect()
    }

    #[test]
    fn a_handshake_reports_the_version_and_host() {
        let frames = exchange(&[Request::Hello { version: VERSION }, Request::Bye]);
        let replies = messages(&frames);
        assert_eq!(replies.len(), 1);
        assert!(matches!(&replies[0], Response::Hello { version, .. } if *version == VERSION));
    }

    #[test]
    fn a_version_mismatch_is_refused_at_the_handshake() {
        let frames = exchange(&[
            Request::Hello { version: VERSION + 1 },
            Request::Scan {
                path: "/".into(),
                one_file_system: true,
                threads: 1,
                exclude: Vec::new(),
            },
        ]);
        let replies = messages(&frames);
        assert_eq!(replies.len(), 1, "nothing should run after a refused handshake");
        assert!(matches!(&replies[0], Response::Error { message } if message.contains("protocol")));
    }

    #[test]
    fn work_before_a_handshake_is_refused() {
        let frames = exchange(&[Request::Scan {
            path: "/".into(),
            one_file_system: true,
            threads: 1,
            exclude: Vec::new(),
        }]);
        let replies = messages(&frames);
        assert!(matches!(&replies[0], Response::Error { message } if message.contains("hello")));
    }

    #[test]
    fn a_scan_comes_back_as_a_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/file.bin"), vec![0u8; 20_000]).unwrap();

        let frames = exchange(&[
            Request::Hello { version: VERSION },
            Request::Scan {
                path: dir.path().display().to_string(),
                one_file_system: false,
                threads: 2,
                exclude: Vec::new(),
            },
            Request::Bye,
        ]);

        let (_, blob) = frames.iter().find(|(tag, _)| *tag == TAG_TREE).expect("no tree frame");
        let tree = export::read(io::Cursor::new(blob)).unwrap();
        assert!(tree.node(ccdu_core::model::ROOT).disk >= 20_000);
        assert_eq!(tree.node(ccdu_core::model::ROOT).items, 2);
    }

    #[test]
    fn scanning_a_path_that_is_not_there_is_an_error_not_a_crash() {
        let frames = exchange(&[
            Request::Hello { version: VERSION },
            Request::Scan {
                path: "/definitely/not/here".into(),
                one_file_system: false,
                threads: 1,
                exclude: Vec::new(),
            },
            Request::Bye,
        ]);
        let replies = messages(&frames);
        assert!(
            replies
                .iter()
                .any(|r| matches!(r, Response::Error { message } if message.contains("not/here"))),
            "{replies:?}"
        );
        assert!(!frames.iter().any(|(tag, _)| *tag == TAG_TREE));
    }

    #[test]
    fn identities_come_back_in_the_order_they_were_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("here"), vec![0u8; 100]).unwrap();

        let frames = exchange(&[
            Request::Hello { version: VERSION },
            Request::Identify {
                paths: vec![
                    dir.path().join("here").display().to_string(),
                    "/definitely/not/here".to_string(),
                ],
            },
            Request::Bye,
        ]);

        let replies = messages(&frames);
        let Response::Identities { idents } = &replies[1] else { panic!("{replies:?}") };
        assert_eq!(idents.len(), 2, "one answer per question, in order");
        assert_eq!(idents[0].as_ref().unwrap().size, 100);
        assert!(idents[1].is_none(), "a path that is not there has no identity");
    }

    #[test]
    fn a_plan_is_run_from_the_agents_own_store() {
        use ccdu_core::plan::{Op, Plan};

        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let target = dir.path().join("doomed");
        std::fs::write(&target, vec![0u8; 20_000]).unwrap();

        let mut plan = Plan::new(dir.path().to_path_buf());
        plan.ops.push(Op::Delete {
            ident: Ident::of(&target).unwrap(),
            path: target.clone(),
            est_bytes: 20_000,
        });
        Store::at(state.path().join("plans")).save(&plan).unwrap();

        // The agent reads CCDU_STATE_DIR the same way the rest of the program does.
        let previous = std::env::var_os("CCDU_STATE_DIR");
        unsafe { std::env::set_var("CCDU_STATE_DIR", state.path()) };

        let frames = exchange(&[
            Request::Hello { version: VERSION },
            Request::Apply { id: plan.id.clone(), dry_run: false },
            Request::Bye,
        ]);

        match previous {
            Some(value) => unsafe { std::env::set_var("CCDU_STATE_DIR", value) },
            None => unsafe { std::env::remove_var("CCDU_STATE_DIR") },
        }

        let replies = messages(&frames);
        assert!(
            replies.iter().any(|r| matches!(r, Response::Exec { .. })),
            "no progress was streamed: {replies:?}"
        );
        let done = replies.iter().find_map(|r| match r {
            Response::ExecDone { outcome, state } => Some((outcome.clone(), *state)),
            _ => None,
        });
        let (outcome, state) = done.expect("no completion reported");
        assert_eq!(outcome.done, 1, "{outcome:?}");
        assert_eq!(state, exec::RunState::Finished);
        assert!(!target.exists(), "the plan ran but the file is still there");
    }

    #[test]
    fn applying_a_plan_that_is_not_there_is_an_error() {
        let frames = exchange(&[
            Request::Hello { version: VERSION },
            Request::Apply { id: "20260101T000000-nosuchid".into(), dry_run: false },
            Request::Bye,
        ]);
        let replies = messages(&frames);
        assert!(
            replies
                .iter()
                .any(|r| matches!(r, Response::Error { message } if message.contains("plan"))),
            "{replies:?}"
        );
    }

    #[test]
    fn a_garbled_request_does_not_end_the_session() {
        let mut input = Vec::new();
        send(&mut input, &Request::Hello { version: VERSION }).unwrap();
        write_frame(&mut input, TAG_MESSAGE, b"{ garbage").unwrap();
        send(&mut input, &Request::Bye).unwrap();

        let mut output = Vec::new();
        serve(io::Cursor::new(input), &mut output).unwrap();

        let mut cursor = io::Cursor::new(output);
        let mut replies = Vec::new();
        while let Some((_, body)) = read_frame(&mut cursor).unwrap() {
            replies.push(parse::<Response>(&body).unwrap());
        }
        assert!(matches!(replies[0], Response::Hello { .. }));
        assert!(matches!(replies[1], Response::Error { .. }), "{replies:?}");
    }
}
