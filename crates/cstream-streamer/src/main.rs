use anyhow::Result;

fn main() -> Result<()> {
    let raised = cstream_streamer::init()?;
    if raised.is_empty() {
        eprintln!("cstream-streamer: no VA-API encoder found — the software path will be used");
    } else {
        eprintln!(
            "cstream-streamer: promoted hardware encoders: {}",
            raised.join(", ")
        );
    }
    Ok(())
}
