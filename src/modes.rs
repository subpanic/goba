//! The management modes: view/follow (R35–R40), kill (R42–R47), list (R30–R34), remove (R57–R59).

use std::ffi::OsString;

use crate::cli::Filter;
use crate::fail::Fail;
use crate::json;
use crate::store::{self, Disp, Meta, Store};
use crate::sys;

// ---------------------------------------------------------------- helpers

fn err(e: &std::io::Error) -> String {
    sys::err_text(e)
}

pub fn sig_name(sig: i32) -> String {
    let table: [(i32, &str); 27] = [
        (libc::SIGHUP, "SIGHUP"),
        (libc::SIGINT, "SIGINT"),
        (libc::SIGQUIT, "SIGQUIT"),
        (libc::SIGILL, "SIGILL"),
        (libc::SIGTRAP, "SIGTRAP"),
        (libc::SIGABRT, "SIGABRT"),
        (libc::SIGBUS, "SIGBUS"),
        (libc::SIGFPE, "SIGFPE"),
        (libc::SIGKILL, "SIGKILL"),
        (libc::SIGUSR1, "SIGUSR1"),
        (libc::SIGSEGV, "SIGSEGV"),
        (libc::SIGUSR2, "SIGUSR2"),
        (libc::SIGPIPE, "SIGPIPE"),
        (libc::SIGALRM, "SIGALRM"),
        (libc::SIGTERM, "SIGTERM"),
        (libc::SIGCHLD, "SIGCHLD"),
        (libc::SIGCONT, "SIGCONT"),
        (libc::SIGSTOP, "SIGSTOP"),
        (libc::SIGTSTP, "SIGTSTP"),
        (libc::SIGTTIN, "SIGTTIN"),
        (libc::SIGTTOU, "SIGTTOU"),
        (libc::SIGXCPU, "SIGXCPU"),
        (libc::SIGXFSZ, "SIGXFSZ"),
        (libc::SIGVTALRM, "SIGVTALRM"),
        (libc::SIGPROF, "SIGPROF"),
        (libc::SIGWINCH, "SIGWINCH"),
        (libc::SIGSYS, "SIGSYS"),
    ];
    table
        .iter()
        .find(|(n, _)| *n == sig)
        .map(|(_, s)| (*s).to_string())
        .unwrap_or_else(|| format!("signal {sig}"))
}

pub fn describe(d: Disp) -> String {
    match d {
        Disp::Running => "running".to_string(),
        Disp::Exited(c) => format!("exited({c})"),
        Disp::Killed(s) => format!("killed({})", sig_name(s)),
        Disp::Lost => "lost".to_string(),
    }
}

fn duration(ms: u64) -> String {
    let secs = ms / 1000;
    let (d, h, m, s) = (
        secs / 86_400,
        (secs % 86_400) / 3600,
        (secs % 3600) / 60,
        secs % 60,
    );
    if d > 0 {
        format!("{d}d {h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

fn age_ms(m: &Meta) -> u64 {
    let end = if m.ended_at > 0 {
        m.ended_at
    } else {
        sys::now_epoch_ms()
    };
    end.saturating_sub(m.started_at)
}

fn quote_cmd(argv: &[OsString]) -> String {
    argv.iter()
        .map(|a| {
            let s: String = a
                .to_string_lossy()
                .chars()
                .map(|c| if c.is_control() { '?' } else { c })
                .collect();
            if s.is_empty()
                || s.chars()
                    .any(|c| c.is_whitespace() || c == '\'' || c == '"' || c == '\\')
            {
                format!("'{}'", s.replace('\'', "'\\''"))
            } else {
                s
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------- view / follow

pub fn view(store: &Store, id: &str, follow: bool, lines: Option<u64>) -> Result<(), Fail> {
    let m = store.resolve(id)?;
    let fd = store
        .open_log(&m)
        .map_err(|e| Fail::NotFound(format!("session {} has no log: {}", m.num, err(&e))))?;

    let mut off = match lines {
        Some(n) => store::tail_offset(fd, n)
            .map_err(|e| Fail::Store(format!("cannot read log: {}", err(&e))))?,
        None => 0,
    };

    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = sys::pread(fd, &mut buf, off)
            .map_err(|e| Fail::Store(format!("cannot read log: {}", err(&e))))?;
        if n > 0 {
            // R35/R53: the log bytes are the entire stdout payload, verbatim.
            sys::write_all(libc::STDOUT_FILENO, &buf[..n])
                .map_err(|e| Fail::Store(format!("cannot write output: {}", err(&e))))?;
            off += n as u64;
            continue;
        }
        if !follow {
            break;
        }
        // R38: termination is decided by the record, never by EOF alone.
        let cur = match store.resolve(id) {
            Ok(c) => c,
            Err(_) => break,
        };
        if store::is_terminal(cur.state) || store::disp(&cur) == Disp::Lost {
            // One more pass: output that landed between our read and the terminal write.
            let n = sys::pread(fd, &mut buf, off)
                .map_err(|e| Fail::Store(format!("cannot read log: {}", err(&e))))?;
            if n > 0 {
                sys::write_all(libc::STDOUT_FILENO, &buf[..n])
                    .map_err(|e| Fail::Store(format!("cannot write output: {}", err(&e))))?;
                off += n as u64;
                continue;
            }
            break;
        }
        sys::sleep_ms(50); // R64: <= 100 ms follow latency
    }
    sys::close(fd);
    Ok(())
}

// ---------------------------------------------------------------- kill

pub fn kill(store: &Store, id: &str, timeout_ms: u64) -> Result<(), Fail> {
    let m = store.resolve(id)?;
    let d = store::disp(&m);
    if d != Disp::Running {
        return Err(Fail::NotFound(format!(
            "session {} is already {}",
            m.num,
            describe(d)
        )));
    }
    if m.pgid <= 0 {
        return Err(Fail::Kill(format!(
            "session {} has no process group recorded yet",
            m.num
        )));
    }

    // R42: the whole group, so descendants die with the leader.
    if let Err(e) = sys::kill(-m.pgid, libc::SIGTERM) {
        if e.raw_os_error() == Some(libc::ESRCH) {
            return Err(Fail::NotFound(format!(
                "session {} is already gone",
                m.num
            )));
        }
        return Err(Fail::Kill(format!(
            "cannot signal process group {}: {}",
            m.pgid,
            err(&e)
        )));
    }

    let deadline = sys::now_epoch_ms() + timeout_ms;
    let mut killed_hard = false;
    loop {
        if let Ok(cur) = store.resolve(id)
            && store::is_terminal(cur.state)
        {
            eprintln!(
                "goba: session {} ({}) {}",
                cur.num,
                cur.sid,
                if killed_hard {
                    "killed (SIGKILL)".to_string()
                } else {
                    describe(store::disp(&cur))
                }
            );
            return Ok(());
        }
        if sys::now_epoch_ms() >= deadline {
            if !killed_hard {
                // R43: escalation.
                killed_hard = true;
                let _ = sys::kill(-m.pgid, libc::SIGKILL);
                continue;
            }
            break;
        }
        sys::sleep_ms(20);
    }

    // Give the supervisor a last moment to record the SIGKILL outcome.
    for _ in 0..50 {
        sys::sleep_ms(10);
        if let Ok(cur) = store.resolve(id)
            && store::is_terminal(cur.state)
        {
            eprintln!(
                "goba: session {} ({}) {}",
                cur.num,
                cur.sid,
                describe(store::disp(&cur))
            );
            return Ok(());
        }
    }
    eprintln!(
        "goba: session {} ({}) signalled; terminal state not yet recorded",
        m.num, m.sid
    );
    Ok(())
}

// ---------------------------------------------------------------- list

pub fn list(store: &Store, filter: Filter, as_json: bool) -> Result<(), Fail> {
    let rows: Vec<(Meta, Disp)> = store
        .list()
        .into_iter()
        .map(|m| {
            let d = store::disp(&m);
            (m, d)
        })
        .filter(|(_, d)| match filter {
            Filter::All => true,
            Filter::Alive => *d == Disp::Running,
            Filter::Dead => *d != Disp::Running,
        })
        .collect();

    if as_json {
        println!("{}", json::session_list(store, &rows));
        return Ok(());
    }
    if rows.is_empty() {
        return Ok(()); // R33
    }

    let num_w = rows
        .iter()
        .map(|(m, _)| m.num.to_string().len())
        .max()
        .unwrap_or(1)
        .max(3);
    let sid_w = rows
        .iter()
        .map(|(m, _)| m.sid.chars().count())
        .max()
        .unwrap_or(3)
        .max(3);
    let state_w = rows
        .iter()
        .map(|(_, d)| describe(*d).len())
        .max()
        .unwrap_or(5)
        .max(5);

    if sys::isatty(libc::STDOUT_FILENO) {
        println!(
            "{:>w$}  {:<iw$}  {:<sw$}  {:<8}  COMMAND",
            "NUM",
            "SID",
            "STATE",
            "AGE",
            w = num_w,
            iw = sid_w,
            sw = state_w
        );
    }
    for (m, d) in &rows {
        println!(
            "{:>w$}  {:<iw$}  {:<sw$}  {:<8}  {}",
            m.num,
            m.sid,
            describe(*d),
            duration(age_ms(m)),
            quote_cmd(&m.argv),
            w = num_w,
            iw = sid_w,
            sw = state_w
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- remove

pub fn remove(store: &Store, id: &str) -> Result<(), Fail> {
    let m = store.resolve(id)?;
    let d = store::disp(&m);
    if d == Disp::Running {
        return Err(Fail::Remove(format!(
            "session {} ({}) is still running; kill it first",
            m.num, m.sid
        )));
    }
    store.remove(&m).map_err(|e| {
        Fail::Remove(format!("cannot remove session {}: {}", m.num, err(&e)))
    })?;
    eprintln!("goba: removed session {} ({})", m.num, m.sid);
    Ok(())
}
