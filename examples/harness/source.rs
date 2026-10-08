//! The bench signal source: a child process that keys a continuous-wave
//! tone into a cable, driven over its stdin. One line per command, one
//! reply line per command on its stdout:
//!
//! ```text
//! tone <hz> <txvga_db 0..47> <backoff_db -60..0>   -> ok ... | err ...
//! off                                               -> ok off
//! quit                                              (exits; EOF does too)
//! ```
//!
//! The process prints `ready ...` once it holds its transmitter. That
//! line names the transmitter, so it is never recorded; only the rate and
//! tone-offset fields are kept. Levels are relative: txvga steps are
//! nominally 1 dB, backoff is exact digital scaling (quantised by the
//! 8-bit DAC below about -40 dB). No absolute calibration exists, so a
//! record compares readings at different source settings, never states a
//! power. A caller settles [`SETTLE`] after `ok` before measuring (the
//! source's queue holds ~55 ms of samples).
//!
//! The child runs in its own process group, so a Ctrl-C at the terminal
//! reaches only the harness; the harness's handler sets a flag the
//! sweeps check, and dropping the [`Source`] sends `off` and `quit`. If
//! the harness dies outright the pipe closes and the source unkeys on EOF.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::json::J;

/// Settle after `ok` before a reading (the source's own guidance: at
/// least 100 ms); `--settle MS` raises it when the receiver's loops need
/// longer (the meter record showed the IF loop still moving 150 ms after
/// a level change).
pub const SETTLE: Duration = Duration::from_millis(150);
static SETTLE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(150);

pub fn set_settle_ms(ms: u64) {
    SETTLE_MS.store(ms.max(SETTLE.as_millis() as u64), Ordering::Relaxed);
}

pub fn settle() -> Duration {
    Duration::from_millis(SETTLE_MS.load(Ordering::Relaxed))
}
/// The source may be a `cargo run` that compiles first.
const READY_TIMEOUT: Duration = Duration::from_secs(600);
/// A retune stops and restarts the transmitter.
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);
const EXIT_WAIT: Duration = Duration::from_secs(5);

/// Backoff below this is DAC-quantised; the level steps stay above it.
pub const BACKOFF_FLOOR_DB: i32 = -36;
pub const TXVGA_MAX: i32 = 47;

static COMMAND: OnceLock<String> = OnceLock::new();

/// `--source-cmd` as given on the command line.
pub fn set_command(s: String) {
    let _ = COMMAND.set(s);
}

pub fn command() -> Option<&'static str> {
    COMMAND.get().map(String::as_str)
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: i32) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

unsafe extern "C" {
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
}

const SIGINT: i32 = 2;

/// Route Ctrl-C to a flag the sweeps poll between readings.
pub fn catch_interrupt() {
    // SAFETY: installing a handler that only stores to an atomic.
    unsafe {
        signal(SIGINT, on_sigint);
    }
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Err("interrupted") once Ctrl-C was seen.
pub fn check() -> Result<(), String> {
    if interrupted() { Err("interrupted".into()) } else { Ok(()) }
}

/// Split a command line into arguments: whitespace separates, single and
/// double quotes group, a backslash escapes the next character outside
/// single quotes.
pub fn split_args(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_arg = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_arg = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    cur.push(c);
                }
            }
            '"' => {
                in_arg = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => cur.push(chars.next().ok_or("unterminated escape")?),
                        Some(c) => cur.push(c),
                        None => return Err("unterminated double quote".into()),
                    }
                }
            }
            '\\' => {
                in_arg = true;
                cur.push(chars.next().ok_or("unterminated escape")?);
            }
            c if c.is_whitespace() => {
                if in_arg {
                    out.push(std::mem::take(&mut cur));
                    in_arg = false;
                }
            }
            c => {
                in_arg = true;
                cur.push(c);
            }
        }
    }
    if in_arg {
        out.push(cur);
    }
    Ok(out)
}

/// The tone the source is keyed with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tone {
    pub hz: f64,
    pub txvga_db: i32,
    pub backoff_db: i32,
}

impl Tone {
    /// The relative level in dB: txvga + backoff (nominal; see the module
    /// doc).
    pub fn level_db(&self) -> i32 {
        self.txvga_db + self.backoff_db
    }

    pub fn record(&self) -> J {
        J::obj().with("hz", self.hz).with("txvga_db", self.txvga_db).with("backoff_db", self.backoff_db).with("level_db_relative", self.level_db())
    }
}

/// A relative level: the backoff moves first (exact scaling), in whole
/// dB, between [`BACKOFF_FLOOR_DB`] and 0; txvga moves when the backoff
/// cannot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Level {
    pub txvga_db: i32,
    pub backoff_db: i32,
}

impl Level {
    /// Move by `delta_db` (either sign); Err when the source is at its
    /// limit in that direction. Returns which knob moved.
    pub fn step(&mut self, delta_db: i32) -> Result<&'static str, String> {
        let b = self.backoff_db + delta_db;
        if (BACKOFF_FLOOR_DB..=0).contains(&b) {
            self.backoff_db = b;
            return Ok("backoff");
        }
        let t = self.txvga_db + delta_db;
        if (0..=TXVGA_MAX).contains(&t) {
            self.txvga_db = t;
            return Ok("txvga");
        }
        Err(format!("source at its limit: txvga {} backoff {} cannot move {delta_db:+} dB", self.txvga_db, self.backoff_db))
    }

    pub fn tone(&self, hz: f64) -> Tone {
        Tone { hz, txvga_db: self.txvga_db, backoff_db: self.backoff_db }
    }
}

pub struct Source {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    command_line: String,
    /// Fields of the ready line that identify nothing: rate, offset.
    ready_fields: J,
    pub current: Option<Tone>,
    commands: usize,
    closed: bool,
}

impl Source {
    /// Spawn `--source-cmd` and wait for `ready`.
    pub fn open() -> Result<Source, String> {
        let cmd = command().ok_or("a signal source is required: --source-cmd \"<command line>\"")?;
        Source::spawn(cmd)
    }

    pub fn spawn(command_line: &str) -> Result<Source, String> {
        let args = split_args(command_line)?;
        let (prog, rest) = args.split_first().ok_or("--source-cmd is empty")?;
        let mut cmd = Command::new(prog);
        cmd.args(rest).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawn {prog}: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let mut src = Source { child, stdin, lines, command_line: command_line.to_string(), ready_fields: J::obj(), current: None, commands: 0, closed: false };
        let start = Instant::now();
        loop {
            let left = READY_TIMEOUT.checked_sub(start.elapsed()).ok_or("the source did not print ready in time")?;
            match src.lines.recv_timeout(left) {
                Ok(l) if l.starts_with("ready") => {
                    // Keep only the numeric fields after "rate" and "offset".
                    let w: Vec<&str> = l.split_whitespace().collect();
                    let after = |k: &str| w.iter().position(|x| *x == k).and_then(|i| w.get(i + 1)).and_then(|v| v.parse::<f64>().ok());
                    src.ready_fields = J::obj().with("rate_hz", after("rate")).with("tone_offset_above_lo_hz", after("offset"));
                    break;
                }
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => return Err("the source did not print ready in time".into()),
                Err(RecvTimeoutError::Disconnected) => return Err("the source exited before ready".into()),
            }
        }
        Ok(src)
    }

    /// For the records: the command line, the ready fields, the settle.
    pub fn describe(&self) -> J {
        J::obj()
            .with("command", self.command_line.as_str())
            .with("ready", self.ready_fields.clone())
            .with("settle_ms_after_ok", settle().as_millis() as u64)
            .with("levels", "relative only: txvga nominally 1 dB per step, backoff exact digital scaling (DAC-quantised below about -40 dB); the tone sits at the offset above the source's LO, whose leakage is that far below the tone")
    }

    fn reply(&mut self, line: &str) -> Result<String, String> {
        self.commands += 1;
        writeln!(self.stdin, "{line}").and_then(|_| self.stdin.flush()).map_err(|e| format!("source stdin: {e}"))?;
        match self.lines.recv_timeout(REPLY_TIMEOUT) {
            Ok(r) if r.starts_with("ok") => Ok(r),
            Ok(r) => Err(format!("source answered {r:?} to {line:?}")),
            Err(RecvTimeoutError::Timeout) => Err(format!("source gave no reply to {line:?}")),
            Err(RecvTimeoutError::Disconnected) => Err("source exited".into()),
        }
    }

    /// Key the tone, check `ok`, settle. The record carries the reply
    /// (the source's echo, which includes its wire amplitude in DAC LSB).
    pub fn tone(&mut self, t: Tone) -> Result<J, String> {
        let r = self.reply(&format!("tone {} {} {}", t.hz, t.txvga_db, t.backoff_db))?;
        self.current = Some(t);
        std::thread::sleep(settle());
        Ok(t.record().with("reply", r))
    }

    pub fn level(&mut self, hz: f64, l: Level) -> Result<J, String> {
        self.tone(l.tone(hz))
    }

    pub fn off(&mut self) -> Result<J, String> {
        let r = self.reply("off")?;
        self.current = None;
        std::thread::sleep(settle());
        Ok(J::obj().with("off", true).with("reply", r))
    }

    /// Unkey, quit, wait for the exit.
    pub fn quit(mut self) {
        self.close();
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let _ = self.reply("off");
        let _ = writeln!(self.stdin, "quit").and_then(|_| self.stdin.flush());
        let start = Instant::now();
        while start.elapsed() < EXIT_WAIT {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STUB: &str = "sh -c 'echo ready stub rate 2400000 offset 400000 amp off; while read l; do case \"$l\" in quit) exit 0;; off) echo ok off;; tone*) echo ok $l;; *) echo err unknown;; esac; done'";

    #[test]
    fn splits_quoted_arguments() {
        assert_eq!(split_args("a b  c").unwrap(), ["a", "b", "c"]);
        assert_eq!(split_args("sh -c 'echo \"x y\"'").unwrap(), ["sh", "-c", "echo \"x y\""]);
        assert_eq!(split_args("a \"b c\" d\\ e").unwrap(), ["a", "b c", "d e"]);
        assert!(split_args("\"open").is_err());
    }

    #[test]
    fn drives_a_stub_that_answers_ok() {
        let mut s = Source::spawn(STUB).expect("spawn stub");
        assert_eq!(s.ready_fields.render(), "{\"rate_hz\":2400000,\"tone_offset_above_lo_hz\":400000}");
        let r = s.tone(Tone { hz: 300e6, txvga_db: 10, backoff_db: -6 }).expect("tone");
        assert!(r.render().contains("\"reply\":\"ok tone 300000000 10 -6\""));
        assert_eq!(s.current.map(|t| t.level_db()), Some(4));
        let off = s.off().expect("off");
        assert!(off.render().contains("ok off"));
        assert!(s.reply("bogus").is_err());
        s.quit();
    }

    #[test]
    fn level_steps_backoff_first_then_txvga() {
        let mut l = Level { txvga_db: 10, backoff_db: -30 };
        assert_eq!(l.step(-6).unwrap(), "backoff");
        assert_eq!(l.backoff_db, -36);
        assert_eq!(l.step(-6).unwrap(), "txvga");
        assert_eq!((l.txvga_db, l.backoff_db), (4, -36));
        assert!(l.step(-6).is_err());
        let mut l = Level { txvga_db: 40, backoff_db: 0 };
        assert_eq!(l.step(6).unwrap(), "txvga");
        assert_eq!(l.txvga_db, 46);
        assert!(l.step(6).is_err());
    }
}
