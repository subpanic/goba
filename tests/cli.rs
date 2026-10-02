//! Integration tests for the acceptance criteria in REQUIREMENTS.md §17 (A1–A21).
//! Everything here drives the real binary through a real filesystem in a private GOBA_DIR.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_goba")
}

struct Env {
    root: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Env {
        let mut root = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("goba-it-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        Env { root }
    }

    fn store(&self) -> PathBuf {
        self.root.join("store")
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(bin());
        c.env("GOBA_DIR", self.store());
        // Sessions inherit this as their cwd, so redirections and relative paths stay in the
        // scratch tree instead of the checkout.
        c.current_dir(&self.root);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    /// Run and require success, returning trimmed stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "goba {args:?} failed: rc={:?} stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    }

    fn code(&self, args: &[&str]) -> i32 {
        self.run(args).status.code().unwrap()
    }

    /// Spawn and parse the `NUM SID` line (R48/R53).
    fn spawn(&self, args: &[&str]) -> (u64, String) {
        self.spawn_env(&[], args)
    }

    fn spawn_env(&self, extra: &[(&str, &str)], args: &[&str]) -> (u64, String) {
        let mut c = self.cmd();
        for (k, v) in extra {
            c.env(k, v);
        }
        let out = c.args(args).output().unwrap();
        assert!(
            out.status.success(),
            "goba {args:?} failed: {:?} {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
        let mut it = stdout.split(' ');
        let num: u64 = it.next().expect("numeric id").parse().expect("numeric id");
        let sid = it.next().expect("string id").to_string();
        assert_eq!(it.next(), None, "stdout must be exactly one line: {stdout:?}");
        (num, sid)
    }

    fn json(&self) -> String {
        self.ok(&["-l", "--json"])
    }

    /// One session object out of `-l --json`, by numeric id.
    fn obj(&self, num: u64) -> Option<String> {
        let j = self.json();
        let needle = format!("\"num\":{num},");
        let i = j.find(&needle)?;
        let start = j[..i].rfind('{').unwrap();
        let end = i + j[i..].find('}').unwrap();
        Some(j[start..=end].to_string())
    }

    fn state(&self, num: u64) -> Option<String> {
        jstr(&self.obj(num)?, "state")
    }

    fn sdir(&self) -> PathBuf {
        self.store().join("s")
    }

    /// The session directory for a numeric id: its name is `<num>-<sid>`.
    fn dir_of(&self, num: u64) -> PathBuf {
        let prefix = format!("{num}-");
        for e in std::fs::read_dir(self.sdir()).unwrap().flatten() {
            if e.file_name().to_string_lossy().starts_with(&prefix) {
                return e.path();
            }
        }
        panic!("no session directory for {num}");
    }

    fn has_dir(&self, num: u64) -> bool {
        let prefix = format!("{num}-");
        std::fs::read_dir(self.sdir())
            .map(|rd| {
                rd.flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            })
            .unwrap_or(false)
    }

    /// Place a session directory directly, as fixtures need.
    fn craft(&self, name: &str, meta_body: &str) -> PathBuf {
        let dir = self.sdir().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta"), meta_body).unwrap();
        std::fs::write(dir.join("log"), b"").unwrap();
        dir
    }

    fn boot_stamp(&self) -> String {
        std::fs::read_to_string(self.store().join("boot")).unwrap()
    }

    fn log(&self, num: u64) -> Vec<u8> {
        std::fs::read(self.dir_of(num).join("log")).unwrap()
    }

    fn meta_path(&self, num: u64) -> PathBuf {
        self.dir_of(num).join("meta")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn jstr(obj: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let i = obj.find(&needle)? + needle.len();
    let j = i + obj[i..].find('"')?;
    Some(obj[i..j].to_string())
}

fn wait_for<F: Fn() -> bool>(f: F, ms: u64) -> bool {
    let start = Instant::now();
    loop {
        if f() {
            return true;
        }
        if start.elapsed().as_millis() >= ms as u128 {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ================================================================ A4, R48, R53

#[test]
fn a4_spawn_is_one_line_and_leaves_a_complete_session() {
    let env = Env::new("a4");
    let out = env.run(&["sleep", "30"]);
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.trim_end();
    assert!(!line.contains('\n'), "stdout must be one line: {s:?}");
    let (num, sid) = {
        let mut it = line.split(' ');
        (
            it.next().unwrap().parse::<u64>().unwrap(),
            it.next().unwrap().to_string(),
        )
    };
    // R27: the string id is the command, not a random token.
    assert_eq!(sid, "sleep");

    // The record and the log exist the moment the id is printed.
    assert!(env.meta_path(num).exists());
    assert!(env.dir_of(num).join("log").exists());
    assert_eq!(env.state(num).as_deref(), Some("running"));

    env.ok(&["-k", &num.to_string()]);
}

// ================================================================ R27

#[test]
fn sid_is_the_command_name_with_numeric_suffixes() {
    let env = Env::new("sid");
    let (a, sa) = env.spawn(&["sleep", "30"]);
    let (_b, sb) = env.spawn(&["sleep", "30"]);
    let (_c, sc) = env.spawn(&["sleep", "30"]);
    assert_eq!(
        (sa.as_str(), sb.as_str(), sc.as_str()),
        ("sleep", "sleep-2", "sleep-3"),
        "collisions take the next free integer suffix"
    );

    // A different command gets its own name, unaffected by the sleep sessions.
    let (_d, sd) = env.spawn(&["cat", "/dev/null"]);
    assert_eq!(sd, "cat");

    // The name is the basename, whatever path it was invoked by.
    let (_e, se) = env.spawn(&["/bin/sleep", "30"]);
    assert_eq!(se, "sleep-4");

    // Unprintable characters become '-' so the id stays typeable, unquoted.
    let odd = env.root.join("my  weird  tool");
    std::fs::copy("/bin/sleep", &odd).unwrap();
    std::fs::set_permissions(
        &odd,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let (_f, sf) = env.spawn(&[odd.to_str().unwrap(), "30"]);
    assert_eq!(sf, "my-weird-tool");

    // An exact name beats a prefix; an inexact prefix must be unambiguous.
    assert_eq!(env.code(&["-v", "sleep"]), 0);
    assert_eq!(env.code(&["-v", "sle"]), 2); // sleep, sleep-2, … are all candidates
    assert_eq!(env.code(&["-v", "cat"]), 0);
    assert_eq!(env.code(&["-v", "ca"]), 0); // unique prefix still resolves

    // The numeric id remains an independent handle: 1 is `sleep` even though
    // the name `sleep` is also taken.
    assert_eq!(env.code(&["-v", &a.to_string()]), 0);

    // Removing a name frees it for reuse.
    env.ok(&["-k", "sleep-2"]);
    env.ok(&["-r", "sleep-2"]);
    let (_g, sg) = env.spawn(&["sleep", "30"]);
    assert_eq!(sg, "sleep-2");

    // Removing frees its directory too.
    assert!(!env.has_dir(2));
    for (n, _) in [(1u64, "x"), (3, "x"), (4, "x"), (5, "x"), (6, "x"), (7, "x")] {
        let id = n.to_string();
        let _ = env.run(&["-k", &id]);
        let _ = env.run(&["-r", &id]);
    }
    assert!(env.json().is_empty() || env.json() == "[]");
}

#[test]
fn concurrent_same_command_spawns_still_get_distinct_names() {
    let env = Env::new("sidrace");
    let children: Vec<_> = (0..24)
        .map(|_| {
            env.cmd()
                .args(["sleep", "5"])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut sids = Vec::new();
    for c in children {
        let out = c.wait_with_output().unwrap();
        assert!(out.status.success());
        let s = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
        sids.push(s.split(' ').nth(1).unwrap().to_string());
    }
    sids.sort();
    let n = sids.len();
    sids.dedup();
    assert_eq!(sids.len(), n, "two concurrent sessions shared a name");
    assert_eq!(sids[0], "sleep");
    // 24 sessions of one command occupy `sleep` and the suffixes 2..=24.
    assert!(sids.iter().any(|s| s == "sleep-2"));
    assert!(sids.iter().any(|s| s == "sleep-24"), "{sids:?}");

    let alive: Vec<u64> = env
        .ok(&["-la"])
        .lines()
        .map(|l| l.split_whitespace().next().unwrap().parse().unwrap())
        .collect();
    for id in alive {
        env.ok(&["-k", &id.to_string()]);
    }
}

#[test]
fn r53_stdout_is_empty_on_failure() {
    let env = Env::new("r53");
    let out = env.run(&["nosuchcommand"]);
    assert_eq!(out.status.code(), Some(5));
    assert!(out.stdout.is_empty(), "stdout must stay clean on failure");
    assert!(!out.stderr.is_empty());
}

// ================================================================ A14 / R50

#[test]
fn a14_exec_failure_leaves_nothing_behind() {
    let env = Env::new("a14");
    let out = env.run(&["definitely-not-a-real-binary-xyz"]);
    assert_eq!(out.status.code(), Some(5));
    let msg = String::from_utf8_lossy(&out.stderr);
    assert!(msg.contains("No such file or directory"), "{msg}");

    // No session directory survives.
    let s = env.store().join("s");
    let left = std::fs::read_dir(&s)
        .map(|rd| rd.filter_map(|e| e.ok()).count())
        .unwrap_or(0);
    assert_eq!(left, 0, "an exec failure must not leave a session record");

    // And the store is still usable afterwards.
    let (num, _) = env.spawn(&["sleep", "30"]);
    assert_eq!(env.state(num).as_deref(), Some("running"));
    env.ok(&["-k", &num.to_string()]);
}

#[test]
fn a14_exec_permission_denied_is_synchronous() {
    let env = Env::new("a14b");
    let f = env.root.join("not-executable");
    std::fs::write(&f, b"#!/bin/sh\n").unwrap(); // no +x
    let out = env.run(&[f.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(5));
    assert!(out.stdout.is_empty());
}

// ================================================================ A5

#[test]
fn a5_stdout_and_stderr_interleave_in_write_order() {
    let env = Env::new("a5");
    let (num, _) = env.spawn(&["sh", "-c", "echo out1; echo err1 1>&2; echo out2"]);
    assert!(wait_for(|| env.state(num).as_deref() == Some("exited"), 5000));
    assert_eq!(env.log(num), b"out1\nerr1\nout2\n");
}

// ================================================================ A6 / R1 / R6

#[test]
fn a6_job_survives_its_invoker_group_and_terminal_hangup() {
    let env = Env::new("a6");
    // The command reports any SIGHUP it receives: proof it is not in the invoker's group.
    let child = env
        .cmd()
        .args(["sh", "-c", "trap 'echo HUPPED' HUP; sleep 20"])
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let front_pgid = child.id() as i32;
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let num: u64 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();

    // Rip the invoker's process group away. The group id still exists for as long as any member
    // lives, so if the job *had* been left in it, this would reach it.
    unsafe {
        libc::kill(-front_pgid, libc::SIGHUP);
        libc::kill(-front_pgid, libc::SIGINT);
        libc::kill(-front_pgid, libc::SIGKILL);
    }

    assert_eq!(
        env.state(num).as_deref(),
        Some("running"),
        "the detached command must survive the invoker's death"
    );
    // 200 ms is far longer than it would take a wrong implementation to die.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(env.state(num).as_deref(), Some("running"));
    assert!(
        !env.log(num).windows(6).any(|w| w == b"HUPPED"),
        "the command must not share the invoker's process group"
    );

    env.ok(&["-k", &num.to_string()]);
}

// ================================================================ A8 / R24

#[test]
fn a8_exit_status_fidelity() {
    let env = Env::new("a8");
    let cases: [(&[&str], Option<i64>, i64); 4] = [
        (&["sh", "-c", "exit 0"], Some(0), 0),
        (&["sh", "-c", "exit 7"], Some(7), 0),
        (&["sh", "-c", "kill -KILL $$"], None, 9),
        (&["sh", "-c", "kill -TERM $$"], None, 15),
    ];
    for (argv, code, sig) in cases {
        let (num, _) = env.spawn(argv);
        assert!(
            wait_for(
                || matches!(env.state(num).as_deref(), Some("exited") | Some("killed")),
                5000
            ),
            "{argv:?} never finished"
        );
        let obj = env.obj(num).unwrap();
        if sig == 0 {
            assert_eq!(env.state(num).as_deref(), Some("exited"), "{argv:?}");
            assert_eq!(jnum(&obj, "exit_code"), code, "{argv:?}");
            assert_eq!(jnum(&obj, "signal"), None, "{argv:?}");
        } else {
            assert_eq!(env.state(num).as_deref(), Some("killed"), "{argv:?}");
            assert_eq!(jnum(&obj, "signal"), Some(sig), "{argv:?}");
            assert_eq!(jnum(&obj, "exit_code"), None, "{argv:?}");
        }
    }
}

fn jnum(obj: &str, key: &str) -> Option<i64> {
    let needle = format!("\"{key}\":");
    let i = obj.find(&needle)? + needle.len();
    let rest = &obj[i..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '-'))
        .unwrap_or(rest.len());
    let v = &rest[..end];
    if v == "null" || v.is_empty() {
        None
    } else {
        v.parse().ok()
    }
}

// ================================================================ A9 / A10 / R42 / R43

#[test]
fn a9_kill_takes_the_whole_process_group() {
    let env = Env::new("a9");
    let (num, _) = env.spawn(&["sh", "-c", "sleep 300 & sleep 300"]);
    std::thread::sleep(Duration::from_millis(250));
    let before = count_sleep_300();
    assert!(before >= 2, "expected descendants to be running");

    env.ok(&["-k", &num.to_string()]);
    assert!(
        wait_for(|| count_sleep_300() < before, 3000),
        "descendants survived the kill"
    );
}

fn count_sleep_300() -> usize {
    let out = Command::new("sh")
        .arg("-c")
        .arg("ps -o command= -ax | grep -c '[s]leep 300'")
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

#[test]
fn a10_kill_escalates_to_sigkill() {
    let env = Env::new("a10");
    // The whole group must survive SIGTERM for the escalation to be what actually kills it:
    // trapping in the shell alone would not protect its `sleep` child, and the session would
    // legitimately end on SIGTERM instead. The command announces readiness *after* installing the
    // trap, so this does not depend on how long `sh` takes to get there under load.
    let (num, _) = env.spawn(&[
        "sh",
        "-c",
        "trap '' TERM; echo ready; while true; do sleep 1; done",
    ]);
    assert!(
        wait_for(|| env.log(num).windows(5).any(|w| w == b"ready"), 5000),
        "command never reported readiness"
    );
    let t0 = Instant::now();
    env.ok(&["-k", &num.to_string(), "-t", "300ms"]);
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert_eq!(env.state(num).as_deref(), Some("killed"));
    assert_eq!(jnum(&env.obj(num).unwrap(), "signal"), Some(9));
}

// ================================================================ A11 / R37 / R38

#[test]
fn a11_follow_streams_then_exits_and_dead_follow_is_immediate() {
    let env = Env::new("a11");
    let (num, _) = env.spawn(&["sh", "-c", "echo early; sleep 0.4; echo late"]);
    std::thread::sleep(Duration::from_millis(150));

    let t0 = Instant::now();
    let out = env.run(&["-f", &num.to_string()]);
    assert!(out.status.success());
    let elapsed = t0.elapsed();
    assert_eq!(out.stdout, b"early\nlate\n");
    assert!(elapsed >= Duration::from_millis(200), "should have followed");
    assert!(elapsed < Duration::from_secs(5));

    // Following a finished session must print history and return at once.
    let t1 = Instant::now();
    let out = env.run(&["-f", &num.to_string()]);
    assert!(out.status.success());
    assert_eq!(out.stdout, b"early\nlate\n");
    assert!(t1.elapsed() < Duration::from_millis(500), "must not hang");
}

// ================================================================ A12 / R57 / R58

#[test]
fn a12_remove_refuses_while_running_then_succeeds() {
    let env = Env::new("a12");
    let (num, _) = env.spawn(&["sleep", "30"]);
    let id = num.to_string();
    assert_eq!(env.code(&["-r", &id]), 7);
    assert_eq!(env.state(num).as_deref(), Some("running"));
    assert_eq!(env.code(&["-r", "999"]), 3);

    env.ok(&["-k", &id]);
    assert_eq!(env.code(&["-r", &id]), 0);
    assert!(env.obj(num).is_none(), "removed session must disappear");
    assert!(!env.has_dir(num));
    // Removing again is a not-found, not a crash.
    assert_eq!(env.code(&["-r", &id]), 3);
}

// ================================================================ A13 / R20 / R21

#[test]
fn a13_sessions_from_a_previous_boot_are_reclaimed() {
    let env = Env::new("a13");
    let (num, _) = env.spawn(&["sleep", "30"]);
    env.ok(&["-k", &num.to_string()]);
    assert!(env.obj(num).is_some());

    // Simulate a reboot by restamping the session with a different boot identity.
    let p = env.meta_path(num);
    let text = std::fs::read_to_string(&p).unwrap();
    let restamped: String = text
        .lines()
        .map(|l| {
            if l.starts_with("boot=") {
                "boot=00000000-0000-0000-0000-000000000000".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&p, restamped).unwrap();

    assert_eq!(env.json(), "[]", "a previous boot's session must not be listed");
    assert!(
        !env.has_dir(num),
        "stale session must be reclaimed"
    );
}

// ================================================================ A15 / R25

#[test]
fn a15_reused_pid_is_never_reported_as_running() {
    let env = Env::new("a15");
    // Establish the store (and its boot stamp).
    let (num, _) = env.spawn(&["sleep", "30"]);
    env.ok(&["-k", &num.to_string()]);
    let boot = env.boot_stamp();

    // A record whose pid is alive *right now* (our own), but whose start stamp is wrong: this is
    // exactly what PID reuse looks like.
    let fake = num + 1;
    env.craft(
        &format!("{fake}-zzzzz9"),
        &format!(
            "schema=1\nboot={boot}\nstate=running\n\
             pid={}\npgid={}\nspid=0\npid_start=1\nstarted_at=1\nended_at=0\n\
             exit_code=-1\nsignal=0\ncwd=/\nexe=/bin/sleep\nargv=sleep%2030\n",
            std::process::id(),
            std::process::id()
        ),
    );

    let obj = env.obj(fake).expect("crafted session is listed");
    assert_eq!(jstr(&obj, "sid").as_deref(), Some("zzzzz9"));
    assert_eq!(
        jstr(&obj, "state").as_deref(),
        Some("lost"),
        "a reused pid must not read as running: {obj}"
    );
    assert_eq!(env.code(&["-k", &fake.to_string()]), 3);
}

// ================================================================ R13 / R13b

#[test]
fn c_flag_gives_real_shell_syntax() {
    let env = Env::new("shellc");
    let sh = [("SHELL", "/bin/sh")];

    // A pipeline, which a direct exec could never express.
    let (num, sid) = env.spawn_env(&sh, &["-c", "echo hello | tr a-z A-Z"]);
    assert_eq!(sid, "echo", "the session is named after the script's first word");
    assert!(wait_for(|| env.state(num).as_deref() == Some("exited"), 5000));
    assert_eq!(env.log(num), b"HELLO\n");

    // `&&`, a builtin that only exists in a shell, and an exit status.
    let (num, _) = env.spawn_env(&sh, &["-c", "cd / && pwd && exit 7"]);
    assert!(wait_for(|| env.state(num).as_deref() == Some("exited"), 5000));
    assert_eq!(env.log(num), b"/\n");
    assert_eq!(jnum(&env.obj(num).unwrap(), "exit_code"), Some(7));

    // A redirection belongs to the *session*, not to goba's own stdout.
    let target = env.root.join("redirect-target");
    let (num, _) = env.spawn_env(
        &sh,
        &["-c", &format!("echo captured > {} ; cat {}", target.display(), target.display())],
    );
    assert!(wait_for(|| env.state(num).as_deref() == Some("exited"), 5000));
    assert_eq!(env.log(num), b"captured\n");

    // Variables and quoting inside the string survive, because the shell parses it.
    let (num, _) = env.spawn_env(&sh, &["-c", "X='a  b'; printf '[%s]\\n' \"$X\""]);
    assert!(wait_for(|| env.state(num).as_deref() == Some("exited"), 5000));
    assert_eq!(env.log(num), b"[a  b]\n");

    // A leading VAR=value assignment is not the name, and it does reach the command.
    let (num, sid) = env.spawn_env(&sh, &["-c", "FOO=bar sleep 20"]);
    assert_eq!(sid, "sleep");
    env.ok(&["-k", &num.to_string()]);
}

#[test]
fn c_flag_falls_back_when_shell_is_unset() {
    let env = Env::new("noshell");
    let (num, _) = env.spawn_env(&[("SHELL", "")], &["-c", "echo fallback"]);
    assert!(wait_for(|| env.state(num).as_deref() == Some("exited"), 5000));
    assert_eq!(env.log(num), b"fallback\n");
}

/// The invariant `-c` exists to protect: goba is handed argv, never a string, so it must never
/// re-parse one. Anything the caller already quoted stays exactly as the caller meant it.
#[test]
fn argv_is_never_reparsed() {
    let env = Env::new("noreparse");
    let (num, _) = env.spawn(&["echo", "a  b"]);
    assert!(wait_for(|| env.state(num).as_deref() == Some("exited"), 5000));
    assert_eq!(env.log(num), b"a  b\n", "two spaces must survive as one argument");

    let (num, _) = env.spawn(&["echo", "a|b", "$HOME", "&&"]);
    assert!(wait_for(|| env.state(num).as_deref() == Some("exited"), 5000));
    assert_eq!(
        env.log(num),
        b"a|b $HOME &&\n",
        "metacharacters in argv are literal text, not syntax"
    );
}

// ================================================================ A16 / R17

#[test]
fn a16_store_is_validated() {
    // Symlinked store: refused.
    let env = Env::new("a16a");
    let real = env.root.join("real");
    std::fs::create_dir_all(&real).unwrap();
    let link = env.root.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let out = Command::new(bin()).env("GOBA_DIR", &link).arg("-l").output().unwrap();
    assert_eq!(out.status.code(), Some(4), "symlinked store must be refused");

    // Group/world-accessible store: refused.
    let env = Env::new("a16b");
    std::fs::create_dir_all(env.store()).unwrap();
    std::fs::set_permissions(
        env.store(),
        std::os::unix::fs::PermissionsExt::from_mode(0o777),
    )
    .unwrap();
    let out = Command::new(bin())
        .env("GOBA_DIR", env.store())
        .arg("-l")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "world-writable store must be refused");
}

// ================================================================ A17 / R54

#[test]
fn a17_parallel_spawns_get_distinct_ids() {
    let env = Env::new("a17");
    let children: Vec<_> = (0..50)
        .map(|_| {
            env.cmd()
                .args(["true"])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut ids = Vec::new();
    for c in children {
        let out = c.wait_with_output().unwrap();
        assert!(out.status.success());
        let s = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
        ids.push(s.split(' ').next().unwrap().parse::<u64>().unwrap());
    }
    ids.sort_unstable();
    let before = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), before, "ids collided under concurrency");
    assert!(env.obj(1).is_some());
    assert!(wait_for(
        || env.json().matches("\"num\":").count() == 50,
        5000
    ));
}

// ================================================================ A2 / R28

#[test]
fn a2_id_resolution() {
    let env = Env::new("a2");
    let (a, sid_a) = env.spawn(&["sh", "-c", "echo a"]);
    let (b, sid_b) = env.spawn(&["sh", "-c", "echo b"]);
    // Both must have finished before their logs can be asserted on.
    assert!(wait_for(
        || env.state(a).as_deref() == Some("exited") && env.state(b).as_deref() == Some("exited"),
        5000
    ));

    // numeric
    assert_eq!(env.ok(&["-v", &a.to_string()]), "a");
    // full string id
    assert_eq!(env.ok(&["-v", &sid_a]), "a");
    assert_eq!(env.ok(&["-v", &sid_b]), "b");
    // unique prefix
    let prefix = &sid_b[..4];
    if !sid_a.starts_with(prefix) {
        assert_eq!(env.ok(&["-v", prefix]), "b");
    }
    // unknown
    assert_eq!(env.code(&["-v", "qqqq"]), 3);
    // `s` is a prefix of both `sh` and `sh-2`, and is not itself a session.
    assert_eq!(env.code(&["-v", "s"]), 2);
    // Two crafted sessions sharing an opening stretch behave the same way.
    let boot = env.boot_stamp();
    for (n, sid) in [(b + 1, "zeta-one"), (b + 2, "zeta-two")] {
        env.craft(
            &format!("{n}-{sid}"),
            &format!(
                "schema=1\nboot={boot}\nstate=exited\npid=0\npgid=0\n\
                 spid=0\npid_start=0\nstarted_at=1\nended_at=2\nexit_code=0\nsignal=0\n\
                 cwd=/\nexe=/bin/true\nargv=true\n"
            ),
        );
    }
    let out = env.run(&["-v", "zeta"]);
    assert_eq!(out.status.code(), Some(2), "prefix must be reported ambiguous");
    assert!(String::from_utf8_lossy(&out.stderr).contains("ambiguous"));
    // …and an exact name still wins over its own prefix.
    assert_eq!(env.code(&["-v", "zeta-one"]), 0);
}

// ================================================================ A3 / R36

#[test]
fn a3_tail_lines_are_binary_safe() {
    let env = Env::new("a3");
    let fixture = env.root.join("fixture");
    std::fs::write(&fixture, b"a\0b\nc\nd").unwrap(); // NUL byte, no trailing newline
    let (num, _) = env.spawn(&["cat", fixture.to_str().unwrap()]);
    assert!(wait_for(|| env.state(num).as_deref() == Some("exited"), 5000));
    let id = num.to_string();

    assert_eq!(env.run(&["-v", &id]).stdout, b"a\0b\nc\nd");
    assert_eq!(env.run(&["-v", &id, "-n", "2"]).stdout, b"c\nd");
    assert_eq!(env.run(&["-v", &id, "-n", "1"]).stdout, b"d");
    assert_eq!(env.run(&["-v", &id, "-n", "3"]).stdout, b"a\0b\nc\nd");
    assert_eq!(env.run(&["-v", &id, "-n", "0"]).stdout, b"");
    assert_eq!(env.run(&["-v", &id, "-n", "99"]).stdout, b"a\0b\nc\nd");

    // A trailing newline does not create an extra empty line.
    let f2 = env.root.join("f2");
    std::fs::write(&f2, b"x\ny\n").unwrap();
    let (n2, _) = env.spawn(&["cat", f2.to_str().unwrap()]);
    assert!(wait_for(|| env.state(n2).as_deref() == Some("exited"), 5000));
    assert_eq!(env.run(&["-v", &n2.to_string(), "-n", "1"]).stdout, b"y\n");

    assert_eq!(env.code(&["-v", &id, "-n", "-1"]), 1);
}

// ================================================================ R7c

#[test]
fn detached_tree_never_holds_the_caller_stdio() {
    let env = Env::new("fds");
    // `Output` reads the captured pipes until EOF, so this blocks for the full 8 s if the
    // supervisor inherited our stdout/stderr — which is what a script or CI job would see.
    let t0 = Instant::now();
    let out = env.run(&["sleep", "8"]);
    assert!(out.status.success());
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "the caller's stdio was held open for {elapsed:?}"
    );
    let num: u64 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(env.state(num).as_deref(), Some("running"));
    env.ok(&["-k", &num.to_string()]);
}

// ================================================================ listing / R30-R33

#[test]
fn listing_filters_and_empty_store() {
    let env = Env::new("list");
    // Empty store: nothing on stdout, exit 0 (R33); --json is a valid empty array.
    assert_eq!(env.ok(&["-l"]), "");
    assert_eq!(env.json(), "[]");

    let (dead, _) = env.spawn(&["true"]);
    let (live, _) = env.spawn(&["sleep", "30"]);
    assert!(wait_for(|| env.state(dead).as_deref() == Some("exited"), 5000));

    let alive: Vec<u64> = env
        .ok(&["-la"])
        .lines()
        .map(|l| l.split_whitespace().next().unwrap().parse().unwrap())
        .collect();
    assert_eq!(alive, vec![live]);

    let deads: Vec<u64> = env
        .ok(&["-ld"])
        .lines()
        .map(|l| l.split_whitespace().next().unwrap().parse().unwrap())
        .collect();
    assert_eq!(deads, vec![dead]);

    // Default lists everything, ascending by id.
    let all: Vec<u64> = env
        .ok(&["-l"])
        .lines()
        .map(|l| l.split_whitespace().next().unwrap().parse().unwrap())
        .collect();
    assert_eq!(all, vec![dead, live]);

    assert_eq!(env.code(&["-l", "-a", "-d"]), 1);
    env.ok(&["-k", &live.to_string()]);
}

// ================================================================ argument passthrough

#[test]
fn command_line_is_verbatim() {
    let env = Env::new("verbatim");
    let (num, _) = env.spawn(&["echo", "-v", "-l", "--", "hi there"]);
    assert!(wait_for(|| env.state(num).as_deref() == Some("exited"), 5000));
    assert_eq!(env.log(num), b"-v -l -- hi there\n");
    let obj = env.obj(num).unwrap();
    assert!(obj.contains("\"-v\""), "{obj}");
}

#[test]
fn spawn_needs_a_command() {
    let env = Env::new("nocmd");
    assert_eq!(env.code(&[]), 1);
    assert_eq!(env.code(&["-v"]), 1);
    assert_eq!(env.code(&["--bogus"]), 1);
}

#[test]
fn help_and_version() {
    let env = Env::new("help");
    let out = env.run(&["--help"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("usage:"));
    let out = env.run(&["--version"]);
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("goba "));
}

// ================================================================ R62

#[test]
fn r62_spawn_latency_is_small() {
    let env = Env::new("lat");
    // Warm the store so we measure steady-state spawning, not first-run setup.
    let (warm, _) = env.spawn(&["true"]);
    assert!(wait_for(|| env.state(warm).as_deref() == Some("exited"), 5000));

    let n = 20;
    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let t0 = Instant::now();
        let out = env.run(&["true"]);
        samples.push(t0.elapsed());
        assert!(out.status.success());
    }
    // The median, not the mean: this suite runs its cases in parallel and a single scheduling
    // spike from a neighbouring test must not fail the gate. Debug builds are ~10x slower than
    // release, so this is deliberately loose; §13's release numbers are the binding ones.
    samples.sort();
    let median = samples[n / 2];
    assert!(
        median < Duration::from_millis(100),
        "spawn took {median:?} per invocation in a debug build"
    );
}

// ================================================================ R38 / supervisor loss

#[test]
fn following_stops_when_the_owner_disappears() {
    let env = Env::new("lost");
    let (num, _) = env.spawn(&["sh", "-c", "echo hi; sleep 0.4"]);
    std::thread::sleep(Duration::from_millis(100));

    // Kill the supervisor out from under the command, so no terminal record will ever be written.
    let meta = std::fs::read_to_string(env.meta_path(num)).unwrap();
    let spid: i32 = meta
        .lines()
        .find_map(|l| l.strip_prefix("spid="))
        .expect("spid recorded")
        .parse()
        .unwrap();
    unsafe { libc::kill(spid, libc::SIGKILL) };

    // R38: following must end once the owner is gone, not hang forever.
    let t0 = Instant::now();
    let out = env.run(&["-f", &num.to_string()]);
    assert!(out.status.success());
    assert_eq!(out.stdout, b"hi\n");
    assert!(
        t0.elapsed() < Duration::from_secs(4),
        "follow hung after the supervisor died: {:?}",
        t0.elapsed()
    );
}
