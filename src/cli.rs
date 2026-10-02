//! Argument parsing (R8–R13) and usage text.
//!
//! The whole point of the surface is that `goba` adds no wrapper syntax: option parsing stops at
//! the first non-option argument, so the command is passed through byte-for-byte.

use std::ffi::OsString;

use crate::fail::Fail;

#[derive(Debug, PartialEq)]
pub enum Filter {
    All,
    Alive,
    Dead,
}

#[derive(Debug)]
pub enum Cmd {
    Help,
    Version,
    Spawn {
        argv: Vec<OsString>,
        quiet: bool,
        /// Set when the command came from `-c <string>`: the raw string, for naming and for
        /// reporting. The argv already contains `$SHELL -c <string>`.
        shell_script: Option<OsString>,
    },
    View {
        id: String,
        follow: bool,
        lines: Option<u64>,
    },
    Kill {
        id: String,
        timeout_ms: u64,
    },
    List {
        filter: Filter,
        json: bool,
    },
    Remove {
        id: String,
    },
}

#[derive(Debug, PartialEq)]
enum Mode {
    View,
    Kill,
    List,
    Remove,
}

pub const USAGE: &str = "\
usage:
  goba [--] <command> [args...]     start a command in the background
  goba -c <string>                  start a shell command (pipes, &&, redirects)
  goba -v <id> [-n N]               print a session's captured output
  goba -f <id> [-n N]               follow a session's output until it ends
  goba -k <id> [-t DUR]             terminate a session (SIGTERM, then SIGKILL)
  goba -l [-a|-d] [--json]          list sessions
  goba -r <id>                      forget a session and delete its log

options:
  -c, --command STR                 run STR with $SHELL -c (falls back to /bin/sh)
  -v, --view        -f, --follow     -n, --lines N      -k, --kill
  -t, --timeout DUR -l, --list       -a, --alive        -d, --dead
  -r, --remove      -q, --quiet      --json             -h, --help
      --version

An id is a number, a command-derived name, or an unambiguous prefix of one.
Sessions are retained until the machine reboots.";

fn usage_err(msg: impl Into<String>) -> Fail {
    Fail::Usage(msg.into())
}

fn parse_u64(s: &str, what: &str) -> Result<u64, Fail> {
    s.parse::<u64>()
        .map_err(|_| usage_err(format!("{what}: not a number: '{s}'")))
}

/// `5`, `5s`, `500ms`, `2m`, `1h`.
fn parse_duration(s: &str) -> Result<u64, Fail> {
    let bad = || usage_err(format!("--timeout: not a duration: '{s}'"));
    let (num, mult) = if let Some(v) = s.strip_suffix("ms") {
        (v, 1u64)
    } else if let Some(v) = s.strip_suffix('s') {
        (v, 1000)
    } else if let Some(v) = s.strip_suffix('m') {
        (v, 60_000)
    } else if let Some(v) = s.strip_suffix('h') {
        (v, 3_600_000)
    } else {
        (s, 1000)
    };
    let n: u64 = num.parse().map_err(|_| bad())?;
    n.checked_mul(mult).ok_or_else(bad)
}

pub fn parse(args: &[OsString]) -> Result<Cmd, Fail> {
    let mut mode: Option<Mode> = None;
    let mut follow = false;
    let mut quiet = false;
    let mut json = false;
    let mut alive = false;
    let mut dead = false;
    let mut lines: Option<u64> = None;
    let mut timeout_ms: u64 = 5000;
    let mut shell_script: Option<OsString> = None;
    let mut positional: Vec<OsString> = Vec::new();
    let mut spawn_at: Option<usize> = None;

    let set_mode = |mode: &mut Option<Mode>, m: Mode| -> Result<(), Fail> {
        match mode {
            Some(cur) if *cur != m => Err(usage_err(
                "conflicting modes: pick one of -v/-f, -k, -l, -r",
            )),
            _ => {
                *mode = Some(m);
                Ok(())
            }
        }
    };

    let mut i = 1usize;
    while i < args.len() {
        let tok = args[i].to_string_lossy().into_owned();

        if tok == "--" {
            if mode.is_none() {
                spawn_at = Some(i + 1);
            } else {
                positional.extend(args[i + 1..].iter().cloned());
            }
            break;
        }

        if tok.len() > 1 && tok.starts_with('-') {
            // ---- long options
            if let Some(rest) = tok.strip_prefix("--") {
                let (name, inline) = match rest.find('=') {
                    Some(p) => (&rest[..p], Some(rest[p + 1..].to_string())),
                    None => (rest, None),
                };
                let next_arg = |i: &mut usize| -> Result<String, Fail> {
                    *i += 1;
                    args.get(*i)
                        .map(|a| a.to_string_lossy().into_owned())
                        .ok_or_else(|| usage_err(format!("--{name} needs a value")))
                };
                match name {
                    "view" => set_mode(&mut mode, Mode::View)?,
                    "follow" => {
                        set_mode(&mut mode, Mode::View)?;
                        follow = true;
                    }
                    "kill" => set_mode(&mut mode, Mode::Kill)?,
                    "list" => set_mode(&mut mode, Mode::List)?,
                    "remove" => set_mode(&mut mode, Mode::Remove)?,
                    "alive" => alive = true,
                    "dead" => dead = true,
                    "quiet" => quiet = true,
                    "json" => json = true,
                    "command" => {
                        let v = match inline {
                            Some(v) => v,
                            None => next_arg(&mut i)?,
                        };
                        shell_script = Some(OsString::from(v));
                    }
                    "lines" => {
                        let v = match inline {
                            Some(v) => v,
                            None => next_arg(&mut i)?,
                        };
                        lines = Some(parse_u64(&v, "--lines")?);
                    }
                    "timeout" => {
                        let v = match inline {
                            Some(v) => v,
                            None => next_arg(&mut i)?,
                        };
                        timeout_ms = parse_duration(&v)?;
                    }
                    "help" => return Ok(Cmd::Help),
                    "version" => return Ok(Cmd::Version),
                    _ => return Err(usage_err(format!("unknown option --{name}"))),
                }
                i += 1;
                continue;
            }

            // ---- short option cluster
            let chars: Vec<char> = tok[1..].chars().collect();
            let mut k = 0usize;
            while k < chars.len() {
                let c = chars[k];
                match c {
                    'v' => set_mode(&mut mode, Mode::View)?,
                    'f' => {
                        set_mode(&mut mode, Mode::View)?;
                        follow = true;
                    }
                    'k' => set_mode(&mut mode, Mode::Kill)?,
                    'l' => set_mode(&mut mode, Mode::List)?,
                    'r' => set_mode(&mut mode, Mode::Remove)?,
                    'a' => alive = true,
                    'd' => dead = true,
                    'q' => quiet = true,
                    'h' => return Ok(Cmd::Help),
                    'c' | 'n' | 't' => {
                        let rest: String = chars[k + 1..].iter().collect();
                        let val = if !rest.is_empty() {
                            rest
                        } else {
                            i += 1;
                            args.get(i)
                                .map(|a| a.to_string_lossy().into_owned())
                                .ok_or_else(|| usage_err(format!("-{c} needs a value")))?
                        };
                        match c {
                            'n' => lines = Some(parse_u64(&val, "--lines")?),
                            't' => timeout_ms = parse_duration(&val)?,
                            _ => shell_script = Some(OsString::from(val)),
                        }
                        k = chars.len();
                        continue;
                    }
                    _ => return Err(usage_err(format!("unknown option -{c}"))),
                }
                k += 1;
            }
            i += 1;
            continue;
        }

        // ---- non-option argument
        if mode.is_none() {
            // R8: everything from here on is the command, verbatim.
            spawn_at = Some(i);
            break;
        }
        positional.push(args[i].clone());
        i += 1;
    }

    if shell_script.is_some() {
        if mode.is_some() {
            return Err(usage_err("-c cannot be combined with a mode option"));
        }
        if spawn_at.is_some() {
            return Err(usage_err(
                "-c takes the whole command; do not also pass one",
            ));
        }
    }

    // Spawn mode (either form): reject the flags that only mean something with a mode.
    if spawn_at.is_some() || shell_script.is_some() {
        for (flag, on) in [
            ("--alive", alive),
            ("--dead", dead),
            ("--json", json),
            ("--lines", lines.is_some()),
        ] {
            if on {
                return Err(usage_err(format!("{flag} requires a mode option")));
            }
        }
    }

    if let Some(start) = spawn_at {
        let argv: Vec<OsString> = args[start..].to_vec();
        if argv.is_empty() {
            return Err(usage_err("no command given"));
        }
        return Ok(Cmd::Spawn {
            argv,
            quiet,
            shell_script: None,
        });
    }

    if let Some(script) = shell_script {
        // R13b: the *only* sound way to get shell syntax is to be handed the raw string, which the
        // caller delimits by quoting. Running it through the user's own shell keeps `-c` honest
        // about which language the string is in.
        let shell = std::env::var_os("SHELL")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| OsString::from("/bin/sh"));
        let argv = vec![shell, OsString::from("-c"), script.clone()];
        return Ok(Cmd::Spawn {
            argv,
            quiet,
            shell_script: Some(script),
        });
    }

    let one_id = |positional: &[OsString]| -> Result<String, Fail> {
        if positional.len() != 1 {
            return Err(usage_err("this mode takes exactly one id"));
        }
        Ok(positional[0].to_string_lossy().into_owned())
    };

    match mode {
        None => Err(usage_err("no command given")),
        Some(Mode::View) => {
            if quiet || alive || dead || json {
                return Err(usage_err("-q/-a/-d/--json do not apply to -v/-f"));
            }
            Ok(Cmd::View {
                id: one_id(&positional)?,
                follow,
                lines,
            })
        }
        Some(Mode::Kill) => {
            if lines.is_some() || quiet || alive || dead || json || follow {
                return Err(usage_err(
                    "-n/-q/-a/-d/--json/-f do not apply to -k",
                ));
            }
            Ok(Cmd::Kill {
                id: one_id(&positional)?,
                timeout_ms,
            })
        }
        Some(Mode::Remove) => {
            if lines.is_some() || quiet || alive || dead || json || follow {
                return Err(usage_err("-n/-q/-a/-d/--json/-f do not apply to -r"));
            }
            Ok(Cmd::Remove {
                id: one_id(&positional)?,
            })
        }
        Some(Mode::List) => {
            if follow || lines.is_some() || quiet {
                return Err(usage_err("-f/-n/-q do not apply to -l"));
            }
            if alive && dead {
                return Err(usage_err("-a and -d are mutually exclusive"));
            }
            if !positional.is_empty() {
                return Err(usage_err("-l takes no arguments"));
            }
            let filter = if alive {
                Filter::Alive
            } else if dead {
                Filter::Dead
            } else {
                Filter::All
            };
            Ok(Cmd::List { filter, json })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{OsStr, OsString};

    fn p(a: &[&str]) -> Result<Cmd, Fail> {
        let v: Vec<OsString> = a.iter().map(OsString::from).collect();
        parse(&v)
    }

    fn spawn_argv(c: &Cmd) -> Vec<String> {
        match c {
            Cmd::Spawn { argv, .. } => argv.iter().map(|a| a.to_string_lossy().into()).collect(),
            other => panic!("expected spawn, got {other:?}"),
        }
    }

    #[test]
    fn stops_at_first_non_option() {
        // R8: goba's own flags after the command belong to the command.
        let c = p(&["goba", "echo", "-v", "hi"]).unwrap();
        assert_eq!(spawn_argv(&c), ["echo", "-v", "hi"]);
    }

    #[test]
    fn double_dash_forces_spawn() {
        let c = p(&["goba", "--", "-weird", "--flag"]).unwrap();
        assert_eq!(spawn_argv(&c), ["-weird", "--flag"]);
    }

    #[test]
    fn mode_is_view_when_flag_precedes_id() {
        match p(&["goba", "-v", "3"]).unwrap() {
            Cmd::View { id, follow, lines } => {
                assert_eq!(id, "3");
                assert!(!follow);
                assert_eq!(lines, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn clustered_flags() {
        match p(&["goba", "-la"]).unwrap() {
            Cmd::List { filter, json } => {
                assert_eq!(filter, Filter::Alive);
                assert!(!json);
            }
            other => panic!("{other:?}"),
        }
        match p(&["goba", "-ld"]).unwrap() {
            Cmd::List { filter, .. } => assert_eq!(filter, Filter::Dead),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn clustered_value_flag() {
        match p(&["goba", "-v", "-n50", "7"]).unwrap() {
            Cmd::View { id, lines, .. } => {
                assert_eq!(id, "7");
                assert_eq!(lines, Some(50));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn follow_implies_view() {
        match p(&["goba", "-f", "9"]).unwrap() {
            Cmd::View { follow, .. } => assert!(follow),
            other => panic!("{other:?}"),
        }
        match p(&["goba", "-v", "-f", "9"]).unwrap() {
            Cmd::View { follow, .. } => assert!(follow),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn kill_with_timeout() {
        match p(&["goba", "-k", "3", "-t", "30s"]).unwrap() {
            Cmd::Kill { id, timeout_ms } => {
                assert_eq!(id, "3");
                assert_eq!(timeout_ms, 30_000);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn errors() {
        assert_eq!(p(&["goba"]).unwrap_err().code(), 1);
        assert_eq!(p(&["goba", "-a"]).unwrap_err().code(), 1); // -a without -l
        assert_eq!(p(&["goba", "-l", "-k", "3"]).unwrap_err().code(), 1);
        assert_eq!(p(&["goba", "-l", "-a", "-d"]).unwrap_err().code(), 1);
        assert_eq!(p(&["goba", "-v"]).unwrap_err().code(), 1); // mode without id
        assert_eq!(p(&["goba", "-l", "x"]).unwrap_err().code(), 1);
        assert_eq!(p(&["goba", "-v", "-n", "-1", "2"]).unwrap_err().code(), 1);
        assert_eq!(p(&["goba", "--bogus"]).unwrap_err().code(), 1);
    }

    #[test]
    fn quiet_spawn() {
        match p(&["goba", "-q", "sleep", "5"]).unwrap() {
            Cmd::Spawn { quiet, .. } => assert!(quiet),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn shell_command_flag() {
        let all = ["goba", "-c", "make -j8 && ./run"];
        match p(&all).unwrap() {
            Cmd::Spawn {
                argv,
                shell_script,
                quiet,
            } => {
                assert!(!quiet);
                assert_eq!(shell_script.as_deref(), Some(OsStr::new("make -j8 && ./run")));
                // argv is the real thing we will exec: $SHELL -c <string>
                assert_eq!(argv.len(), 3);
                assert_eq!(argv[1], "-c");
                assert_eq!(argv[2], "make -j8 && ./run");
            }
            other => panic!("{other:?}"),
        }
        // `--command=X` and clustered `-cX` both work.
        for form in [
            vec!["goba", "--command=ls | wc -l"],
            vec!["goba", "-cls | wc -l"],
        ] {
            match p(&form).unwrap() {
                Cmd::Spawn { argv, .. } => assert_eq!(argv[2], "ls | wc -l"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn shell_command_flag_errors() {
        assert_eq!(p(&["goba", "-c"]).unwrap_err().code(), 1); // needs a value
        assert_eq!(p(&["goba", "-c", "x", "y"]).unwrap_err().code(), 1); // plus a command
        assert_eq!(p(&["goba", "-c", "x", "-l"]).unwrap_err().code(), 1); // plus a mode
        assert_eq!(p(&["goba", "-c", "x", "-n", "3"]).unwrap_err().code(), 1);
        assert_eq!(p(&["goba", "-c", "x", "-k", "1"]).unwrap_err().code(), 1);
    }

    #[test]
    fn quiet_composes_with_shell_command() {
        match p(&["goba", "-q", "-c", "echo hi"]).unwrap() {
            Cmd::Spawn { quiet, .. } => assert!(quiet),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn help_and_version() {
        assert!(matches!(p(&["goba", "-h"]).unwrap(), Cmd::Help));
        assert!(matches!(p(&["goba", "--version"]).unwrap(), Cmd::Version));
    }
}
