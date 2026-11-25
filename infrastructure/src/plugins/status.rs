use super::{
    HostMessage, WorkerReply,
    host::{CALL_EPOCHS, components, engine, load_component, store_limits},
    valid_status_field,
};
use crate::wire;
use anyhow::{Result, bail};
use std::{
    collections::BTreeMap,
    io::{self, BufReader},
    path::PathBuf,
};
use wasmtime::{
    Store, StoreLimits,
    component::{HasSelf, Linker},
};

wasmtime::component::bindgen!({
    path: "wit/status-bar.wit",
    world: "status-bar-plugin",
});

impl power::status_bar::types::Host for StoreLimits {}

pub fn run_status_bar_host(directory: PathBuf) -> Result<()> {
    let (engine, execution) = engine()?;
    let mut linker = Linker::new(&engine);
    StatusBarPlugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
    let mut plugins = Vec::new();
    execution.send(())?;
    for path in components(&directory)? {
        let mut store = Store::new(&engine, store_limits());
        store.limiter(|limits: &mut StoreLimits| limits);
        store.set_epoch_deadline(CALL_EPOCHS);
        let plugin = load_component(&path, &engine).and_then(|component| {
            Ok(StatusBarPlugin::instantiate(
                &mut store, &component, &linker,
            )?)
        });
        match plugin {
            Ok(api) => plugins.push((path, store, api)),
            Err(error) => eprintln!("Could not load status plugin {}. {error:#}", path.display()),
        }
    }
    execution.send(())?;
    wire::send(io::stdout(), &WorkerReply::Ready)?;
    let mut reader = BufReader::new(io::stdin());
    while let Some(message) = wire::receive(&mut reader)? {
        let HostMessage::ActiveView(view) = message else {
            bail!("Unexpected status plugin message");
        };
        let view = ActiveView {
            session: view.session,
            worktree_name: view.worktree_name,
            worktree_path: view.worktree_path,
            repository_path: view.repository_path,
            agent_type: view.agent_type,
            agent_task: view.agent_task,
            agent_model: view.agent_model,
        };
        let mut fields = BTreeMap::new();
        execution.send(())?;
        plugins.retain_mut(|(path, store, api)| {
            store.set_epoch_deadline(CALL_EPOCHS);
            match api.call_on_active_view(store, &view) {
                Ok(contributed) => {
                    fields.extend(contributed.into_iter().take(32).filter_map(|field| {
                        (valid_status_field(&field.name)
                            && field.value.len() <= 256
                            && !field.value.trim().is_empty()
                            && !field.value.chars().any(char::is_control))
                        .then_some((field.name, field.value))
                    }));
                    true
                }
                Err(error) => {
                    eprintln!("Disabled status plugin {}. {error:#}", path.display());
                    false
                }
            }
        });
        execution.send(())?;
        wire::send(io::stdout(), &WorkerReply::StatusFields(fields))?;
    }
    Ok(())
}
