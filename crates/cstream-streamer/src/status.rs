//! The status file the gateway reports.
//!
//! The streamer is the only process that knows which encoder actually opened, and
//! the gateway is the one a check asks. A file in `XDG_RUNTIME_DIR` is the seam:
//! written atomically (temp file + rename), so the gateway reads either the old
//! report or the new one, never half of one.

use anyhow::{Context, Result};
use std::path::Path;

/// The file name inside `XDG_RUNTIME_DIR`. The gateway's `StreamerStatusFile`
/// constant names the same file; the two are the contract.
pub const FILE: &str = "cstream-streamer.json";

/// Render the status document.
pub fn render(encoder: &str) -> String {
    // The label is a fixed vocabulary (`hardware:<element>` or `software`), but it is
    // escaped anyway: the document is JSON, and a quote in it would corrupt the file
    // the gateway parses.
    let escaped: String = encoder
        .chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            c => vec![c],
        })
        .collect();
    format!("{{\"encoder\":\"{escaped}\"}}\n")
}

/// Atomically write the status file into `runtime_dir`.
pub fn publish(runtime_dir: &str, encoder: &str) -> Result<()> {
    let dir = Path::new(runtime_dir);
    let tmp = dir.join(format!(".{FILE}.tmp"));
    std::fs::write(&tmp, render(encoder)).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join(FILE))
        .with_context(|| format!("renaming {} into place", tmp.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_the_encoder_atomically() {
        let dir = std::env::temp_dir().join(format!("cstream-status-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        publish(dir.to_str().unwrap(), "hardware:nvh264enc").unwrap();
        let got = std::fs::read_to_string(dir.join(FILE)).unwrap();
        assert_eq!(got, "{\"encoder\":\"hardware:nvh264enc\"}\n");
        assert!(
            !dir.join(format!(".{FILE}.tmp")).exists(),
            "the temp file must be renamed away"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn escapes_json_metacharacters() {
        assert_eq!(render(r#"a"b\c"#), "{\"encoder\":\"a\\\"b\\\\c\"}\n");
    }
}
