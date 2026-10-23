use crate::{Ingest, JobError, plan::Planned, protocol::Report};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};

pub(crate) fn retry(reason: &str) -> JobError {
    JobError::Retry {
        after: Duration::from_secs(30),
        reason: reason.into(),
    }
}
async fn capture(reader: impl AsyncRead + Unpin, cap: usize) -> Result<Vec<u8>, JobError> {
    let mut bytes = Vec::new();
    reader
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| retry("codec stream failed"))?;
    if bytes.len() > cap {
        return Err(retry("codec report exceeded limit"));
    }
    Ok(bytes)
}
/// Preserve the first line only when it is a plain diagnostic. Unknown punctuation,
/// separators and path-like words are replaced, including relative and Windows paths.
pub(crate) fn failure_reason(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let line = text.lines().next().unwrap_or("image rejected");
    if line.is_empty()
        || line
            .chars()
            .any(|c| !c.is_alphabetic() && !c.is_whitespace())
        || line.contains("..")
    {
        return "image rejected".into();
    }
    line.chars().filter(|c| !c.is_control()).take(200).collect()
}
impl Ingest {
    pub(crate) async fn run_host(
        &self,
        input: &Path,
        out: &Path,
        outputs: &[Planned],
        attempt: i32,
    ) -> Result<Report, JobError> {
        let request = serde_json::json!({
            "input":input, "out_dir":out, "limits":self.limits,
            "outputs":outputs.iter().map(|o| serde_json::json!({"name":o.name,"resize":{"long_edge":o.long_edge,"square":o.square},"encode":o.format.encode(o.long_edge),"keep_icc":o.keep_icc})).collect::<Vec<_>>(),
            "want":["identity","metadata","thumbhash","dominant_colors"]
        });
        self.request_host(&request, attempt).await
    }
    pub(crate) async fn request_host<T: serde::de::DeserializeOwned>(
        &self,
        request: &serde_json::Value,
        attempt: i32,
    ) -> Result<T, JobError> {
        let data = serde_json::to_vec(request)
            .map_err(|_| JobError::Fatal("invalid codec request".into()))?;
        let mut command = Command::new(&self.codec_host);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("VIPS_CONCURRENCY", self.concurrency_per_host.to_string())
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Each invocation gets a fresh process group. Dropping the guard kills both
        // the host supervisor and its sandbox child, including on task cancellation.
        command.process_group(0);
        #[cfg(feature = "test-hooks")]
        if let Some(delay) = self.test_delay {
            command.env("YHOTOS_TEST_DELAY_MS", delay.as_millis().to_string());
        }
        let mut child = command
            .spawn()
            .map_err(|_| retry("codec host unavailable"))?;
        let _guard = ProcessGroup(child.id());
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| retry("codec stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| retry("codec stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| retry("codec stderr unavailable"))?;
        let operation = async {
            let write = async {
                stdin
                    .write_all(&data)
                    .await
                    .map_err(|_| retry("codec input failed"))?;
                drop(stdin);
                Ok::<(), JobError>(())
            };
            let wait = async { child.wait().await.map_err(|_| retry("codec wait failed")) };
            let ((), stdout, stderr, status) = tokio::try_join!(
                write,
                capture(stdout, 1 << 20),
                capture(stderr, 64 << 10),
                wait
            )?;
            match status.code() {
                Some(0) => {
                    serde_json::from_slice(&stdout).map_err(|_| retry("invalid codec report"))
                }
                Some(2) => Err(JobError::Fatal(failure_reason(&stderr))),
                Some(3) if attempt >= 2 => Err(JobError::Fatal("image processing failed".into())),
                Some(4) => Err(retry("codec timed out")),
                _ => Err(retry("image processing failed")),
            }
        };
        if let Ok(result) = tokio::time::timeout(self.codec_timeout, operation).await {
            result
        } else {
            let _ = child.kill().await;
            Err(retry("codec timed out"))
        }
    }
}
struct ProcessGroup(Option<u32>);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0.and_then(|id| i32::try_from(id).ok()) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[allow(clippy::unwrap_used)]
    fn fake_host(script: &str) -> (tempfile::TempDir, Ingest) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host");
        std::fs::write(&path, format!("#!/bin/sh\ncat >/dev/null\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://127.0.0.1/unused")
            .unwrap();
        let runner = Ingest::new(yhotos_storage::Store::open(dir.path()).unwrap(), pool, path);
        (dir, runner)
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn process_exit_policy_and_path_redaction() {
        let (dir, runner) = fake_host("echo 'error /private/original.jpg' >&2; exit 2");
        assert!(
            matches!(runner.run_host(dir.path(),dir.path(),&[],1).await,Err(JobError::Fatal(reason)) if reason == "image rejected")
        );
        let (dir, runner) = fake_host("exit 3");
        assert!(matches!(
            runner.run_host(dir.path(), dir.path(), &[], 1).await,
            Err(JobError::Retry { .. })
        ));
        assert!(matches!(
            runner.run_host(dir.path(), dir.path(), &[], 2).await,
            Err(JobError::Fatal(_))
        ));
        let (dir, runner) = fake_host("exit 4");
        assert!(matches!(
            runner.run_host(dir.path(), dir.path(), &[], 1).await,
            Err(JobError::Retry { .. })
        ));
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn process_timeout_and_oversized_streams_are_bounded() {
        let (dir, mut runner) = fake_host("exec /bin/sleep 10");
        runner.codec_timeout = Duration::from_millis(50);
        let start = std::time::Instant::now();
        assert!(matches!(
            runner.run_host(dir.path(), dir.path(), &[], 1).await,
            Err(JobError::Retry { .. })
        ));
        assert!(start.elapsed() < Duration::from_secs(2));
        for script in ["head -c 1048577 /dev/zero", "head -c 65537 /dev/zero >&2"] {
            let (dir, runner) = fake_host(script);
            assert!(matches!(
                runner.run_host(dir.path(), dir.path(), &[], 1).await,
                Err(JobError::Retry { .. })
            ));
        }
    }
    #[test]
    fn failure_reason_is_one_line_bounded_and_path_free() {
        for text in [
            "error /data/private/photo.jpg",
            "error C:\\private\\image",
            "error ../photo",
            "error relative.jpg",
            "error file:secret",
        ] {
            assert_eq!(failure_reason(text.as_bytes()), "image rejected");
        }
        assert_eq!(
            failure_reason(b"image rejected\n/data/secret"),
            "image rejected"
        );
        assert_eq!(
            failure_reason("x".repeat(400).as_bytes()).chars().count(),
            200
        );
    }
    #[tokio::test]
    async fn capture_rejects_overflow_without_buffering_the_stream() {
        assert!(capture(&b"12345"[..], 4).await.is_err());
        assert_eq!(capture(&b"1234"[..], 4).await.ok(), Some(b"1234".to_vec()));
    }
}
