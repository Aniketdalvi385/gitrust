use anyhow::Context;
use chrono::Local;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

/// Structured execution logger writing to ~/.glit/log.
/// Writes START/END entries per step so crash recovery can detect
/// which step was interrupted.
pub struct ExecutionLog {
    pub pipeline_id: String,
    log_path: PathBuf,
    enabled: bool,
}

impl ExecutionLog {
    pub fn new(pipeline_id: &str, enabled: bool) -> anyhow::Result<Self> {
        let log_dir = crate::config::glit_dir()?;
        fs::create_dir_all(&log_dir)
            .with_context(|| format!("Failed to create log dir: {}", log_dir.display()))?;
        Ok(Self {
            pipeline_id: pipeline_id.to_string(),
            log_path: log_dir.join("log"),
            enabled,
        })
    }

    pub fn log_pipeline_start(&self, query: &str) -> anyhow::Result<()> {
        self.append(&format!(
            "PIPELINE start   id={} query=\"{}\"",
            self.pipeline_id, query
        ))
    }

    pub fn log_pipeline_end(&self, status: &str, duration_secs: u64) -> anyhow::Result<()> {
        self.append(&format!(
            "PIPELINE end     id={} duration={}s status={}",
            self.pipeline_id, duration_secs, status
        ))
    }

    pub fn log_classify(&self, intent: &str, confidence: f32) -> anyhow::Result<()> {
        self.append(&format!(
            "CLASSIFY         intent={} confidence={:.2}",
            intent, confidence
        ))
    }

    pub fn log_snapshot(&self, head: &str, index: &str) -> anyhow::Result<()> {
        self.append(&format!("SNAPSHOT         head={} index={}", head, index))
    }

    pub fn log_resolve(
        &self,
        related: usize,
        uncertain: usize,
        unrelated: usize,
    ) -> anyhow::Result<()> {
        self.append(&format!(
            "RESOLVE          related={} uncertain={} unrelated={}",
            related, uncertain, unrelated
        ))
    }

    pub fn log_gate(&self, description: &str) -> anyhow::Result<()> {
        self.append(&format!("GATE             {}", description))
    }

    pub fn log_step_start(&self, step: &str) -> anyhow::Result<()> {
        self.append(&format!("STEP start       {}", step))
    }

    pub fn log_step_end(&self, step: &str, status: &str) -> anyhow::Result<()> {
        self.append(&format!("STEP end         {} {}", step, status))
    }

    pub fn log_post_check(&self, step: &str, result: &str) -> anyhow::Result<()> {
        self.append(&format!("POST-CHECK       {} {}", step, result))
    }

    fn append(&self, message: &str) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
        let line = format!("{}  {}\n", timestamp, message);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .with_context(|| format!("Failed to open log at {}", self.log_path.display()))?;
        file.write_all(line.as_bytes())
            .context("Failed to write log entry")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_disabled() {
        let log = ExecutionLog {
            pipeline_id: "test-123".to_string(),
            log_path: PathBuf::from("/tmp/glit-test-log"),
            enabled: false,
        };
        // Should not error even when disabled
        log.log_step_start("ADD").unwrap();
        log.log_step_end("ADD", "ok").unwrap();
    }

    #[test]
    fn test_log_write_and_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log");
        let log = ExecutionLog {
            pipeline_id: "test-456".to_string(),
            log_path: log_path.clone(),
            enabled: true,
        };
        log.log_step_start("COMMIT").unwrap();
        log.log_step_end("COMMIT", "hash=abc123").unwrap();

        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("STEP start       COMMIT"));
        assert!(content.contains("STEP end         COMMIT hash=abc123"));
    }

    #[test]
    fn test_log_format() {
        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log");
        let log = ExecutionLog {
            pipeline_id: "fmt-test".to_string(),
            log_path: log_path.clone(),
            enabled: true,
        };
        log.log_pipeline_start("push login changes").unwrap();
        log.log_classify("FeaturePipeline", 0.91).unwrap();

        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("PIPELINE start   id=fmt-test query=\"push login changes\""));
        assert!(content.contains("CLASSIFY         intent=FeaturePipeline confidence=0.91"));
    }
}
