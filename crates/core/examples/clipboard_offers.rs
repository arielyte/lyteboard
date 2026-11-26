//! Observe selection changes without sending a clipboard data request.

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::{
        io::{self, Write},
        time::Instant,
    };
    use wl_clipboard_rs::{
        paste::Seat,
        watch::{ClipboardEvent, ClipboardType, Watcher},
    };

    let start = Instant::now();
    let mut watcher = Watcher::new(ClipboardType::Regular, Seat::Unspecified)?;
    let mut output = io::stdout().lock();
    while let Some(event) = watcher.next_event()? {
        write!(output, "{:>7} ms ", start.elapsed().as_millis())?;
        match event {
            ClipboardEvent::Changed { mime_types, .. } => {
                let has = |name| mime_types.iter().any(|value| value == name);
                let sensitive = mime_types.iter().any(|value| {
                    value.eq_ignore_ascii_case("x-kde-passwordManagerHint")
                        || value.eq_ignore_ascii_case("application/x-kde-passwordManagerHint")
                });
                writeln!(
                    output,
                    "offer jpeg={} png={} webp={} text={} sensitive={} targets={}",
                    has("image/jpeg"),
                    has("image/png"),
                    has("image/webp"),
                    has("text/plain") || has("text/plain;charset=utf-8") || has("UTF8_STRING"),
                    sensitive,
                    mime_types.len(),
                )?;
            }
            ClipboardEvent::Cleared { .. } => writeln!(output, "selection cleared")?,
        }
        output.flush()?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("This diagnostic requires a Linux Wayland session.");
}
