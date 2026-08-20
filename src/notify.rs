use tracing::warn;

/// Best-effort desktop notification. Failures are logged, never fatal.
/// macOS: osascript (built in). Linux: notify-send (libnotify).
pub async fn notify(title: &str, message: &str) {
    if let Err(e) = send(title, message).await {
        warn!("notification failed: {e}");
    }
}

#[cfg(target_os = "macos")]
async fn send(title: &str, message: &str) -> std::io::Result<()> {
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        escape(message),
        escape(title)
    );
    tokio::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .await?;
    Ok(())
}

#[cfg(target_os = "linux")]
async fn send(title: &str, message: &str) -> std::io::Result<()> {
    // Args go straight to exec (no shell), so no escaping needed.
    tokio::process::Command::new("notify-send")
        .arg("--app-name=kwkly")
        .arg(title)
        .arg(message)
        .output()
        .await?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn send(title: &str, message: &str) -> std::io::Result<()> {
    tracing::info!("[notification] {title}: {message}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
