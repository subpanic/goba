//! goba — go background.

mod cli;
mod fail;
mod json;
mod modes;
mod platform;
mod spawn;
mod store;
mod sys;

use std::ffi::OsString;
use std::io::Write;

use cli::Cmd;
use fail::Fail;
use store::Store;

fn main() {
    let args: Vec<OsString> = std::env::args_os().collect();
    let code = match cli::parse(&args) {
        Ok(cmd) => dispatch(cmd),
        Err(f) => {
            report(&f);
            f.code()
        }
    };
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

fn report(f: &Fail) {
    eprintln!("goba: {}", f.msg());
    if matches!(f, Fail::Usage(_)) {
        eprintln!();
        eprintln!("{}", cli::USAGE);
    }
}

fn dispatch(cmd: Cmd) -> i32 {
    match cmd {
        Cmd::Help => {
            println!("{}", cli::USAGE);
            0
        }
        Cmd::Version => {
            println!("goba {}", env!("CARGO_PKG_VERSION"));
            0
        }
        other => {
            let store = match Store::open() {
                Ok(s) => s,
                Err(f) => {
                    report(&f);
                    return f.code();
                }
            };
            let r = match other {
                Cmd::Spawn {
                    argv,
                    quiet,
                    shell_script,
                } => {
                    // R27: a `-c` session is named after the first word of the script, so
                    // `goba -c 'make -j8 && ./run'` is `make`, not `sh`.
                    let name_source = shell_script
                        .as_deref()
                        .and_then(store::shell_name_source)
                        .unwrap_or_else(|| argv[0].clone());
                    spawn::run(&store, &argv, quiet, &name_source)
                }
                Cmd::View { id, follow, lines } => modes::view(&store, &id, follow, lines),
                Cmd::Kill { id, timeout_ms } => modes::kill(&store, &id, timeout_ms),
                Cmd::List { filter, json } => modes::list(&store, filter, json),
                Cmd::Remove { id } => modes::remove(&store, &id),
                Cmd::Help | Cmd::Version => unreachable!(),
            };
            match r {
                Ok(()) => 0,
                Err(f) => {
                    report(&f);
                    f.code()
                }
            }
        }
    }
}
