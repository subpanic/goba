//! Everything OS-specific, behind one interface (R68). No other module may `cfg` on the OS.

use std::io;
use std::path::PathBuf;

// ---------------------------------------------------------------- boot identity (R20)

#[cfg(target_os = "linux")]
pub fn boot_id() -> io::Result<String> {
    let raw = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    Ok(raw.trim().to_string())
}

#[cfg(target_os = "macos")]
pub fn boot_id() -> io::Result<String> {
    // `kern.boottime` is a struct timeval: fixed size on every macOS, no string encoding issues.
    let mut name = *b"kern.boottime\0";
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut len = std::mem::size_of::<libc::timeval>();
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_mut_ptr() as *const libc::c_char,
            &mut tv as *mut libc::timeval as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize != std::mem::size_of::<libc::timeval>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected kern.boottime size",
        ));
    }
    Ok(format!("{}.{}", tv.tv_sec, tv.tv_usec))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn boot_id() -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "unsupported platform",
    ))
}

// ---------------------------------------------------------------- process start time (R25)

/// An opaque, per-platform process start stamp. Only ever compared with another stamp produced by
/// the same platform, so the unit (Linux clock ticks, macOS microseconds) does not matter.
#[cfg(target_os = "linux")]
pub fn pid_start(pid: libc::pid_t) -> io::Result<u64> {
    let stat = std::fs::read(format!("/proc/{pid}/stat"))?;
    // `comm` (field 2) may contain spaces and parentheses; fields resume after the LAST ')'.
    let close = stat
        .iter()
        .rposition(|&b| b == b')')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed /proc stat"))?;
    let rest = &stat[close + 1..];
    let fields: Vec<&[u8]> = rest
        .split(|b: &u8| *b == b' ')
        .filter(|s| !s.is_empty())
        .collect();
    // After ')' the first field is `state` (field 3); `starttime` is field 22.
    let idx = 22usize - 3;
    let raw = fields
        .get(idx)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed /proc stat"))?;
    let s = std::str::from_utf8(raw)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 /proc stat"))?;
    s.parse::<u64>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad starttime"))
}

#[cfg(target_os = "macos")]
pub fn pid_start(pid: libc::pid_t) -> io::Result<u64> {
    #[repr(C)]
    struct ProcBsdInfo {
        pbi_flags: u32,
        pbi_status: u32,
        pbi_xstatus: u32,
        pbi_pid: u32,
        pbi_ppid: u32,
        pbi_uid: u32,
        pbi_gid: u32,
        pbi_ruid: u32,
        pbi_rgid: u32,
        pbi_svuid: u32,
        pbi_svgid: u32,
        rfu_1: u32,
        pbi_comm: [libc::c_char; 16],
        pbi_name: [libc::c_char; 32],
        pbi_nfiles: u32,
        pbi_pgid: u32,
        pbi_pjobc: u32,
        e_tdev: u32,
        e_tpgid: u32,
        pbi_nice: i32,
        pbi_start_tvsec: u64,
        pbi_start_tvusec: u64,
    }

    unsafe extern "C" {
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut libc::c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }

    const PROC_PIDTBSDINFO: libc::c_int = 3;

    let mut info = unsafe { std::mem::zeroed::<ProcBsdInfo>() };
    let want = std::mem::size_of::<ProcBsdInfo>() as libc::c_int;
    let got = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDTBSDINFO,
            0,
            &mut info as *mut ProcBsdInfo as *mut libc::c_void,
            want,
        )
    };
    if got != want {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "proc_pidinfo failed",
        ));
    }
    Ok(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn pid_start(pid: libc::pid_t) -> io::Result<u64> {
    let _ = pid;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "unsupported platform",
    ))
}

// ---------------------------------------------------------------- resolved executable

#[cfg(target_os = "linux")]
pub fn exe_path(pid: libc::pid_t) -> io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
}

#[cfg(target_os = "macos")]
pub fn exe_path(pid: libc::pid_t) -> io::Result<PathBuf> {
    unsafe extern "C" {
        fn proc_pidpath(
            pid: libc::c_int,
            buffer: *mut libc::c_void,
            buffersize: u32,
        ) -> libc::c_int;
    }
    let mut buf = [0u8; libc::PATH_MAX as usize];
    let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if n <= 0 {
        return Err(io::Error::new(io::ErrorKind::NotFound, "proc_pidpath failed"));
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Ok(PathBuf::from(std::ffi::OsString::from_vec(buf[..end].to_vec())))
}

#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStringExt;

#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStringExt;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn exe_path(pid: libc::pid_t) -> io::Result<PathBuf> {
    let _ = pid;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "unsupported platform",
    ))
}
