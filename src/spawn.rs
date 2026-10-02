//! Spawning: front-end → supervisor → command (R1–R7, R48–R50).
//!
//! The front-end exits as soon as the id is printable, so a *supervisor* must outlive it: nobody
//! else would `waitpid` the command or record its exit status. The front-end learns the truth
//! about `exec` through a `CLOEXEC` pipe, so "command not found" is reported synchronously and
//! leaves nothing behind.

use std::ffi::{CString, OsStr, OsString};
use std::io;

use crate::fail::Fail;
use crate::platform;
use crate::store::{Meta, State, Store};
use crate::sys::{self, Fd, Fork};

const OK: u32 = 0;

fn reply(fd: Fd, code: u32) {
    let _ = sys::write_all(fd, &code.to_le_bytes());
}

fn spawn_err(msg: String) -> Fail {
    Fail::Spawn(msg)
}

fn etext(e: &io::Error) -> String {
    sys::err_text(e)
}

pub fn run(
    store: &Store,
    argv: &[OsString],
    quiet: bool,
    name_source: &OsStr,
) -> Result<(), Fail> {
    let cprog = sys::cstr(&argv[0])
        .map_err(|e| spawn_err(format!("bad command name: {}", etext(&e))))?;
    let cargv: Vec<CString> = argv
        .iter()
        .map(|a| sys::cstr(a))
        .collect::<Result<_, _>>()
        .map_err(|e| spawn_err(format!("bad argument: {}", etext(&e))))?;
    let cwd = std::env::current_dir()
        .map_err(|e| spawn_err(format!("cannot read working directory: {}", etext(&e))))?;

    // R48: the record exists (and is complete) before the id is ever printed.
    let (meta, logfd) = store.create(argv, &cwd, name_source)?;

    let (devnull, log_hi, dev_hi) = match (|| -> io::Result<(Fd, Fd, Fd)> {
        let devnull = sys::open_devnull()?;
        let log_hi = sys::dupfd_above(logfd, 3)?;
        let dev_hi = sys::dupfd_above(devnull, 3)?;
        Ok((devnull, log_hi, dev_hi))
    })() {
        Ok(v) => v,
        Err(e) => {
            sys::close(logfd);
            let _ = store.remove(&meta);
            return Err(spawn_err(format!("cannot prepare descriptors: {}", etext(&e))));
        }
    };

    // R6: hold terminal signals across the fork window; the child drops them once `setsid` is done.
    let oldmask = match sys::block_term_signals() {
        Ok(m) => m,
        Err(e) => {
            sys::close(logfd);
            sys::close(log_hi);
            sys::close(devnull);
            sys::close(dev_hi);
            let _ = store.remove(&meta);
            return Err(spawn_err(format!("cannot block signals: {}", etext(&e))));
        }
    };

    let (ar, aw) = match sys::pipe2(libc::O_CLOEXEC) {
        Ok(p) => p,
        Err(e) => {
            sys::restore_sigmask(&oldmask);
            sys::close(logfd);
            sys::close(log_hi);
            sys::close(devnull);
            sys::close(dev_hi);
            let _ = store.remove(&meta);
            return Err(spawn_err(format!("cannot create pipe: {}", etext(&e))));
        }
    };

    match sys::fork() {
        Err(e) => {
            sys::restore_sigmask(&oldmask);
            sys::close(ar);
            sys::close(aw);
            sys::close(logfd);
            sys::close(log_hi);
            sys::close(devnull);
            sys::close(dev_hi);
            let _ = store.remove(&meta);
            Err(spawn_err(format!("cannot fork: {}", etext(&e))))
        }
        Ok(Fork::Child) => supervise(
            store, meta, oldmask, cprog, cargv, ar, aw, log_hi, dev_hi,
        ),
        Ok(Fork::Parent(spid)) => {
            sys::close(aw);
            sys::close(logfd);
            sys::close(log_hi);
            sys::close(devnull);
            sys::close(dev_hi);
            sys::restore_sigmask(&oldmask);

            let mut buf = [0u8; 4];
            let got = sys::read_exact_raw(ar, &mut buf).unwrap_or(false);
            sys::close(ar);

            let code = if got { u32::from_le_bytes(buf) } else { u32::MAX };
            if code != OK {
                let _ = sys::waitpid(spid);
                let _ = store.remove(&meta); // R50: nothing left behind
                let detail = if got {
                    etext(&io::Error::from_raw_os_error(code as i32))
                } else {
                    "supervisor exited before reporting".to_string()
                };
                return Err(spawn_err(format!(
                    "{}: {}",
                    argv[0].to_string_lossy(),
                    detail
                )));
            }

            if quiet {
                println!("{}", meta.num);
            } else {
                println!("{} {}", meta.num, meta.sid);
            }
            Ok(())
        }
    }
}

fn pid_start_retry(pid: libc::pid_t) -> u64 {
    for _ in 0..3 {
        if let Ok(v) = platform::pid_start(pid)
            && v != 0
        {
            return v;
        }
        sys::sleep_ms(1);
    }
    0
}

/// The supervisor. Never returns.
#[allow(clippy::too_many_arguments)]
fn supervise(
    store: &Store,
    mut meta: Meta,
    oldmask: libc::sigset_t,
    cprog: CString,
    cargv: Vec<CString>,
    ar: Fd,
    aw: Fd,
    log_hi: Fd,
    dev: Fd,
) -> ! {
    // R1: new session, no controlling terminal — immune to the terminal's signals for good.
    let _ = sys::setsid();
    sys::restore_sigmask(&oldmask);

    // R7c: cut the detached tree loose from the invoker's stdio. Without this the supervisor keeps
    // the caller's stdout/stderr open for the whole life of the job, so anything reading goba's
    // output through a pipe (`$(goba …)`, subprocess capture, CI logs) blocks until the job ends.
    // Invisible from an interactive terminal, fatal for scripts.
    let _ = sys::dup2(dev, 0);
    let _ = sys::dup2(dev, 1);
    let _ = sys::dup2(dev, 2);

    let (er, ew) = match sys::pipe2(libc::O_CLOEXEC) {
        Ok(p) => p,
        Err(e) => {
            reply(aw, e.raw_os_error().unwrap_or(libc::EIO) as u32);
            sys::exit_now(1)
        }
    };

    match sys::fork() {
        Ok(Fork::Child) => {
            // ---- the command
            sys::close(er);
            sys::close(ar);
            sys::close(aw);
            let _ = sys::setpgid(0, 0); // R2: its own process group
            let _ = sys::dup2(dev, 0); // R7: /dev/null stdin
            let _ = sys::dup2(log_hi, 1);
            let _ = sys::dup2(log_hi, 2); // one shared offset => write order preserved
            if log_hi > 2 {
                sys::close(log_hi);
            }
            if dev > 2 {
                sys::close(dev);
            }
            // A background command gets a pristine signal environment.
            for s in [
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGPIPE,
                libc::SIGTSTP,
                libc::SIGTTIN,
                libc::SIGTTOU,
            ] {
                sys::sig_handler(s, libc::SIG_DFL);
            }
            sys::unblock_all();
            let e = sys::execvp(&cprog, &cargv); // R13: no shell; R49: errno goes to the front
            let code = e.raw_os_error().unwrap_or(libc::ENOENT) as u32;
            reply(ew, code);
            sys::exit_now(127)
        }
        Ok(Fork::Parent(cpid)) => {
            sys::close(ew);
            let mut buf = [0u8; 4];
            // The handshake: the write end is CLOEXEC, so EOF means `exec` succeeded and the
            // kernel closed it. Four bytes means `exec` failed with that errno. (R49)
            let failed = sys::read_exact_raw(er, &mut buf).unwrap_or(false);
            sys::close(er);

            // Survive anything terminal-adjacent: the supervisor must outlive the command so it
            // can record the terminal state. Set AFTER the fork, so the command keeps SIG_DFL.
            for s in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGPIPE] {
                sys::sig_handler(s, libc::SIG_IGN);
            }

            if failed {
                let code = u32::from_le_bytes(buf);
                let _ = sys::waitpid(cpid);
                reply(aw, code);
                sys::exit_now(1);
            }

            meta.state = State::Running;
            meta.pid = cpid;
            meta.pgid = cpid;
            meta.spid = sys::getpid();
            meta.pid_start = pid_start_retry(cpid);
            meta.exe = platform::exe_path(cpid).unwrap_or_default();
            meta.started_at = sys::now_epoch_ms();
            let _ = store.write_meta(&meta);

            reply(aw, OK);
            sys::close(aw);
            sys::close(log_hi);
            sys::close(dev);

            // R21: reclamation, deliberately off the spawn critical path.
            store.gc();

            match sys::waitpid(cpid) {
                Ok(sys::Status::Exited(c)) => {
                    meta.state = State::Exited;
                    meta.exit_code = c;
                }
                Ok(sys::Status::Signaled(s)) => {
                    meta.state = State::Killed;
                    meta.signal = s;
                }
                Err(_) => meta.state = State::Lost,
            }
            meta.ended_at = sys::now_epoch_ms();
            let _ = store.write_meta(&meta);
            sys::exit_now(0)
        }
        Err(e) => {
            reply(aw, e.raw_os_error().unwrap_or(libc::EIO) as u32);
            sys::exit_now(1)
        }
    }
}
