//! Thin wrappers over libc. This is the only module with `unsafe` FFI; everything above it is
//! safe Rust. Each wrapper maps one syscall (or one small syscall cluster) to an `io::Result`.
//!
//! Program-wide invariant: goba is **single-threaded** for its entire lifetime, so `fork()` is
//! always safe here and no lock can be held across it.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type Fd = libc::c_int;

pub fn last_err() -> io::Error {
    io::Error::last_os_error()
}

pub fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// "No such file or directory" without the ` (os error 2)` tail.
pub fn err_text(e: &io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

pub fn cstr(s: &OsStr) -> io::Result<CString> {
    CString::new(s.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interior NUL byte"))
}

// ---------------------------------------------------------------- fd I/O

pub fn write_all(fd: Fd, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n < 0 {
            let e = last_err();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        buf = &buf[n as usize..];
    }
    Ok(())
}

/// Positional read: never touches the shared file offset, so many readers are independent.
pub fn pread(fd: Fd, buf: &mut [u8], off: u64) -> io::Result<usize> {
    loop {
        let n = unsafe {
            libc::pread(
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                off as libc::off_t,
            )
        };
        if n < 0 {
            let e = last_err();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        return Ok(n as usize);
    }
}

pub fn read(fd: Fd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            let e = last_err();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        return Ok(n as usize);
    }
}

/// Read exactly `buf.len()` bytes. `Ok(false)` means EOF arrived early.
pub fn read_exact_raw(fd: Fd, buf: &mut [u8]) -> io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        let n = read(fd, &mut buf[got..])?;
        if n == 0 {
            return Ok(false);
        }
        got += n;
    }
    Ok(true)
}

pub fn close(fd: Fd) {
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
}

pub fn fstat(fd: Fd) -> io::Result<libc::stat> {
    let mut st = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut st) } < 0 {
        return Err(last_err());
    }
    Ok(st)
}

pub fn fchmod(fd: Fd, mode: libc::mode_t) -> io::Result<()> {
    if unsafe { libc::fchmod(fd, mode) } < 0 {
        return Err(last_err());
    }
    Ok(())
}

/// Duplicate `fd` so that the new descriptor is >= `min` and has FD_CLOEXEC **clear**
/// (`F_DUPFD` never copies CLOEXEC). Used to park inherited descriptors above 0..=2.
pub fn dupfd_above(fd: Fd, min: Fd) -> io::Result<Fd> {
    let n = unsafe { libc::fcntl(fd, libc::F_DUPFD, min) };
    if n < 0 {
        return Err(last_err());
    }
    Ok(n)
}

pub fn open_devnull() -> io::Result<Fd> {
    let path = c"/dev/null";
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        return Err(last_err());
    }
    Ok(fd)
}

pub fn pipe2(flags: libc::c_int) -> io::Result<(Fd, Fd)> {
    let mut fds = [0 as Fd; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(last_err());
    }
    for fd in fds {
        if flags & libc::O_CLOEXEC != 0
            && let Err(e) = set_cloexec(fd, true)
        {
            close(fds[0]);
            close(fds[1]);
            return Err(e);
        }
    }
    Ok((fds[0], fds[1]))
}

pub fn set_cloexec(fd: Fd, on: bool) -> io::Result<()> {
    let old = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if old < 0 {
        return Err(last_err());
    }
    let new = if on {
        old | libc::FD_CLOEXEC
    } else {
        old & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, new) } < 0 {
        return Err(last_err());
    }
    Ok(())
}

pub fn isatty(fd: Fd) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

// ---------------------------------------------------------------- *at operations

pub fn openat(dirfd: Fd, name: &OsStr, flags: libc::c_int, mode: libc::mode_t) -> io::Result<Fd> {
    let c = cstr(name)?;
    // `openat` is variadic, so the mode argument must be promoted to `c_uint` (Darwin's mode_t is
    // 16-bit; the varargs ABI promotes it).
    let fd = unsafe {
        libc::openat(
            dirfd,
            c.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(last_err());
    }
    Ok(fd)
}

pub fn mkdirat(dirfd: Fd, name: &OsStr, mode: libc::mode_t) -> io::Result<()> {
    let c = cstr(name)?;
    if unsafe { libc::mkdirat(dirfd, c.as_ptr(), mode) } < 0 {
        return Err(last_err());
    }
    Ok(())
}

pub fn unlinkat(dirfd: Fd, name: &OsStr, flags: libc::c_int) -> io::Result<()> {
    let c = cstr(name)?;
    if unsafe { libc::unlinkat(dirfd, c.as_ptr(), flags) } < 0 {
        return Err(last_err());
    }
    Ok(())
}

pub fn renameat(dirfd: Fd, from: &OsStr, to: &OsStr) -> io::Result<()> {
    let a = cstr(from)?;
    let b = cstr(to)?;
    if unsafe { libc::renameat(dirfd, a.as_ptr(), dirfd, b.as_ptr()) } < 0 {
        return Err(last_err());
    }
    Ok(())
}

/// `stat` a name relative to the held dirfd, without following a final symlink.
pub fn fstatat(dirfd: Fd, name: &OsStr) -> io::Result<libc::stat> {
    let c = cstr(name)?;
    let mut st = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstatat(dirfd, c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } < 0 {
        return Err(last_err());
    }
    Ok(st)
}

/// Whole-directory listing via the held dirfd (`dup` + `fdopendir`, so the fd stays ours).
pub fn readdir(dirfd: Fd) -> io::Result<Vec<OsString>> {
    let dup = unsafe { libc::dup(dirfd) };
    if dup < 0 {
        return Err(last_err());
    }
    let dir = unsafe { libc::fdopendir(dup) };
    if dir.is_null() {
        let e = last_err();
        close(dup);
        return Err(e);
    }
    let mut out = Vec::new();
    loop {
        let ent = unsafe { libc::readdir(dir) };
        if ent.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
        let b = name.to_bytes();
        if b != b"." && b != b".." {
            out.push(OsString::from_vec(b.to_vec()));
        }
    }
    unsafe { libc::closedir(dir) };
    Ok(out)
}

pub fn read_file_at(dirfd: Fd, name: &OsStr) -> io::Result<Vec<u8>> {
    let fd = openat(dirfd, name, libc::O_RDONLY | libc::O_NOFOLLOW, 0)?;
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match read(fd, &mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => {
                close(fd);
                return Err(e);
            }
        }
    }
    close(fd);
    Ok(out)
}

// ---------------------------------------------------------------- process control

pub enum Fork {
    Parent(libc::pid_t),
    Child,
}

pub fn fork() -> io::Result<Fork> {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(last_err());
    }
    if pid == 0 { Ok(Fork::Child) } else { Ok(Fork::Parent(pid)) }
}

pub fn getpid() -> libc::pid_t {
    unsafe { libc::getpid() }
}

pub fn geteuid() -> libc::uid_t {
    unsafe { libc::geteuid() }
}

pub fn setsid() -> io::Result<()> {
    if unsafe { libc::setsid() } < 0 {
        return Err(last_err());
    }
    Ok(())
}

pub fn setpgid(pid: libc::pid_t, pgid: libc::pid_t) -> io::Result<()> {
    if unsafe { libc::setpgid(pid, pgid) } < 0 {
        return Err(last_err());
    }
    Ok(())
}

pub fn dup2(from: Fd, to: Fd) -> io::Result<()> {
    if from == to {
        return Ok(());
    }
    if unsafe { libc::dup2(from, to) } < 0 {
        return Err(last_err());
    }
    Ok(())
}

/// Only returns on failure.
pub fn execvp(prog: &CStr, argv: &[CString]) -> io::Error {
    let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    unsafe { libc::execvp(prog.as_ptr(), ptrs.as_ptr()) };
    last_err()
}

pub fn exit_now(code: i32) -> ! {
    unsafe { libc::_exit(code) }
}

pub enum Status {
    Exited(i32),
    Signaled(i32),
}

pub fn waitpid(pid: libc::pid_t) -> io::Result<Status> {
    loop {
        let mut st: libc::c_int = 0;
        let r = unsafe { libc::waitpid(pid, &mut st, 0) };
        if r < 0 {
            let e = last_err();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if libc::WIFEXITED(st) {
            return Ok(Status::Exited(libc::WEXITSTATUS(st)));
        }
        if libc::WIFSIGNALED(st) {
            return Ok(Status::Signaled(libc::WTERMSIG(st)));
        }
        // Stopped/continued cannot occur: we never pass WUNTRACED/WCONTINUED.
    }
}

pub fn kill(pid: libc::pid_t, sig: libc::c_int) -> io::Result<()> {
    if unsafe { libc::kill(pid, sig) } < 0 {
        return Err(last_err());
    }
    Ok(())
}

/// Signal-0 liveness probe. `EPERM` means the process exists but is not ours.
pub fn kill_ok(pid: libc::pid_t) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    errno() == libc::EPERM
}

// ---------------------------------------------------------------- signals

pub fn sig_handler(sig: libc::c_int, handler: libc::sighandler_t) {
    unsafe { libc::signal(sig, handler) };
}

pub fn block_term_signals() -> io::Result<libc::sigset_t> {
    let mut new = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    let mut old = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    unsafe { libc::sigemptyset(&mut new) };
    for s in [
        libc::SIGHUP,
        libc::SIGINT,
        libc::SIGTERM,
        libc::SIGQUIT,
        libc::SIGTSTP,
        libc::SIGTTIN,
        libc::SIGTTOU,
    ] {
        unsafe { libc::sigaddset(&mut new, s) };
    }
    if unsafe { libc::sigprocmask(libc::SIG_BLOCK, &new, &mut old) } < 0 {
        return Err(last_err());
    }
    Ok(old)
}

pub fn restore_sigmask(old: &libc::sigset_t) {
    unsafe { libc::sigprocmask(libc::SIG_SETMASK, old, std::ptr::null_mut()) };
}

pub fn unblock_all() {
    let mut empty = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    unsafe { libc::sigemptyset(&mut empty) };
    unsafe { libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) };
}

// ---------------------------------------------------------------- time

pub fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}
