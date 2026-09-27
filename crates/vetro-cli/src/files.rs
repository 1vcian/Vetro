//! `vetro boot --files-ls/--files-cat/--files-put` (M8, ADR 0020): a few
//! file manager operations from the command line, to try out the
//! guest's `vetro-files` daemon without the web app.
//!
//! The operations run one after the other, in command-line
//! order, as soon as the daemon answers; the results go to stdout (`ls`:
//! one line per entry; `cat`: the file's bytes). Once they have all finished, `vetro` exits:
//! 0 if they succeeded, 1 otherwise (with the reason on stderr).

use std::collections::VecDeque;
use std::io::Write;

use vetro_machine::files::proto::{Entry, Kind, Stat, display_name};
use vetro_machine::files::{FilesError, Outcome};
use vetro_machine::{FilesClient, Machine};

/// An operation requested from the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileCmd {
    /// `--files-ls=PATH`
    Ls(String),
    /// `--files-cat=PATH`
    Cat(String),
    /// `--files-put=GUEST_PATH:HOST_FILE` (permissions 0644 if it is new).
    Put(String, Vec<u8>),
}

impl FileCmd {
    fn path(&self) -> &str {
        match self {
            FileCmd::Ls(p) | FileCmd::Cat(p) | FileCmd::Put(p, _) => p,
        }
    }
}

/// The operations in progress.
pub struct FilesTask {
    fc: FilesClient,
    queue: VecDeque<FileCmd>,
    current: Option<(u32, FileCmd)>,
    failed: bool,
}

/// `drwxr-x---` of an `st_mode`.
pub fn mode_string(s: &Stat) -> String {
    let t = match s.kind {
        Kind::Dir => 'd',
        Kind::Symlink => 'l',
        Kind::Char => 'c',
        Kind::Block => 'b',
        Kind::Fifo => 'p',
        Kind::Socket => 's',
        Kind::File | Kind::Other => '-',
    };
    let mut out = String::from(t);
    for shift in [6, 3, 0] {
        let b = (s.mode >> shift) & 7;
        out.push(if b & 4 != 0 { 'r' } else { '-' });
        out.push(if b & 2 != 0 { 'w' } else { '-' });
        out.push(if b & 1 != 0 { 'x' } else { '-' });
    }
    out
}

/// A line of `--files-ls`: mode, uid, gid, size, mtime, name, then
/// target and SELinux context if present.
pub fn ls_line(e: &Entry) -> String {
    let s = &e.stat;
    let name = display_name(&e.name);
    let mut line = format!("{} {} {} {} {} {name}", mode_string(s), s.uid, s.gid, s.size, s.mtime_s);
    if !s.link.is_empty() {
        line.push_str(&format!(" -> {}", display_name(&s.link)));
    }
    if !s.selinux.is_empty() {
        line.push_str(&format!(" [{}]", s.selinux));
    }
    line
}

impl FilesTask {
    pub fn new(cmds: Vec<FileCmd>) -> Self {
        FilesTask { fc: FilesClient::default(), queue: cmds.into(), current: None, failed: false }
    }

    /// Between one quantum and the next. `Some(succeeded)` when nothing is left.
    pub fn step(&mut self, m: &mut Machine, out: &mut impl Write) -> Option<bool> {
        if self.current.is_none() {
            let Some(cmd) = self.queue.pop_front() else { return Some(!self.failed) };
            let op = match &cmd {
                FileCmd::Ls(p) => self.fc.list(p),
                FileCmd::Cat(p) => self.fc.read_file(p),
                FileCmd::Put(p, data) => self.fc.write_file(p, data, 0o644),
            };
            self.current = Some((op, cmd));
        }
        self.fc.pump(m);
        while let Some(c) = self.fc.take_completion() {
            let Some((op, cmd)) = self.current.take_if(|(op, _)| *op == c.op) else { continue };
            let _ = op;
            match c.result {
                Ok(Outcome::List(entries)) => {
                    for e in &entries {
                        let _ = writeln!(out, "{}", ls_line(e));
                    }
                }
                Ok(Outcome::Data { data, .. }) => {
                    let _ = out.write_all(&data);
                }
                Ok(Outcome::Written(s)) => {
                    eprintln!("vetro-files: wrote {} ({} bytes, {})", cmd.path(), s.size, mode_string(&s));
                }
                Ok(_) => {}
                Err(e) => {
                    self.fail(&cmd, &e);
                }
            }
            let _ = out.flush();
        }
        None
    }

    fn fail(&mut self, cmd: &FileCmd, e: &FilesError) {
        eprintln!("vetro-files: {}: {e}", cmd.path());
        self.failed = true;
    }
}

/// `GUEST_PATH:HOST_FILE` of `--files-put`.
pub fn parse_put(v: &str) -> Result<FileCmd, String> {
    let (guest, host) =
        v.split_once(':').ok_or_else(|| format!("--files-put={v}: GUEST_PATH:HOST_FILE required"))?;
    if !guest.starts_with('/') {
        return Err(format!("--files-put={v}: the guest path must be absolute"));
    }
    let data = std::fs::read(host).map_err(|e| format!("{host}: {e}"))?;
    Ok(FileCmd::Put(guest.to_string(), data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn righe_di_ls() {
        let s = Stat {
            kind: Kind::Symlink,
            mode: 0o120777,
            uid: 0,
            gid: 0,
            size: 5,
            mtime_s: 1_790_000_000,
            mtime_ns: 0,
            nlink: 1,
            link: "a.txt".into(),
            selinux: "u:object_r:shell_data_file:s0".into(),
        };
        assert_eq!(
            ls_line(&Entry { name: "link".into(), stat: s.clone() }),
            "lrwxrwxrwx 0 0 5 1790000000 link -> a.txt [u:object_r:shell_data_file:s0]"
        );
        let f = Stat { kind: Kind::File, mode: 0o100640, link: Vec::new(), selinux: String::new(), ..s };
        assert_eq!(
            ls_line(&Entry { name: b"n\xff".to_vec(), stat: f.clone() }),
            "-rw-r----- 0 0 5 1790000000 n\\xff"
        );
        assert_eq!(mode_string(&f), "-rw-r-----");
        assert!(parse_put("relativo:x").is_err());
        assert!(parse_put("/tmp/x").is_err());
        assert!(parse_put("/tmp/x:/non/esiste").is_err());
    }
}
