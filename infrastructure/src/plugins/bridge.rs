use super::{HostMessage, WorkerReply};
use crate::{terminal::PtyRuntime, wire};
use anyhow::{Context, Result, bail};
use application::{Application, ArtifactPublished, ResolveArtifact};
use std::{
    env,
    io::BufReader,
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

pub(crate) struct Plugins {
    events: SyncSender<ArtifactPublished>,
    dropped: Arc<AtomicU64>,
}

impl Plugins {
    pub(crate) fn start(application: Arc<Application<PtyRuntime>>) -> Option<Self> {
        let directory = env::var_os("POWER_PLUGIN_DIR")?;
        if directory.is_empty() {
            return None;
        }
        let (events, pending) = mpsc::sync_channel(32);
        let dropped = Arc::new(AtomicU64::new(0));
        let overflow = dropped.clone();
        thread::spawn(move || {
            let mut worker = None;
            let mut retry_after = None;
            for event in pending {
                if retry_after.is_some_and(|deadline| Instant::now() < deadline) {
                    overflow.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let count = overflow.swap(0, Ordering::Relaxed);
                if count > 0 {
                    eprintln!("Dropped {count} plugin notifications");
                }
                let result = (|| {
                    if worker.is_none() {
                        worker = Some(Worker::start(Path::new(&directory))?);
                    }
                    worker.as_mut().unwrap().dispatch(event, &application)
                })();
                if let Err(error) = result {
                    eprintln!("Plugin host failed. {error:#}");
                    worker = None;
                    retry_after = Some(Instant::now() + Duration::from_secs(1));
                }
            }
        });
        Some(Self { events, dropped })
    }

    pub(crate) fn notify(&self, event: ArtifactPublished) {
        if self.events.try_send(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct Worker {
    child: Child,
    connection: BufReader<UnixStream>,
}

impl Worker {
    fn start(directory: &Path) -> Result<Self> {
        let started_at = Instant::now();
        let executable = env::current_exe()?.with_file_name("power-plugin-host");
        let (daemon, worker) = UnixStream::pair()?;
        daemon.set_read_timeout(Some(Duration::from_secs(30)))?;
        daemon.set_write_timeout(Some(Duration::from_secs(3)))?;
        let input = OwnedFd::from(worker.try_clone()?);
        let output = OwnedFd::from(worker);
        let child = Command::new(&executable)
            .arg(directory)
            .stdin(Stdio::from(input))
            .stdout(Stdio::from(output))
            .spawn()
            .with_context(|| format!("Start {}", executable.display()))?;
        let mut started = Self {
            child,
            connection: BufReader::new(daemon),
        };
        if !matches!(
            wire::receive(&mut started.connection)?,
            Some(WorkerReply::Ready)
        ) {
            bail!("Plugin host disconnected during startup");
        }
        started
            .connection
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(3)))?;
        eprintln!(
            "Plugin host ready in {:.1} ms",
            started_at.elapsed().as_secs_f64() * 1000.0
        );
        Ok(started)
    }

    fn dispatch(
        &mut self,
        event: ArtifactPublished,
        application: &Application<PtyRuntime>,
    ) -> Result<()> {
        wire::send(
            self.connection.get_ref(),
            &HostMessage::ArtifactPublished {
                uri: event.uri.clone(),
            },
        )?;
        let mut resolved = false;
        loop {
            match wire::receive(&mut self.connection)? {
                Some(WorkerReply::ResolvePublished) => {
                    if resolved {
                        bail!("Duplicate plugin artifact query");
                    }
                    resolved = true;
                    let result = application
                        .resolve_artifact(ResolveArtifact {
                            session: event.session.clone(),
                            target: event.uri.clone(),
                        })
                        .map(super::PluginArtifact::from)
                        .map_err(|error| error.to_string())
                        .and_then(|artifact| {
                            if artifact.title.len() + artifact.location.len() <= 32 * 1024 {
                                Ok(artifact)
                            } else {
                                Err("Artifact metadata exceeds the plugin size limit".into())
                            }
                        });
                    wire::send(
                        self.connection.get_ref(),
                        &HostMessage::ArtifactResolved(result),
                    )?;
                }
                Some(WorkerReply::Finished) => return Ok(()),
                Some(WorkerReply::Ready) => bail!("Unexpected plugin host message"),
                None => bail!("Plugin host disconnected"),
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
