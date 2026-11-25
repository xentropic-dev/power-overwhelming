use super::{HostMessage, PluginArtifact, PluginTargetKind, WorkerReply};
use crate::wire;
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{self, File},
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};
use wasmtime::{
    Cache, Config, Engine, Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, HasSelf, Linker},
};

wasmtime::component::bindgen!({
    path: "wit/artifact-plugin.wit",
    world: "artifact-plugin",
});

use power::plugins::artifacts::{Artifact, Host, TargetKind};

const MAX_PLUGINS: usize = 8;
const MAX_COMPONENT_SIZE: u64 = 8 * 1024 * 1024;
const EPOCH_INTERVAL: Duration = Duration::from_millis(10);
pub(super) const CALL_EPOCHS: u64 = 10;

pub(super) fn engine() -> Result<(Engine, mpsc::Sender<()>)> {
    let mut config = Config::new();
    config
        .wasm_component_model(true)
        .epoch_interruption(true)
        .wasm_backtrace_max_frames(None);
    match Cache::from_file(None) {
        Ok(cache) => {
            config.cache(Some(cache));
        }
        Err(error) => eprintln!("Could not configure the plugin compilation cache. {error:#}"),
    }
    let engine = Engine::new(&config)?;
    let clock = engine.clone();
    let (execution, ticks) = mpsc::channel();
    thread::spawn(move || {
        // Consecutive messages arm and disarm the watchdog.
        while ticks.recv().is_ok() {
            while let Err(mpsc::RecvTimeoutError::Timeout) = ticks.recv_timeout(EPOCH_INTERVAL) {
                clock.increment_epoch();
            }
        }
    });
    Ok((engine, execution))
}

pub fn run_host(directory: PathBuf) -> Result<()> {
    let (engine, execution) = engine()?;
    let mut linker = Linker::new(&engine);
    ArtifactPlugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
    let connection = Arc::new(Mutex::new(Connection {
        reader: BufReader::new(io::stdin()),
        writer: io::stdout(),
        active: false,
        resolved: None,
    }));
    let mut plugins = Vec::new();
    execution.send(())?;
    for path in components(&directory)? {
        match Plugin::load(&path, &engine, &linker, connection.clone()) {
            Ok(plugin) => plugins.push(plugin),
            Err(error) => eprintln!("Could not load plugin {}. {error:#}", path.display()),
        }
    }

    execution.send(())?;
    connection.lock().unwrap().send(&WorkerReply::Ready)?;
    loop {
        let message = wire::receive(&mut connection.lock().unwrap().reader)?;
        let uri = match message {
            Some(HostMessage::ArtifactPublished { uri }) => uri,
            Some(_) => bail!("Unexpected artifact response"),
            None => return Ok(()),
        };
        connection.lock().unwrap().active = true;
        execution.send(())?;
        plugins.retain_mut(|plugin| {
            plugin.store.data_mut().queried = false;
            plugin.store.set_epoch_deadline(CALL_EPOCHS);
            match plugin
                .api
                .call_on_artifact_published(&mut plugin.store, &uri)
            {
                Ok(()) => true,
                Err(error) => {
                    eprintln!("Disabled plugin {}. {error:#}", plugin.path.display());
                    false
                }
            }
        });
        execution.send(())?;
        let mut connection = connection.lock().unwrap();
        connection.active = false;
        connection.resolved = None;
        connection.send(&WorkerReply::Finished)?;
    }
}

pub(super) fn components(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory).with_context(|| format!("Read {}", directory.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|part| part == "wasm") && entry.file_type()?.is_file() {
            ensure!(
                paths.len() < MAX_PLUGINS,
                "At most {MAX_PLUGINS} plugins are supported"
            );
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

pub(super) fn load_component(path: &Path, engine: &Engine) -> Result<Component> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_COMPONENT_SIZE + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_COMPONENT_SIZE,
        "Plugin exceeds the file size limit"
    );
    Ok(Component::from_binary(engine, &bytes)?)
}

pub(super) fn store_limits() -> StoreLimits {
    StoreLimitsBuilder::new()
        .memory_size(16 * 1024 * 1024)
        .memories(2)
        .tables(4)
        .table_elements(4_096)
        .instances(4)
        .build()
}

struct Plugin {
    path: PathBuf,
    store: Store<HostState>,
    api: ArtifactPlugin,
}

impl Plugin {
    fn load(
        path: &Path,
        engine: &Engine,
        linker: &Linker<HostState>,
        connection: Arc<Mutex<Connection>>,
    ) -> Result<Self> {
        let component = load_component(path, engine)?;
        let mut store = Store::new(
            engine,
            HostState {
                connection,
                queried: false,
                limits: store_limits(),
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_epoch_deadline(CALL_EPOCHS);
        let api = ArtifactPlugin::instantiate(&mut store, &component, linker)?;
        Ok(Self {
            path: path.to_owned(),
            store,
            api,
        })
    }
}

struct HostState {
    connection: Arc<Mutex<Connection>>,
    queried: bool,
    limits: StoreLimits,
}

impl Host for HostState {
    fn resolve_published(&mut self) -> Result<Artifact, String> {
        let mut connection = self.connection.lock().unwrap();
        if !connection.active || self.queried {
            return Err("Artifact resolution is available once per notification".into());
        }
        self.queried = true;
        if connection.resolved.is_none() {
            connection.resolved = Some(
                connection
                    .resolve()
                    .map_err(|error| error.to_string())
                    .and_then(|result| result),
            );
        }
        match connection.resolved.as_ref().unwrap() {
            Ok(artifact) => Ok(Artifact {
                title: artifact.title.clone(),
                kind: match artifact.kind {
                    PluginTargetKind::Image => TargetKind::Image,
                    PluginTargetKind::Video => TargetKind::Video,
                    PluginTargetKind::Html => TargetKind::Html,
                    PluginTargetKind::Loopback => TargetKind::Loopback,
                },
                location: artifact.location.clone(),
            }),
            Err(error) => Err(error.clone()),
        }
    }
}

struct Connection {
    reader: BufReader<io::Stdin>,
    writer: io::Stdout,
    active: bool,
    resolved: Option<Result<PluginArtifact, String>>,
}

impl Connection {
    fn send(&mut self, reply: &WorkerReply) -> Result<()> {
        wire::send(&mut self.writer, reply)?;
        Ok(self.writer.flush()?)
    }

    fn resolve(&mut self) -> Result<Result<PluginArtifact, String>> {
        self.send(&WorkerReply::ResolvePublished)?;
        match wire::receive(&mut self.reader)? {
            Some(HostMessage::ArtifactResolved(result)) => Ok(result),
            _ => bail!("Missing artifact response from daemon"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn calls_the_artifact_import_and_interrupts_a_spinning_plugin() -> Result<()> {
        let engine = Engine::new(Config::new().epoch_interruption(true))?;
        let mut linker = Linker::new(&engine);
        ArtifactPlugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
        let connection = Arc::new(Mutex::new(Connection {
            reader: BufReader::new(io::stdin()),
            writer: io::stdout(),
            active: true,
            resolved: Some(Ok(PluginArtifact {
                title: "Existing preview".into(),
                kind: PluginTargetKind::Image,
                location: "/tmp/preview.png".into(),
            })),
        }));
        let fixtures = tempfile::tempdir()?;
        let path = fixtures.path().join("fixture.wasm");
        let component = include_str!("../../tests/fixtures/artifact-plugin.wat");
        fs::write(&path, wat::parse_str(component)?)?;
        let mut plugin = Plugin::load(&path, &engine, &linker, connection.clone())?;
        plugin
            .api
            .call_on_artifact_published(&mut plugin.store, "pwr://artifact/example")?;
        assert!(plugin.store.data().queried);

        let spinning =
            component.replace("(call $resolve (i32.const 0))", "(loop $spin (br $spin))");
        fs::write(&path, wat::parse_str(spinning)?)?;
        let mut plugin = Plugin::load(&path, &engine, &linker, connection)?;
        plugin.store.set_epoch_deadline(1);
        let done = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    engine.increment_epoch();
                    thread::sleep(EPOCH_INTERVAL);
                }
            });
            let result = plugin
                .api
                .call_on_artifact_published(&mut plugin.store, "pwr://artifact/example");
            done.store(true, Ordering::Relaxed);
            assert!(matches!(
                result.unwrap_err().downcast_ref::<wasmtime::Trap>(),
                Some(wasmtime::Trap::Interrupt)
            ));
        });
        Ok(())
    }
}
