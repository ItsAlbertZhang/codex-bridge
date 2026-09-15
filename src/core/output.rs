//! Process output failures end the CLI without unwinding or retrying stdout.

use super::session::EXIT_ERROR;
use std::fmt::Display;
use std::io::{self, Write};

pub fn write_line(value: impl Display) {
    let result = {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{value}").and_then(|_| stdout.flush())
    };
    check_stdout(result);
}

pub fn flush_stdout() {
    check_stdout(io::stdout().flush());
}

pub fn check_stdout(result: io::Result<()>) {
    if let Err(error) = result {
        let message = error.to_string().replace(['\r', '\n'], " ");
        let mut stderr = io::stderr().lock();
        let _ = writeln!(stderr, "agent-bridge: failed writing stdout: {message}");
        let _ = stderr.flush();
        std::process::exit(EXIT_ERROR);
    }
}
