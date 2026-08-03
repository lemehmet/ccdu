//! The local end: spawn an agent and talk to it.
//!
//! The transport is a `Command`, so this works over ssh in production and over a directly spawned
//! agent in tests — same code path, no second machine required.

use std::io::{self, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use ccdu_core::export;
use ccdu_core::model::Tree;
use ccdu_core::plan::Plan;
use ccdu_core::scan::Progress;
use crossbeam_channel::Sender;

use crate::protocol::{
    parse, read_frame, write_message, Request, Response, TAG_MESSAGE, TAG_TREE, VERSION,
};

/// Where to scan: a host and a path on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// Everything ssh needs to reach the machine, `[user@]host` with an optional `:port`.
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
}

impl Target {
    /// Parse `ssh://[user@]host[:port]/path`, or the scp-style `[user@]host:path`.
    ///
    /// Returns `None` for anything that is an ordinary local path, so callers can simply try this
    /// first — a path with a colon in it is far more likely than a host called `./data`.
    pub fn parse(text: &str) -> Option<Target> {
        if let Some(rest) = text.strip_prefix("ssh://") {
            let (authority, path) = rest.split_once('/')?;
            let (host, port) = split_port(authority);
            if host.is_empty() || path.is_empty() {
                return None;
            }
            return Some(Target { host, port, path: format!("/{path}") });
        }

        // scp form. Rejected when it looks like a local path so `./a:b` stays a file.
        if text.starts_with('.') || text.starts_with('/') || text.starts_with('~') {
            return None;
        }
        let (host, path) = text.split_once(':')?;
        if host.is_empty() || path.is_empty() || host.contains('/') {
            return None;
        }
        Some(Target { host: host.to_string(), port: None, path: path.to_string() })
    }

    /// The command that starts an agent on the far side.
    pub fn agent_command(&self, remote_binary: &str) -> Command {
        let mut cmd = Command::new("ssh");
        if let Some(port) = self.port {
            cmd.arg("-p").arg(port.to_string());
        }
        cmd.arg(&self.host).arg(remote_binary).arg("--agent");
        cmd
    }

    /// The fallback for a host with no ccdu on it: ask ncdu for a dump on standard output.
    pub fn ncdu_command(&self) -> Command {
        let mut cmd = Command::new("ssh");
        if let Some(port) = self.port {
            cmd.arg("-p").arg(port.to_string());
        }
        cmd.arg(&self.host).arg("ncdu").arg("-o-").arg("-x").arg(&self.path);
        cmd
    }
}

fn split_port(authority: &str) -> (String, Option<u16>) {
    match authority.rsplit_once(':') {
        Some((host, port)) => match port.parse() {
            Ok(port) => (host.to_string(), Some(port)),
            // Not a port: probably an IPv6 literal or a colon in a username.
            Err(_) => (authority.to_string(), None),
        },
        None => (authority.to_string(), None),
    }
}

/// A live connection to an agent.
pub struct Remote {
    child: Child,
    input: BufReader<ChildStdout>,
    output: ChildStdin,
    pub host: String,
    pub version: String,
}

impl Remote {
    /// Start `command` and shake hands with the agent it runs.
    pub fn connect(mut command: Command) -> io::Result<Remote> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr is left alone so ssh's own messages reach the user.
            .spawn()?;

        let output = child.stdin.take().expect("stdin was piped");
        let input = BufReader::new(child.stdout.take().expect("stdout was piped"));
        let mut remote =
            Remote { child, input, output, host: String::new(), version: String::new() };

        write_message(&mut remote.output, &Request::Hello { version: VERSION })?;
        match remote.next_message()? {
            Some(Response::Hello { version, ccdu, host }) => {
                if version != VERSION {
                    return Err(io::Error::other(format!(
                        "remote speaks protocol v{version}, this build speaks v{VERSION}"
                    )));
                }
                remote.host = host;
                remote.version = ccdu;
                Ok(remote)
            }
            Some(Response::Error { message }) => Err(io::Error::other(message)),
            // End of stream at the handshake almost always means the command was not found. Say
            // that, rather than report the absence of a reply as if it were a surprise.
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the remote said nothing; ccdu may not be installed or on its PATH",
            )),
            other => Err(io::Error::other(format!("unexpected greeting: {other:?}"))),
        }
    }

    /// Scan a path on the far side and bring the tree back.
    pub fn scan(
        &mut self,
        path: &str,
        one_file_system: bool,
        threads: usize,
        exclude: Vec<String>,
        progress: Option<&Sender<Progress>>,
    ) -> io::Result<Tree> {
        write_message(
            &mut self.output,
            &Request::Scan { path: path.to_string(), one_file_system, threads, exclude },
        )?;

        loop {
            let Some((tag, body)) = read_frame(&mut self.input)? else {
                return Err(io::Error::other("the remote closed the connection mid-scan"));
            };
            if tag == TAG_TREE {
                return export::read(io::Cursor::new(body));
            }
            match parse::<Response>(&body)? {
                Response::Progress { dirs, entries, disk } => {
                    if let Some(tx) = progress {
                        tx.send(Progress {
                            dirs,
                            entries,
                            disk,
                            current: std::path::PathBuf::new(),
                        })
                        .ok();
                    }
                }
                Response::Scanning => {}
                Response::Error { message } => return Err(io::Error::other(message)),
                other => return Err(io::Error::other(format!("unexpected reply: {other:?}"))),
            }
        }
    }

    /// Store a plan on the remote host, where its paths mean something. Returns the plan id and
    /// the file it was written to.
    pub fn save_plan(&mut self, plan: &Plan) -> io::Result<(String, String)> {
        write_message(&mut self.output, &Request::SavePlan { plan: Box::new(plan.clone()) })?;
        match self.next_message()? {
            Some(Response::PlanSaved { id, path }) => Ok((id, path)),
            Some(Response::Error { message }) => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("unexpected reply: {other:?}"))),
        }
    }

    fn next_message(&mut self) -> io::Result<Option<Response>> {
        loop {
            let Some((tag, body)) = read_frame(&mut self.input)? else { return Ok(None) };
            if tag == TAG_MESSAGE {
                return parse::<Response>(&body).map(Some);
            }
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        // Best effort: say goodbye so the agent exits cleanly, then make sure it is gone.
        let _ = write_message(&mut self.output, &Request::Bye);
        let _ = self.output.flush();
        let _ = self.child.wait();
    }
}

/// Scan a remote path using ncdu, for hosts that have it but not ccdu.
pub fn scan_with_ncdu(target: &Target) -> io::Result<Tree> {
    let output = target.ncdu_command().stdout(Stdio::piped()).spawn()?.wait_with_output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "ncdu on {} exited with {}",
            target.host, output.status
        )));
    }
    export::read(io::Cursor::new(output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_urls_are_understood() {
        assert_eq!(
            Target::parse("ssh://server/var/log"),
            Some(Target { host: "server".into(), port: None, path: "/var/log".into() })
        );
        assert_eq!(
            Target::parse("ssh://me@server:2222/data"),
            Some(Target { host: "me@server".into(), port: Some(2222), path: "/data".into() })
        );
    }

    #[test]
    fn the_scp_form_is_understood() {
        assert_eq!(
            Target::parse("server:/var/log"),
            Some(Target { host: "server".into(), port: None, path: "/var/log".into() })
        );
        assert_eq!(
            Target::parse("me@server:data"),
            Some(Target { host: "me@server".into(), port: None, path: "data".into() })
        );
    }

    #[test]
    fn local_paths_are_not_mistaken_for_hosts() {
        // A colon is legal in a filename, and treating one as a hostname would send somebody's
        // local scan to a machine that does not exist.
        for local in ["/var/log", "./weird:name", "~/data", "relative/path", "plain"] {
            assert_eq!(Target::parse(local), None, "{local:?} was taken for a remote target");
        }
    }

    #[test]
    fn malformed_urls_are_rejected() {
        for bad in ["ssh://", "ssh://host", "ssh:///path", "ssh://host/", "host:"] {
            assert_eq!(Target::parse(bad), None, "accepted {bad:?}");
        }
    }

    #[test]
    fn an_ipv6_literal_is_not_read_as_a_port() {
        let target = Target::parse("ssh://[fe80::1]/data").unwrap();
        assert_eq!(target.port, None);
        assert_eq!(target.host, "[fe80::1]");
    }

    #[test]
    fn the_commands_are_shaped_the_way_ssh_expects() {
        let target = Target::parse("ssh://me@server:2222/data").unwrap();

        let agent = target.agent_command("ccdu");
        let args: Vec<_> = agent.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["-p", "2222", "me@server", "ccdu", "--agent"]);

        let ncdu = target.ncdu_command();
        let args: Vec<_> = ncdu.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["-p", "2222", "me@server", "ncdu", "-o-", "-x", "/data"]);
    }
}
