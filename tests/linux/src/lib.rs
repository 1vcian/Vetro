//! Test differenziali di programmi Linux arm64: lo stesso binario, con gli
//! stessi argomenti, ambiente, stdin e directory di lavoro, gira su Vetro e
//! su `qemu-aarch64`. Si confrontano codice d'uscita (o segnale), stdout,
//! stderr e i file lasciati nella directory di lavoro.
//!
//! I binari vengono da `tools/guest-bins/build.sh` (in `target/guest-bins`,
//! o `VETRO_GUEST_BINS`). Se mancano i test stampano SKIP; con
//! `VETRO_REQUIRE_GUEST_BINS=1` (in CI) falliscono.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use vetro_cli::linux::{Config, Exit};
use vetro_diff::qemu;

/// Directory dei binari guest.
pub fn guest_bins() -> PathBuf {
    std::env::var_os("VETRO_GUEST_BINS")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/guest-bins"))
}

/// Percorso di un binario guest, oppure `None` (SKIP) se manca.
pub fn guest_bin(name: &str, test: &str) -> Option<PathBuf> {
    let p = guest_bins().join(name);
    if p.is_file() {
        return Some(std::fs::canonicalize(p).unwrap());
    }
    if std::env::var("VETRO_REQUIRE_GUEST_BINS").is_ok_and(|v| v == "1") {
        panic!("{test}: binario guest {name} mancante (tools/guest-bins/build.sh)");
    }
    eprintln!("SKIP {test}: {} mancante, esegui tools/guest-bins/build.sh", p.display());
    None
}

/// Esito confrontabile di un processo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Exited(i32),
    Signaled(i32),
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub status: Status,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// File della directory di lavoro dopo l'esecuzione: percorso → contenuto
    /// (le directory hanno contenuto vuoto e percorso che finisce con '/').
    pub files: BTreeMap<String, Vec<u8>>,
}

pub struct Case {
    name: String,
    prog: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
    stdin: Vec<u8>,
    files: Vec<(String, Vec<u8>)>,
    links: Vec<(String, PathBuf)>,
    compare_stderr: bool,
}

pub fn case(name: &str, prog: &Path, args: &[&str]) -> Case {
    Case {
        name: name.into(),
        prog: prog.to_path_buf(),
        args: args.iter().map(|s| s.to_string()).collect(),
        env: Vec::new(),
        stdin: Vec::new(),
        files: Vec::new(),
        links: Vec::new(),
        compare_stderr: true,
    }
}

impl Case {
    /// Variabile d'ambiente del guest; `{wd}` nel valore diventa la
    /// directory di lavoro.
    pub fn env(mut self, k: &str, v: &str) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }

    pub fn stdin(mut self, data: &[u8]) -> Self {
        self.stdin = data.to_vec();
        self
    }

    /// File iniziale nella directory di lavoro (sottodirectory con '/').
    pub fn file(mut self, path: &str, data: &[u8]) -> Self {
        self.files.push((path.into(), data.to_vec()));
        self
    }

    /// Link simbolico iniziale nella directory di lavoro.
    pub fn link(mut self, path: &str, target: &Path) -> Self {
        self.links.push((path.into(), target.to_path_buf()));
        self
    }

    /// Non confrontare stderr (messaggi che includono dati dell'host).
    pub fn ignore_stderr(mut self) -> Self {
        self.compare_stderr = false;
        self
    }

    fn workdir(&self) -> PathBuf {
        let slug: String =
            self.name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp/linux").join(slug)
    }

    fn prepare(&self) -> PathBuf {
        let dir = self.workdir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (p, data) in &self.files {
            let path = dir.join(p);
            if p.ends_with('/') {
                std::fs::create_dir_all(&path).unwrap();
            } else {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(&path, data).unwrap();
            }
        }
        for (p, target) in &self.links {
            let path = dir.join(p);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::os::unix::fs::symlink(target, &path).unwrap();
        }
        std::fs::canonicalize(dir).unwrap()
    }

    pub fn run_vetro(&self) -> Run {
        let dir = self.prepare();
        let image = std::fs::read(&self.prog).unwrap();
        let mut argv = vec![self.prog.to_string_lossy().into_owned()];
        argv.extend(self.args.iter().cloned());
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let wd = dir.to_string_lossy();
        let env: Vec<String> =
            self.env.iter().map(|(k, v)| format!("{k}={}", v.replace("{wd}", &wd))).collect();
        let envp: Vec<&str> = env.iter().map(String::as_str).collect();
        let cfg = Config {
            stdin: self.stdin.clone(),
            cwd: dir.to_string_lossy().into_owned(),
            max_steps: 5_000_000_000,
            ..Config::default()
        }
        .jit_from_env();
        let out = vetro_cli::run_elf(&image, &argv, &envp, &self.prog.to_string_lossy(), cfg);
        let (status, stdout, stderr) = match out {
            Ok(o) => {
                let st = match o.exit {
                    Exit::Code(c) => Status::Exited(c),
                    Exit::Signal { signo, .. } => Status::Signaled(signo),
                    other => Status::Other(format!("{other:?}")),
                };
                (st, o.stdout, o.stderr)
            }
            Err(e) => (Status::Other(e), Vec::new(), Vec::new()),
        };
        Run { status, stdout, stderr, files: snapshot(&dir) }
    }

    pub fn run_qemu(&self, q: &Path) -> Run {
        let dir = self.prepare();
        let wd = dir.to_string_lossy().into_owned();
        let env: Vec<(String, String)> =
            self.env.iter().map(|(k, v)| (k.clone(), v.replace("{wd}", &wd))).collect();
        let out =
            qemu::run_program(q, &self.prog, &self.args, &env, &dir, &self.stdin, Duration::from_secs(120));
        let (status, stdout, stderr) = match out {
            Ok(o) => {
                let st = match (o.signal(), o.exit_code) {
                    (Some(s), _) => Status::Signaled(s),
                    (None, Some(c)) => Status::Exited(c),
                    (None, None) => Status::Other("terminato senza codice".into()),
                };
                let mut stderr = o.stderr;
                // Il messaggio di QEMU sul segnale non fa parte dell'output del guest.
                if let Some(i) = find(&stderr, b"qemu: uncaught target signal") {
                    stderr.truncate(i);
                }
                (st, o.stdout, stderr)
            }
            Err(e) => (Status::Other(e.to_string()), Vec::new(), Vec::new()),
        };
        Run { status, stdout, stderr, files: snapshot(&dir) }
    }

    /// Esegue su Vetro e su QEMU e pretende esiti identici. Senza oracolo
    /// esegue solo Vetro e restituisce il suo esito.
    pub fn check(self) -> Run {
        let ours = self.run_vetro();
        let Some(q) = qemu::locate_or_skip(&self.name) else { return ours };
        let theirs = self.run_qemu(&q);
        let mut diff = String::new();
        if ours.status != theirs.status {
            diff += &format!("  esito: vetro={:?} qemu={:?}\n", ours.status, theirs.status);
        }
        if ours.stdout != theirs.stdout {
            diff += &format!(
                "  stdout:\n    vetro: {:?}\n    qemu:  {:?}\n",
                String::from_utf8_lossy(&ours.stdout),
                String::from_utf8_lossy(&theirs.stdout)
            );
        }
        if self.compare_stderr && ours.stderr != theirs.stderr {
            diff += &format!(
                "  stderr:\n    vetro: {:?}\n    qemu:  {:?}\n",
                String::from_utf8_lossy(&ours.stderr),
                String::from_utf8_lossy(&theirs.stderr)
            );
        }
        if ours.files != theirs.files {
            diff += &format!(
                "  file:\n    vetro: {:?}\n    qemu:  {:?}\n",
                ours.files.keys().collect::<Vec<_>>(),
                theirs.files.keys().collect::<Vec<_>>()
            );
            for (k, v) in &ours.files {
                if theirs.files.get(k) != Some(v) {
                    diff += &format!("    diverso: {k}\n");
                }
            }
        }
        assert!(diff.is_empty(), "{}: Vetro e QEMU divergono\n{diff}", self.name);
        ours
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                out.insert(format!("{rel}/"), Vec::new());
                walk(base, &p, out);
            } else if ft.is_symlink() {
                let t = std::fs::read_link(&p).map(|t| t.to_string_lossy().into_owned()).unwrap_or_default();
                out.insert(rel, format!("-> {t}").into_bytes());
            } else {
                out.insert(rel, std::fs::read(&p).unwrap_or_default());
            }
        }
    }
    walk(dir, dir, &mut out);
    out
}
