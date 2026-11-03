//! End-to-end tests that run the compiled `dush` binary.

use std::fs;
use std::process::{Command, Stdio};

use tempfile::TempDir;

/// A reader that goes away before dush writes (`dush | head -1`) must
/// not crash dush. Before the fix this panicked with "failed printing to
/// stdout: Broken pipe" and exited with status 101.
///
/// We close our end of the stdout pipe before dush gets a chance to write.
/// If dush happens to write first, the output fits in the pipe buffer and
/// the test still passes, just without exercising the error path — so the
/// test can never fail spuriously, only (rarely) pass vacuously.
#[cfg(unix)]
#[test]
fn closed_stdout_pipe_is_not_an_error() {
    let tmp = TempDir::new().expect("tempdir");
    for i in 0..100 {
        fs::write(tmp.path().join(format!("file-{i}")), b"x").expect("write");
    }

    for format in ["pretty", "text", "csv", "json"] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dush"))
            .args(["-A", "-f", format])
            .arg(tmp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn dush");
        drop(child.stdout.take());

        let output = child.wait_with_output().expect("wait for dush");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "format {format}: status {:?}, stderr:\n{stderr}",
            output.status
        );
        assert!(
            !stderr.contains("panicked"),
            "format {format}: stderr:\n{stderr}"
        );
    }
}
