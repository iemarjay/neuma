use anyhow::{Context, Result};
use arboard::Clipboard;
use enigo::{Enigo, Key, Keyboard, Settings as EnigoSettings};
use std::thread;
use std::time::Duration;

// ─── macOS Accessibility permission check ────────────────────────────────────

#[cfg(target_os = "macos")]
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// On macOS, keystroke simulation requires Accessibility permission.
/// If not granted, opens System Settings to the right pane and returns an error.
/// The user grants access there, then restarts Neuma.
#[cfg(target_os = "macos")]
fn require_accessibility() -> Result<()> {
    if unsafe { AXIsProcessTrusted() } {
        return Ok(());
    }
    // Open System Settings → Privacy & Security → Accessibility
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .spawn();
    anyhow::bail!(
        "Accessibility access required. Grant Neuma access in System Settings → Privacy & Security → Accessibility, then restart."
    )
}

/// Inject `text` into the currently focused application by:
/// 1. Saving the current clipboard contents.
/// 2. Writing `text` to the clipboard.
/// 3. Simulating the system paste shortcut (Cmd+V on macOS, Ctrl+V elsewhere).
/// 4. Restoring the original clipboard contents.
pub fn inject(text: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    require_accessibility()?;
    let mut clipboard = Clipboard::new().context("failed to open clipboard")?;

    // Save current clipboard text. Non-text content (images, HTML, files) cannot
    // be round-tripped through arboard; None here means we skip restore below.
    let original: Option<String> = clipboard.get_text().ok();

    // Write the transcribed/cleaned text
    clipboard
        .set_text(text.to_owned())
        .context("failed to write text to clipboard")?;

    // Give clipboard time to settle before simulating the keystroke
    thread::sleep(Duration::from_millis(50));

    // Simulate paste
    paste_shortcut().context("failed to simulate paste keystroke")?;

    // Give the target app time to consume the clipboard before we restore it
    thread::sleep(Duration::from_millis(100));

    // Restore original clipboard text. If the clipboard held non-text content
    // (image, HTML, file reference, etc.) we leave it alone rather than
    // destroying it — the clipboard will simply contain the injected text.
    if let Some(orig) = original {
        let _ = clipboard.set_text(orig);
    }

    Ok(())
}

/// Simulate Cmd+V (macOS) or Ctrl+V (Windows / Linux).
fn paste_shortcut() -> Result<()> {
    let mut enigo = Enigo::new(&EnigoSettings::default())
        .context("failed to create Enigo instance")?;

    #[cfg(target_os = "macos")]
    {
        enigo
            .key(Key::Meta, enigo::Direction::Press)
            .context("failed to press Meta")?;
        enigo
            .key(Key::Unicode('v'), enigo::Direction::Click)
            .context("failed to click v")?;
        enigo
            .key(Key::Meta, enigo::Direction::Release)
            .context("failed to release Meta")?;
    }

    #[cfg(not(target_os = "macos"))]
    {
        enigo
            .key(Key::Control, enigo::Direction::Press)
            .context("failed to press Control")?;
        enigo
            .key(Key::Unicode('v'), enigo::Direction::Click)
            .context("failed to click v")?;
        enigo
            .key(Key::Control, enigo::Direction::Release)
            .context("failed to release Control")?;
    }

    Ok(())
}
