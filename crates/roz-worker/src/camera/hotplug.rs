/// Hot-plug events for camera devices.
#[derive(Debug, Clone)]
pub enum HotplugEvent {
    /// A new video device appeared (e.g., `/dev/video0`).
    Added(String),
    /// A video device was removed.
    Removed(String),
}

/// Watch `/dev/` for `video*` device additions and removals using inotify.
///
/// This function runs forever, sending `HotplugEvent`s on the provided channel.
/// It only returns if the channel is closed (receiver dropped).
#[cfg(target_os = "linux")]
pub async fn watch_dev_video(event_tx: tokio::sync::mpsc::Sender<HotplugEvent>) {
    use inotify::{EventMask, Inotify, WatchMask};

    let Ok(inotify) = Inotify::init() else {
        tracing::error!("failed to initialize inotify for /dev/ hot-plug monitoring");
        return;
    };

    if inotify
        .watches()
        .add("/dev/", WatchMask::CREATE | WatchMask::DELETE)
        .is_err()
    {
        tracing::error!("failed to add inotify watch on /dev/");
        return;
    }

    let mut stream = inotify.into_event_stream([0u8; 4096]).expect("inotify event stream");

    loop {
        use futures::StreamExt;

        let Some(Ok(event)) = stream.next().await else {
            tracing::warn!("inotify event stream ended");
            break;
        };

        let Some(name) = event.name.and_then(|n| n.to_str().map(String::from)) else {
            continue;
        };

        if !name.starts_with("video") {
            continue;
        }

        let path = format!("/dev/{name}");

        let hotplug_event = if event.mask.contains(EventMask::CREATE) {
            tracing::info!(device = %path, "video device added");
            HotplugEvent::Added(path)
        } else if event.mask.contains(EventMask::DELETE) {
            tracing::info!(device = %path, "video device removed");
            HotplugEvent::Removed(path)
        } else {
            continue;
        };

        if event_tx.send(hotplug_event).await.is_err() {
            break; // receiver dropped
        }
    }
}

/// No-op on non-Linux platforms. Blocks forever (never sends events).
#[cfg(not(target_os = "linux"))]
pub async fn watch_dev_video(_event_tx: tokio::sync::mpsc::Sender<HotplugEvent>) {
    // Camera hot-plug monitoring requires Linux inotify.
    futures::future::pending::<()>().await;
}
