# goba — go background

Start any command detached from your terminal, keep its output, and manage it by id.
One static binary, no daemon, no service files, no wrapper syntax.

```
$ goba make -j8
4 make
$ goba -l
NUM  SID      STATE            AGE      COMMAND
  4  make     running          00:00:31  make -j8
  3  make-2   exited(0)        00:12:04  make -j4
  2  fetch    exited(0)        00:31:55  ./scripts/fetch-assets.sh
  1  make-3   killed(SIGTERM)  01:04:02  make -j8
$ goba -f make          # follow the build
$ goba -k make          # …or stop it
```

Sessions are retained until the machine reboots. `goba -r <id>` forgets one early.

## Build

```sh
cargo build --release        # target/release/goba
cargo test                   # unit + integration suites
```

macOS (arm64/x86_64) and Linux (x86_64/arm64). No runtime dependencies beyond libc.

## Usage

```
goba [--] <command> [args...]     start a command in the background
goba -c <string>                  start a shell command (pipes, &&, redirects)
goba -v <id> [-n N]               print a session's captured output
goba -f <id> [-n N]               follow a session's output until it ends
goba -k <id> [-t DUR]             terminate a session (SIGTERM, then SIGKILL)
goba -l [-a|-d] [--json]          list sessions
goba -r <id>                      forget a session and delete its log
```

- `-c STR` — run STR with `$SHELL -c`; see [Shell syntax](#shell-syntax--c).
- `-n N` — only the last N lines (like `tail -n`).
- `-t DUR` — grace before SIGKILL; `5`, `5s`, `500ms`, `2m`, `1h`. Default `5s`.
- `-l` filters: `-a`/`--alive` (running only), `-d`/`--dead` (finished only).
- `-q` — print only the numeric id (for scripts).

Starting prints `<numeric id> <string id>` on stdout and nothing else, so
`id=$(goba -q long-job)` is safe. Everything else — progress, errors, confirmations — goes to
stderr.

Option parsing stops at the first non-option argument, so a command may contain anything that looks
like a goba flag. Use `--` when the command itself starts with a dash:

```sh
goba echo -v -l -- hi there         # runs: echo -v -l -- hi there
goba -- -weird-binary --flag
```

### Ids

Every session gets two handles: a small **number**, and a **name taken from the command**. The name
is the command's basename, so `goba make -j8` is `make`, and `/usr/bin/sleep` is `sleep`. When two
sessions would share a name, the second takes the next free integer suffix:

```
$ goba sleep 30; goba sleep 30; goba sleep 30
1 sleep
2 sleep-2
3 sleep-3
```

Either handle works anywhere an id is expected, and a **unique** prefix of a name works too. An
exact name always wins over a prefix, so `goba -v sleep` is the session named `sleep` and never
`sleep-2`:

```sh
goba -v 2          # by number
goba -v sleep-2    # by name
goba -v sle        # unique prefix — but errors if it matches more than one session
```

Names are folded to something safe to type unquoted: lower-case, with anything outside
`a-z 0-9 _ . +` turned into `-` and long names truncated. Removing a session frees its name for
reuse. One consequence worth knowing: a token of all digits is always read as the *number*, so a
command whose name is entirely numeric is reachable only by number.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | usage error |
| 2 | ambiguous id prefix |
| 3 | no such session, or already finished where that is an error |
| 4 | session store unusable |
| 5 | could not start the command |
| 6 | could not kill |
| 7 | could not remove (still running) |

Note that `goba -v` exits 0 even when the *session* failed: the command's exit status is data
(`goba -l --json`), not goba's own status.

## Shell syntax: `-c`

A plain `goba <cmd>` never involves a shell — for any single command you need nothing special:

```sh
goba make -j8
goba rsync -av /src /dst
```

Shell *syntax* (pipes, `&&`, redirects, loops, globs) is different, because your shell has already
parsed the line before goba runs. `goba seq 1 200000 | wc -l` counts the one line goba printed, not
the sequence — and `goba make && ./run` starts `make` and then runs `./run` *in the foreground*.
When you want shell syntax in the background, hand goba the raw string with `-c`:

```sh
goba -c 'make -j8 && ./run'                     # named `make`
goba -c 'tail -f app.log | grep -i error'
goba -c 'for f in *.csv; do wc -l "$f"; done > counts.txt'
```

`-c` runs the string with `$SHELL -c` (falling back to `/bin/sh`), so it is in the language you
typed it in, and the session is named after the script's first word (leading `VAR=value`
assignments are skipped, so `-c 'FOO=1 make'` is `make`). Redirections and variables belong to the
session, not to goba.

goba will not *guess*. It never joins your words back into a string to re-parse them, because the
quoting is already gone by then — `goba echo 'a  b'` is one argument with two spaces, and
`goba echo 'a|b'` is a literal pipe. `-c` exists precisely so that never has to be guessed.

## Where output goes, and why it looks different from a terminal

The command's stdout and stderr are redirected to a file, so the command is **not talking to a
terminal**. Two consequences worth knowing:

- **Output is block-buffered, so `-f` shows bursts, not a trickle.** A program writing a little at a
  time will appear silent until it has filled its ~4-8 KB buffer. If you need live output:

  ```sh
  goba stdbuf -oL ./server        # glibc/coreutils
  goba python -u worker.py
  goba grep --line-buffered pattern
  ```

- **`isatty()` is false, so colour and progress bars switch off.** Many tools have a force flag
  (`--color=always`, `--progress=plain`) if you want them anyway. Escape sequences a program emits
  anyway end up in the log verbatim.

## Killing

`goba -k <id>` signals the command's whole **process group**, so children die with it. It sends
`SIGTERM`, then `SIGKILL` after the grace period if the command traps or ignores it.

A process that escapes the group by creating its own session (`setsid`, some daemons) is beyond
goba's reach; this is best-effort by design, not a sandbox.

## Retention and growth

- Retained **until reboot**, then reclaimed automatically. This is enforced by a boot-identity stamp
  on every session, not by relying on the OS to clear a temp directory.
- Within a boot, logs live in `$GOBA_DIR` (default: `$XDG_RUNTIME_DIR/goba` on Linux, `$TMPDIR/goba`
  on macOS, else `/tmp/goba-<uid>`). Override with `GOBA_DIR`.
- **Log growth is unbounded.** `goba yes` will fill your disk. Nothing truncates a log — the trade is
  that `goba -v` always shows exactly what the command wrote, byte for byte, binary included.

## What goba is not

Not a terminal multiplexer: there is no attach, and the command gets `/dev/null` on stdin. Not a
service manager: no restart policies, no dependencies. Not durable across reboot, deliberately.

`REQUIREMENTS.md` is the full specification — the process model, the on-disk format, the failure
contract and the acceptance criteria this implementation is tested against.
