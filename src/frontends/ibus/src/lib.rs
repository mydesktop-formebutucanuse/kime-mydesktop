//! kime as an IBus implementation.
//!
//! Applications built against IBus — `GTK_IM_MODULE=ibus`,
//! `QT_IM_MODULE=ibus`, and the GTK/Qt runtimes bundled in flatpak and snap
//! packages, which cannot load kime's own modules — find the IBus daemon via
//! the address file (see [`address`]) or, inside a sandbox, via the
//! `org.freedesktop.portal.IBus` name on the session bus. `kime-ibus` owns
//! both IBus names on the session bus and points the address file at it, so
//! those clients reach kime's engine with no `ibus-daemon` running.

pub mod address;
pub mod bus;
pub mod context;
pub mod session;
pub mod text;

use std::sync::Arc;

use futures_util::StreamExt;
use kime_engine_core::Config;
use zbus::{
    fdo::{DBusProxy, RequestNameFlags, RequestNameReply},
    names::OwnedUniqueName,
    Connection,
};

/// `g_dbus_address_get_for_bus_sync(G_BUS_TYPE_SESSION)` minus autolaunch.
pub fn session_bus_address() -> Option<String> {
    if let Some(address) = std::env::var("DBUS_SESSION_BUS_ADDRESS")
        .ok()
        .filter(|a| !a.is_empty())
    {
        return Some(address);
    }

    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let socket = std::path::Path::new(&runtime_dir).join("bus");
    socket
        .exists()
        .then(|| format!("unix:path={}", socket.display()))
}

pub struct Server {
    pub connection: Connection,
    pub registry: Arc<bus::Registry>,
}

impl Server {
    /// Connect to `address`, export the IBus objects and claim the IBus
    /// names. Fails if another IBus implementation already owns
    /// `org.freedesktop.IBus`; a taken portal name is only logged.
    pub async fn start(address: &str, config: Arc<Config>) -> zbus::Result<Self> {
        let connection = zbus::connection::Builder::address(address)?.build().await?;
        let registry = bus::Registry::new(config);

        let server = connection.object_server();
        server
            .at(
                bus::IBUS_PATH,
                bus::IBus {
                    registry: registry.clone(),
                },
            )
            .await?;
        server
            .at(
                bus::IBUS_PATH,
                bus::Portal {
                    registry: registry.clone(),
                },
            )
            .await?;

        let flags = RequestNameFlags::DoNotQueue.into();

        match connection
            .request_name_with_flags(bus::IBUS_SERVICE, flags)
            .await?
        {
            RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => {}
            _ => {
                return Err(zbus::Error::NameTaken);
            }
        }

        match connection
            .request_name_with_flags(bus::PORTAL_SERVICE, flags)
            .await
        {
            Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
            other => log::warn!(
                "Can't own {} ({:?}): sandboxed applications keep using ibus-portal",
                bus::PORTAL_SERVICE,
                other
            ),
        }

        Ok(Self {
            connection,
            registry,
        })
    }

    /// Drop the contexts of clients that disconnect without destroying them.
    /// Returns when the bus connection is gone.
    pub async fn reap_contexts(&self) -> zbus::Result<()> {
        let dbus = DBusProxy::new(&self.connection).await?;
        let mut changes = dbus.receive_name_owner_changed().await?;

        while let Some(change) = changes.next().await {
            let Ok(args) = change.args() else {
                continue;
            };

            if args.new_owner().is_some() {
                continue;
            }

            if let zbus::names::BusName::Unique(name) = args.name() {
                let name: OwnedUniqueName = name.to_owned().into();
                self.registry
                    .owner_vanished(self.connection.object_server(), &name)
                    .await;
            }
        }

        Ok(())
    }
}
