//! Desktop notifications, sent to the freedesktop notification service over D-Bus.

use anyhow::Result;

/// Shows `body` under `title` as a desktop notification from Power Overwhelming. Blocks until
/// the notification service accepts it, so call it off the UI thread.
#[cfg(target_os = "linux")]
pub fn desktop_notification(title: &str, body: &str) -> Result<()> {
    use std::collections::HashMap;
    use zbus::zvariant::Value;
    zbus::block_on(async {
        let connection = zbus::Connection::session().await?;
        let hints: HashMap<&str, Value<'_>> = HashMap::new();
        connection
            .call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &(
                    "Power Overwhelming",
                    0u32,
                    "",
                    title,
                    body,
                    Vec::<&str>::new(),
                    hints,
                    -1i32,
                ),
            )
            .await?;
        Ok(())
    })
}

#[cfg(not(target_os = "linux"))]
pub fn desktop_notification(_title: &str, _body: &str) -> Result<()> {
    anyhow::bail!("Desktop notifications are only supported on Linux")
}
