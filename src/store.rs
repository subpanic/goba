//! The session store: layout (R16–R18), the record codec (R22–R24), identity (R26–R29), reboot
//! scoping (R19–R21) and log access (R35–R37, R63).
//!
//! Layout:
//! ```text
//! $GOBA_DIR/          0700
//!   boot              current boot identity
//!   s/
//!     3-make/         one session; name = <numeric id>-<string id> (R26, R27)
//!       meta          0600, atomically replaced
//!       log           0600, raw captured bytes
//!     t.<pid>/        half-built session (ignored by listing, reclaimed if its creator is gone)
//! ```
//!
//! Every operation is relative to one `O_DIRECTORY` fd held by `Store` — never a re-resolved
//! absolute path (R18).

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::fail::Fail;
use crate::platform;
use crate::sys::{self, Fd};

pub const SCHEMA: u32 = 1;
const DIR_MODE: libc::mode_t = 0o700;
const FILE_MODE: libc::mode_t = 0o600;

// `DirBuilder::mode` takes a `u32`, but `libc::mode_t` is `u16` on macOS and `u32` on Linux
// (R67): the widening cast is required on macOS and redundant on Linux.
#[allow(clippy::unnecessary_cast)]
const DIR_MODE_U32: u32 = DIR_MODE as u32;

/// A published session directory: `s/<num>-<sid>`.
#[derive(Clone, Debug)]
pub struct Entry {
    pub num: u64,
    pub sid: String,
    name: OsString,
}

impl Entry {
    /// Path relative to the store root.
    fn path(&self) -> OsString {
        let mut p = OsString::from("s/");
        p.push(&self.name);
        p
    }
}

/// Directory entries under `s/`: published sessions and half-built (`t.<pid>`) or unknown names.
type Scan = (Vec<Entry>, Vec<OsString>);

fn sdir(num: u64, sid: &str) -> OsString {
    OsString::from(format!("s/{num}-{sid}"))
}

/// `5-make` -> (5, "make"). Anything else is not a session directory.
fn parse_name(name: &OsStr) -> Option<(u64, String)> {
    let s = name.to_str()?;
    let (n, sid) = s.split_once('-')?;
    if sid.is_empty() {
        return None;
    }
    Some((n.parse::<u64>().ok()?, sid.to_string()))
}

/// Derive a session's string id from the command name (R27): the basename of `argv[0]`, folded to
/// a lowercase, shell-typeable slug. Anything that is not safe to type unquoted becomes `-`.
fn slug(argv0: &OsStr) -> String {
    const MAX: usize = 24;
    let base = Path::new(argv0).file_name().unwrap_or(argv0);
    let mut out = String::new();
    let mut dashed = false;
    for &b in base.as_bytes() {
        let c = b.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'+') {
            out.push(c as char);
            dashed = false;
        } else if !dashed && !out.is_empty() {
            out.push('-');
            dashed = true;
        }
    }
    out.truncate(MAX); // ASCII only, so this cannot split a character
    let out = out.trim_matches(|c| c == '-' || c == '.').to_string();
    if out.is_empty() {
        "cmd".to_string()
    } else {
        out
    }
}

/// R27/R13b: which token names a session started with `-c <string>`. The first word of the script
/// is what the user actually asked for (`-c 'make -j8 && ./run'` should be `make`), skipping any
/// leading `VAR=value` assignments. `None` means "fall back to the shell's own name".
///
/// This is a *heuristic* tokenizer, not a shell parser: it tracks quoting so a spaced value does
/// not split into words, but it knows nothing about `$(…)`, backticks or escapes. That is safe
/// because the result is only ever used to name a session — never to decide what runs.
pub fn shell_name_source(script: &OsStr) -> Option<OsString> {
    for word in split_words(&script.to_string_lossy()) {
        let (head, _) = word.split_once('=').unwrap_or((word.as_str(), ""));
        // `VAR=value`, or the degenerate `=x`. An identifier before `=` is what makes it an
        // assignment: `./a=b` and `--flag=1` are ordinary words.
        let is_assignment = word.contains('=')
            && (head.is_empty()
                || (head.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !head.starts_with(|c: char| c.is_ascii_digit())
                    && !head.is_empty()));
        if !is_assignment {
            return Some(OsString::from(word));
        }
    }
    None
}

/// Split on whitespace that is not inside single or double quotes.
fn split_words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in text.chars() {
        match quote {
            Some(q) if c == q => {
                quote = None;
                cur.push(c);
            }
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// R27: the bare name if free, else `base-2`, `base-3`, … — the smallest suffix not in use.
fn pick_sid(base: &str, taken: &std::collections::HashSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_string();
    }
    for n in 2..10_000u32 {
        let candidate = format!("{base}-{n}");
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    base.to_string()
}

// ---------------------------------------------------------------- record

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    Starting,
    Running,
    Exited,
    Killed,
    /// Never written to disk: derived when the recorded process is gone but no status was
    /// recorded (supervisor died). See [`disp`].
    Lost,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Running => "running",
            State::Exited => "exited",
            State::Killed => "killed",
            State::Lost => "lost",
        }
    }

    fn parse(s: &str) -> Option<State> {
        Some(match s {
            "starting" => State::Starting,
            "running" => State::Running,
            "exited" => State::Exited,
            "killed" => State::Killed,
            "lost" => State::Lost,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Meta {
    pub num: u64,
    pub sid: String,
    pub boot: String,
    pub state: State,
    pub pid: i32,
    pub pgid: i32,
    pub spid: i32,
    pub pid_start: u64,
    pub started_at: u64,
    pub ended_at: u64,
    pub exit_code: i32,
    pub signal: i32,
    pub cwd: PathBuf,
    pub exe: PathBuf,
    pub argv: Vec<OsString>,
}

impl Meta {
    fn blank() -> Meta {
        Meta {
            num: 0,
            sid: String::new(),
            boot: String::new(),
            state: State::Starting,
            pid: 0,
            pgid: 0,
            spid: 0,
            pid_start: 0,
            started_at: 0,
            ended_at: 0,
            exit_code: -1,
            signal: 0,
            cwd: PathBuf::new(),
            exe: PathBuf::new(),
            argv: Vec::new(),
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut kv = |k: &str, v: String| {
            out.extend_from_slice(k.as_bytes());
            out.push(b'=');
            out.extend_from_slice(v.as_bytes());
            out.push(b'\n');
        };
        kv("schema", SCHEMA.to_string());
        // Note: neither the numeric id nor the string id is stored. The session directory name is
        // `<num>-<sid>` and is the single source of truth for both, so a record can never disagree
        // with where it lives, and a publication that has to retry for a free name never has to
        // rewrite its own record.
        kv("boot", pct(self.boot.as_bytes()));
        kv("state", self.state.as_str().to_string());
        kv("pid", self.pid.to_string());
        kv("pgid", self.pgid.to_string());
        kv("spid", self.spid.to_string());
        kv("pid_start", self.pid_start.to_string());
        kv("started_at", self.started_at.to_string());
        kv("ended_at", self.ended_at.to_string());
        kv("exit_code", self.exit_code.to_string());
        kv("signal", self.signal.to_string());
        kv("cwd", pct(self.cwd.as_os_str().as_bytes()));
        kv("exe", pct(self.exe.as_os_str().as_bytes()));
        let argv = self
            .argv
            .iter()
            .map(|a| pct(a.as_bytes()))
            .collect::<Vec<_>>()
            .join("\0");
        kv("argv", argv);
        out
    }

    fn decode(raw: &[u8], num: u64, sid: &str) -> Option<Meta> {
        let text = std::str::from_utf8(raw).ok()?;
        let mut m = Meta::blank();
        m.num = num;
        m.sid = sid.to_string();
        let mut seen_schema = false;
        for line in text.lines() {
            let (k, v) = line.split_once('=')?;
            match k {
                "schema" => {
                    if v.parse::<u32>().ok()? != SCHEMA {
                        return None;
                    }
                    seen_schema = true;
                }
                "boot" => m.boot = unpct_str(v),
                "state" => m.state = State::parse(v)?,
                "pid" => m.pid = v.parse().ok()?,
                "pgid" => m.pgid = v.parse().ok()?,
                "spid" => m.spid = v.parse().ok()?,
                "pid_start" => m.pid_start = v.parse().ok()?,
                "started_at" => m.started_at = v.parse().ok()?,
                "ended_at" => m.ended_at = v.parse().ok()?,
                "exit_code" => m.exit_code = v.parse().ok()?,
                "signal" => m.signal = v.parse().ok()?,
                "cwd" => m.cwd = PathBuf::from(OsString::from_vec(unpct(v))),
                "exe" => m.exe = PathBuf::from(OsString::from_vec(unpct(v))),
                "argv" => {
                    m.argv = v
                        .split('\0')
                        .map(|a| OsString::from_vec(unpct(a)))
                        .collect();
                }
                _ => {}
            }
        }
        if !seen_schema {
            return None;
        }
        Some(m)
    }
}

fn pct(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len());
    for &c in b {
        if c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b'~' | b'/' | b':') {
            s.push(c as char);
        } else {
            s.push('%');
            s.push_str(&format!("{c:02X}"));
        }
    }
    s
}

fn unpct(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn unpct_str(s: &str) -> String {
    String::from_utf8_lossy(&unpct(s)).into_owned()
}

// ---------------------------------------------------------------- derived display state

/// What a session *is*, as opposed to what it last recorded.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Disp {
    Running,
    Exited(i32),
    Killed(i32),
    /// Recorded as live, but neither the command nor its supervisor exists any more (R25: a PID
    /// alone is not liveness, so a reused PID must never be reported as running).
    Lost,
}

pub fn disp(m: &Meta) -> Disp {
    match m.state {
        State::Exited => Disp::Exited(m.exit_code),
        State::Killed => Disp::Killed(m.signal),
        State::Starting | State::Running | State::Lost => {
            if process_alive(m.pid, m.pid_start) {
                Disp::Running
            } else if m.spid > 0 && sys::kill_ok(m.spid) {
                // Hand-off window: the supervisor is alive, so a terminal record is imminent.
                Disp::Running
            } else {
                Disp::Lost
            }
        }
    }
}

pub fn process_alive(pid: i32, recorded_start: u64) -> bool {
    if pid <= 0 || !sys::kill_ok(pid) {
        return false;
    }
    if recorded_start == 0 {
        return false;
    }
    match platform::pid_start(pid) {
        Ok(now) => now == recorded_start,
        Err(_) => false,
    }
}

pub fn is_terminal(s: State) -> bool {
    matches!(s, State::Exited | State::Killed)
}

// ---------------------------------------------------------------- store

pub struct Store {
    fd: Fd,
    boot: String,
}

fn base_path() -> PathBuf {
    if let Some(p) = std::env::var_os("GOBA_DIR")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    for var in ["XDG_RUNTIME_DIR", "TMPDIR"] {
        if let Some(p) = std::env::var_os(var)
            && !p.is_empty()
        {
            let mut b = PathBuf::from(p);
            b.push("goba");
            return b;
        }
    }
    PathBuf::from(format!("/tmp/goba-{}", sys::geteuid()))
}

impl Store {
    pub fn open() -> Result<Store, Fail> {
        let path = base_path();
        let created = match std::fs::DirBuilder::new()
            .mode(DIR_MODE_U32)
            .create(&path)
        {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
            Err(e) => {
                return Err(Fail::Store(format!(
                    "cannot create {}: {}",
                    path.display(),
                    sys::err_text(&e)
                )))
            }
        };

        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|e| {
                Fail::Store(format!(
                    "cannot open {}: {} (symlinks are not allowed)",
                    path.display(),
                    sys::err_text(&e)
                ))
            })?;
        let fd = std::os::fd::AsRawFd::as_raw_fd(&file);

        // R17: real directory, ours, not reachable by group/other.
        let st = sys::fstat(fd)
            .map_err(|e| Fail::Store(format!("cannot stat {}: {}", path.display(), sys::err_text(&e))))?;
        if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(Fail::Store(format!("{} is not a directory", path.display())));
        }
        if st.st_uid != sys::geteuid() {
            return Err(Fail::Store(format!(
                "{} is owned by uid {}, not {}",
                path.display(),
                st.st_uid,
                sys::geteuid()
            )));
        }
        if st.st_mode & 0o077 != 0 {
            return Err(Fail::Store(format!(
                "{} has mode {:04o}; refusing (group/other access is not allowed)",
                path.display(),
                st.st_mode & 0o7777
            )));
        }
        if created {
            let _ = sys::fchmod(fd, DIR_MODE);
        }
        std::mem::forget(file); // the fd outlives the File; Store closes it on drop

        let boot = platform::boot_id()
            .map_err(|e| Fail::Store(format!("cannot determine boot identity: {}", sys::err_text(&e))))?;

        let store = Store { fd, boot };
        let stamped = sys::read_file_at(store.fd, OsStr::new("boot"))
            .map(|b| String::from_utf8_lossy(&b).trim().to_string())
            .unwrap_or_default();
        if stamped != store.boot {
            let dir = store.fd;
            let _ = sys::mkdirat(dir, OsStr::new("s"), DIR_MODE);
            store.write_boot_stamp()?;
        }
        Ok(store)
    }

    fn write_boot_stamp(&self) -> Result<(), Fail> {
        // Callers race here by construction: every process that finds a stale or absent stamp
        // writes it. A shared temp name would let one process rename the file out from under
        // another, so the name is per-process.
        let tmp = OsString::from(format!("boot.tmp.{}", sys::getpid()));
        let fd = sys::openat(
            self.fd,
            &tmp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW,
            FILE_MODE,
        )
        .map_err(|e| Fail::Store(format!("cannot write boot stamp: {}", sys::err_text(&e))))?;
        let _ = sys::write_all(fd, self.boot.as_bytes());
        sys::close(fd);
        sys::renameat(self.fd, &tmp, OsStr::new("boot"))
            .map_err(|e| Fail::Store(format!("cannot write boot stamp: {}", sys::err_text(&e))))
    }

    /// Session directory names, split into published sessions and everything else (`t.<pid>` from
    /// a creation in flight, or a name that is not a session at all).
    fn scan(&self) -> io::Result<Scan> {
        let sfd = match sys::openat(self.fd, OsStr::new("s"), libc::O_DIRECTORY, 0) {
            Ok(fd) => fd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
            Err(e) => return Err(e),
        };
        let names = sys::readdir(sfd);
        sys::close(sfd);
        let mut sessions = Vec::new();
        let mut junk = Vec::new();
        for n in names? {
            match parse_name(&n) {
                Some((num, sid)) => sessions.push(Entry { num, sid, name: n }),
                None => junk.push(n),
            }
        }
        sessions.sort_by_key(|e| e.num);
        Ok((sessions, junk))
    }

    fn read_meta(&self, e: &Entry) -> io::Result<Meta> {
        let mut name = e.path();
        name.push("/meta");
        let raw = sys::read_file_at(self.fd, &name)?;
        Meta::decode(&raw, e.num, &e.sid).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("corrupt record for {}", e.name.to_string_lossy()),
            )
        })
    }

    pub fn write_meta(&self, m: &Meta) -> io::Result<()> {
        self.write_meta_in(&sdir(m.num, &m.sid), m)
    }

    fn write_meta_in(&self, dir: &OsStr, m: &Meta) -> io::Result<()> {
        // R23: temp file in the same directory, then rename — readers never see a torn record.
        let mut tmp = dir.to_os_string();
        tmp.push("/m.tmp");
        let mut final_name = dir.to_os_string();
        final_name.push("/meta");
        let fd = sys::openat(
            self.fd,
            &tmp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW,
            FILE_MODE,
        )?;
        let body = m.encode();
        let w = sys::write_all(fd, &body);
        sys::close(fd);
        w?;
        sys::renameat(self.fd, &tmp, &final_name)
    }

    /// Atomic allocation **and** publication in one step (R26, R27).
    ///
    /// The session is built in `s/t.<pid>` (invisible to listing) and then renamed into place as
    /// `<num>-<sid>`. That rename *is* the claim on both identifiers at once: it succeeds only if
    /// no session already holds that name, so no half-built or empty directory is ever visible,
    /// and two concurrent `goba make` can never both end up as `make` — the loser sees the name
    /// taken and comes back as `make-2`.
    ///
    /// An earlier design pre-claimed the number with `mkdir` and filled it afterwards. That looks
    /// equivalent and is not: the reclaimer (R21) could delete a claim that was still being
    /// filled, after which two sessions owned the same number.
    fn publish(&self, tmp: &OsStr, meta: &mut Meta, base: &str) -> io::Result<()> {
        for _ in 0..4096 {
            let (sessions, _) = self.scan()?;
            let num = sessions.last().map(|e| e.num + 1).unwrap_or(1);
            let taken: std::collections::HashSet<String> =
                sessions.iter().map(|e| e.sid.clone()).collect();
            let sid = pick_sid(base, &taken);

            meta.num = num;
            meta.sid = sid;
            self.write_meta_in(tmp, meta)?;

            match sys::renameat(self.fd, tmp, &sdir(num, &meta.sid)) {
                Ok(()) => return Ok(()),
                Err(e) => match e.raw_os_error() {
                    // Someone took the number or the name between our scan and our rename.
                    Some(libc::ENOTEMPTY) | Some(libc::EEXIST) | Some(libc::EISDIR) => continue,
                    _ => return Err(e),
                },
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free session id",
        ))
    }

    /// Create a session: build it in `t.<pid>`, then publish it with one rename, so no reader ever
    /// observes a half-built session, no number is ever visible before it is complete, and no
    /// failure path leaves anything behind (R48, R50).
    pub fn create(&self, argv: &[OsString], cwd: &Path, name_source: &OsStr) -> Result<(Meta, Fd), Fail> {
        let tmp = OsString::from(format!("s/t.{}", sys::getpid()));
        // A recycled pid can leave a non-empty directory of the same name behind.
        if sys::unlinkat(self.fd, &tmp, libc::AT_REMOVEDIR).is_err() {
            let _ = self.remove_tree(&tmp);
        }
        if let Err(e) = sys::mkdirat(self.fd, &tmp, DIR_MODE) {
            return Err(Fail::Store(format!("cannot create session: {}", sys::err_text(&e))));
        }

        let logname = {
            let mut p = tmp.clone();
            p.push("/log");
            p
        };
        let logfd = match sys::openat(
            self.fd,
            &logname,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            FILE_MODE,
        ) {
            Ok(fd) => fd,
            Err(e) => {
                let _ = self.remove_tree(&tmp);
                return Err(Fail::Store(format!("cannot create log: {}", sys::err_text(&e))));
            }
        };

        let mut meta = Meta {
            num: 0, // assigned by `publish`, and read back from the directory name thereafter
            sid: String::new(),
            boot: self.boot.clone(),
            state: State::Starting,
            pid: 0,
            pgid: 0,
            spid: 0,
            pid_start: 0,
            started_at: sys::now_epoch_ms(),
            ended_at: 0,
            exit_code: -1,
            signal: 0,
            cwd: cwd.to_path_buf(),
            exe: PathBuf::new(),
            argv: argv.to_vec(),
        };
        // The record is written by `publish`, because only the successful attempt knows the name.
        if let Err(e) = self.publish(&tmp, &mut meta, &slug(name_source)) {
            sys::close(logfd);
            let _ = self.remove_tree(&tmp);
            return Err(Fail::Store(format!("cannot create session: {}", sys::err_text(&e))));
        }
        Ok((meta, logfd))
    }

    fn remove_tree(&self, name: &OsStr) -> io::Result<()> {
        let dfd = match sys::openat(self.fd, name, libc::O_DIRECTORY | libc::O_NOFOLLOW, 0) {
            Ok(fd) => fd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        for entry in sys::readdir(dfd).unwrap_or_default() {
            let _ = sys::unlinkat(dfd, &entry, 0);
        }
        sys::close(dfd);
        sys::unlinkat(self.fd, name, libc::AT_REMOVEDIR)
    }

    /// Delete a session by its directory name (the caller already resolved it).
    pub fn remove(&self, m: &Meta) -> io::Result<()> {
        self.remove_tree(&sdir(m.num, &m.sid))
    }

    pub fn open_log(&self, m: &Meta) -> io::Result<Fd> {
        let mut name = sdir(m.num, &m.sid);
        name.push("/log");
        sys::openat(self.fd, &name, libc::O_RDONLY | libc::O_NOFOLLOW, 0)
    }

    pub fn log_bytes(&self, m: &Meta) -> u64 {
        match self.open_log(m) {
            Ok(fd) => {
                let n = sys::fstat(fd).map(|s| s.st_size as u64).unwrap_or(0);
                sys::close(fd);
                n
            }
            Err(_) => 0,
        }
    }

    /// All sessions of the current boot, oldest first. Reclaims what it walks past (R21).
    ///
    /// Reclamation may only ever delete something it can *prove* is not in use: a session whose
    /// record says it belongs to a previous boot, or a leftover `t.<pid>` whose owner is gone.
    /// A published number with an unreadable record is only reclaimed once it is clearly
    /// abandoned, so a publication in flight can never be taken away from its owner.
    pub fn list(&self) -> Vec<Meta> {
        let (sessions, junk) = self.scan().unwrap_or_default();
        let mut out = Vec::new();
        for e in sessions {
            match self.read_meta(&e) {
                Ok(m) if m.boot == self.boot => out.push(m),
                // A record that parses but belongs to another boot is unambiguously stale.
                Ok(_) => {
                    let _ = self.remove_tree(&e.path());
                }
                Err(_) => {
                    if self.is_abandoned(&e.path()) {
                        let _ = self.remove_tree(&e.path());
                    }
                }
            }
        }
        for name in junk {
            // `t.<pid>` whose creator is gone is an abandoned creation. `name` is relative to
            // `s/`, so re-qualify it before unlinking.
            let mut full = OsString::from("s/");
            full.push(&name);
            let pid: i32 = name
                .to_string_lossy()
                .trim_start_matches("t.")
                .parse()
                .unwrap_or(0);
            if pid > 0 {
                if !sys::kill_ok(pid) {
                    let _ = self.remove_tree(&full);
                }
            } else if self.is_abandoned(&full) {
                // A name that is not a session at all: only touched once clearly left behind.
                let _ = self.remove_tree(&full);
            }
        }
        out
    }

    fn is_abandoned(&self, path: &OsStr) -> bool {
        const GRACE_SECS: libc::time_t = 60;
        let now = (sys::now_epoch_ms() / 1000) as libc::time_t;
        match sys::fstatat(self.fd, path) {
            Ok(st) => now - st.st_mtime > GRACE_SECS,
            Err(_) => false,
        }
    }

    /// The reclaim pass, off the spawn critical path (R21, R62).
    pub fn gc(&self) {
        let _ = self.list();
    }

    /// R28: numeric id, full string id, or unambiguous prefix. Ambiguity is an error.
    ///
    /// The string id lives in the directory name, so an exact match costs one directory read
    /// plus one record read — no full-store scan.
    pub fn resolve(&self, token: &str) -> Result<Meta, Fail> {
        if token.is_empty() {
            return Err(Fail::Usage("empty id".to_string()));
        }
        let (sessions, _) = self.scan().unwrap_or_default();
        let not_found = || Fail::NotFound(format!("no session '{token}'"));

        let hit: &Entry = if token.bytes().all(|b| b.is_ascii_digit()) {
            match token.parse::<u64>() {
                Ok(num) => sessions.iter().find(|e| e.num == num).ok_or_else(not_found)?,
                Err(_) => return Err(not_found()),
            }
        } else if let Some(e) = sessions.iter().find(|e| e.sid == token) {
            // An exact name always wins over a prefix match.
            e
        } else {
            let hits: Vec<&Entry> = sessions
                .iter()
                .filter(|e| e.sid.starts_with(token))
                .collect();
            match hits.len() {
                0 => return Err(not_found()),
                1 => hits[0],
                _ => {
                    return Err(Fail::Ambiguous(
                        token.to_string(),
                        hits.iter().map(|e| format!("{} ({})", e.sid, e.num)).collect(),
                    ));
                }
            }
        };

        match self.read_meta(hit) {
            // A session from a previous boot is not this boot's session (R20); the next
            // reclamation pass will delete it.
            Ok(m) if m.boot == self.boot => Ok(m),
            _ => Err(not_found()),
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        sys::close(self.fd);
    }
}

/// R36: byte offset of the start of the last `n` lines. A trailing newline does not count as
/// starting a further empty line (`tail -n` semantics).
pub fn tail_offset(fd: Fd, n: u64) -> io::Result<u64> {
    let size = sys::fstat(fd)?.st_size as u64;
    if n == 0 {
        return Ok(size);
    }
    let mut buf = vec![0u8; 64 * 1024];
    let mut count = 0u64;
    let mut pos = size;
    while pos > 0 {
        let start = pos.saturating_sub(buf.len() as u64);
        let len = (pos - start) as usize;
        let got = sys::pread(fd, &mut buf[..len], start)?;
        if got == 0 {
            break;
        }
        for idx in (0..got).rev() {
            let abs = start + idx as u64;
            if buf[idx] == b'\n' && abs + 1 != size {
                count += 1;
                if count == n {
                    return Ok(abs + 1);
                }
            }
        }
        pos = start;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_roundtrip() {
        let m = Meta {
            num: 12,
            sid: "a7k2mq".into(),
            boot: "b0-01".into(),
            state: State::Exited,
            pid: 4242,
            pgid: 4242,
            spid: 4241,
            pid_start: 998877,
            started_at: 1_700_000_000_000,
            ended_at: 1_700_000_001_000,
            exit_code: 7,
            signal: 0,
            cwd: PathBuf::from("/tmp/a b"),
            exe: PathBuf::from("/bin/echo"),
            argv: vec![
                OsString::from("echo"),
                OsString::from("-n"),
                OsString::from("hello world\nsecond line"),
                OsString::from("100%"),
            ],
        };
        let back = Meta::decode(&m.encode(), m.num, &m.sid).expect("decodes");
        assert_eq!(back.num, m.num);
        assert_eq!(back.sid, m.sid);
        assert_eq!(back.boot, m.boot);
        assert_eq!(back.state, m.state);
        assert_eq!(back.exit_code, 7);
        assert_eq!(back.ended_at, m.ended_at);
        assert_eq!(back.cwd, m.cwd);
        assert_eq!(back.exe, m.exe);
        assert_eq!(back.argv, m.argv);
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(Meta::decode(b"not a record", 1, "x").is_none());
        assert!(Meta::decode(b"schema=99\nnum=1\n", 1, "x").is_none());
        assert!(Meta::decode(b"num=1\n", 1, "x").is_none()); // no schema
    }

    #[test]
    fn identity_comes_from_the_directory_not_the_record() {
        let m = Meta {
            num: 7,
            sid: "make-2".into(),
            ..Meta::blank()
        };
        let encoded = m.encode();
        assert!(!encoded.windows(4).any(|w| w == b"num="));
        assert!(!encoded.windows(4).any(|w| w == b"sid="));
        let back = Meta::decode(&encoded, 42, "other").unwrap();
        assert_eq!((back.num, back.sid.as_str()), (42, "other"));
    }

    #[test]
    fn slug_is_a_typeable_command_name() {
        let cases: [(&str, &str); 14] = [
            ("make", "make"),
            ("/usr/bin/make", "make"),
            ("./build.sh", "build.sh"),
            ("/bin/sleep", "sleep"),
            ("CURL", "curl"),
            ("my  weird   tool", "my-weird-tool"),
            ("weird name!!", "weird-name"),
            ("", "cmd"),
            ("/", "cmd"),
            ("..", "cmd"),
            ("...", "cmd"),
            ("...hidden", "hidden"),
            (".env", "env"),
            ("-leading", "leading"),
        ];
        for (input, want) in cases {
            assert_eq!(slug(OsStr::new(input)), want, "slug({input:?})");
        }
        // Long names are truncated to something you can type.
        assert_eq!(slug(OsStr::new(&"x".repeat(80))).len(), 24);
    }

    #[test]
    fn suffixes_fill_the_first_free_slot() {
        let taken = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<std::collections::HashSet<_>>();
        assert_eq!(pick_sid("make", &taken(&[])), "make");
        assert_eq!(pick_sid("make", &taken(&["make"])), "make-2");
        assert_eq!(pick_sid("make", &taken(&["make", "make-2"])), "make-3");
        // A gap is reused, and another command's names do not interfere.
        assert_eq!(pick_sid("make", &taken(&["make", "make-3"])), "make-2");
        assert_eq!(pick_sid("make", &taken(&["curl"])), "make");
    }

    #[test]
    fn shell_script_names_come_from_the_first_word() {
        let w = |s: &str| shell_name_source(OsStr::new(s));
        assert_eq!(w("make -j8 && ./run"), Some(OsString::from("make")));
        assert_eq!(w("  ls | wc -l"), Some(OsString::from("ls")));
        assert_eq!(w("FOO=1 BAR=2 make -j8"), Some(OsString::from("make")));
        assert_eq!(w("X=y; cmd"), Some(OsString::from("cmd")));
        // A quoted value containing whitespace must not split the assignment apart.
        assert_eq!(w("X=\"a  b\"; printf hi"), Some(OsString::from("printf")));
        assert_eq!(w("X='p q' rsync -a"), Some(OsString::from("rsync")));
        assert_eq!(w("./run.sh --flag=1"), Some(OsString::from("./run.sh")));
        assert_eq!(w("/usr/bin/env python3 x.py"), Some(OsString::from("/usr/bin/env")));
        assert_eq!(w(""), None);
        assert_eq!(w("   "), None);
        assert_eq!(w("=x"), None);
        assert_eq!(w("FOO=1"), None);
    }

    #[test]
    fn session_directory_names_round_trip() {
        assert_eq!(parse_name(OsStr::new("5-make")), Some((5, "make".into())));
        assert_eq!(parse_name(OsStr::new("12-make-2")), Some((12, "make-2".into())));
        assert_eq!(parse_name(OsStr::new("3-a-b-c")), Some((3, "a-b-c".into())));
        assert_eq!(parse_name(OsStr::new("t.1234")), None);
        assert_eq!(parse_name(OsStr::new("5-")), None);
        assert_eq!(parse_name(OsStr::new("5")), None);
        assert_eq!(parse_name(OsStr::new("make")), None);
    }

    #[test]
    fn percent_codec_is_byte_exact() {
        let raw: Vec<u8> = (0u8..=255).collect();
        assert_eq!(unpct(&pct(&raw)), raw);
    }
}
