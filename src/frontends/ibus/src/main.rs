use std::sync::Arc;

use kime_ibus::{address, session_bus_address, Server};
use tokio::signal::unix::{signal, SignalKind};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    kime_version::cli_boilerplate!((),);

    if let Err(err) = run().await {
        log::error!("{}", err);
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let address_file = address::address_file_path();

    if let Some(pid) = address::live_foreign_daemon(&address_file) {
        return Err(format!(
            "An IBus daemon (pid {}) already serves {}; not starting",
            pid,
            address_file.display()
        ));
    }

    let bus_address =
        session_bus_address().ok_or("No session bus: DBUS_SESSION_BUS_ADDRESS is unset")?;

    let config =
        Arc::new(kime_engine_core::load_engine_config_from_config_dir().unwrap_or_default());

    let server = Server::start(&bus_address, config)
        .await
        .map_err(|err| match err {
            zbus::Error::NameTaken => {
                "org.freedesktop.IBus is already owned on the session bus; not starting".to_owned()
            }
            err => format!("Can't start IBus server: {}", err),
        })?;

    address::write(&address_file, &bus_address)
        .map_err(|err| format!("Can't write {}: {}", address_file.display(), err))?;

    log::info!(
        "Server init success! IBus address file: {}",
        address_file.display()
    );

    let mut sigterm = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut sigint = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;

    let result = tokio::select! {
        res = server.reap_contexts() => match res {
            Ok(()) => Err("Session bus connection closed".to_owned()),
            Err(err) => Err(format!("Session bus error: {}", err)),
        },
        _ = sigterm.recv() => Ok(()),
        _ = sigint.recv() => Ok(()),
    };

    address::remove_if_ours(&address_file);

    result
}
