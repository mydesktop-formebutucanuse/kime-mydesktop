//! The IBus daemon objects on `/org/freedesktop/IBus` and the bookkeeping of
//! the input contexts they create.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use kime_engine_core::Config;
use zbus::{
    fdo, interface,
    message::Header,
    names::OwnedUniqueName,
    zvariant::{ObjectPath, OwnedObjectPath},
    Connection, ObjectServer,
};

use crate::context::{check_sender, InputContext};

/// `IBUS_SERVICE_IBUS`: what `GTK_IM_MODULE=ibus`/`QT_IM_MODULE=ibus` talk
/// to on the bus named by the address file.
pub const IBUS_SERVICE: &str = "org.freedesktop.IBus";
/// `IBUS_SERVICE_PORTAL`: what sandboxed (flatpak, snap) IBus clients talk
/// to on the session bus. `org.freedesktop.portal.*` is reachable from
/// inside the sandbox.
pub const PORTAL_SERVICE: &str = "org.freedesktop.portal.IBus";
/// `IBUS_PATH_IBUS`
pub const IBUS_PATH: &str = "/org/freedesktop/IBus";

pub struct Registry {
    config: Arc<Config>,
    next_id: AtomicU32,
    contexts: Mutex<HashMap<OwnedUniqueName, Vec<OwnedObjectPath>>>,
}

impl Registry {
    pub fn new(config: Arc<Config>) -> Arc<Self> {
        Arc::new(Self {
            config,
            next_id: AtomicU32::new(1),
            contexts: Mutex::new(HashMap::new()),
        })
    }

    async fn create(
        self: &Arc<Self>,
        server: &ObjectServer,
        header: &Header<'_>,
        client_name: &str,
    ) -> fdo::Result<OwnedObjectPath> {
        let owner: OwnedUniqueName = header
            .sender()
            .ok_or_else(|| fdo::Error::AccessDenied("Anonymous sender".into()))?
            .to_owned()
            .into();

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // `IBUS_PATH_INPUT_CONTEXT`
        let path = OwnedObjectPath::try_from(format!("{IBUS_PATH}/InputContext_{id}"))
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;

        log::info!(
            "Create input context {} for {} ({})",
            path.as_str(),
            client_name,
            owner
        );

        let context = InputContext::new(owner.clone(), self.clone(), self.config.clone());
        let service = Service {
            owner: owner.clone(),
            registry: self.clone(),
        };
        server.at(&path, context).await?;
        server.at(&path, service).await?;

        self.contexts
            .lock()
            .unwrap()
            .entry(owner)
            .or_default()
            .push(path.clone());

        Ok(path)
    }

    async fn remove_objects(server: &ObjectServer, path: &ObjectPath<'_>) {
        log::info!("Destroy input context {}", path.as_str());
        // Dropping the context drops its engine, which closes a hanja
        // candidate window it may still own.
        server.remove::<InputContext, _>(path).await.ok();
        server.remove::<Service, _>(path).await.ok();
    }

    pub async fn destroy(&self, server: &ObjectServer, path: &ObjectPath<'_>) {
        for paths in self.contexts.lock().unwrap().values_mut() {
            paths.retain(|p| p.as_ref() != *path);
        }

        Self::remove_objects(server, path).await;
    }

    /// Destroy `path` once the call asking for it has been answered: the
    /// object server is still dispatching into the object being removed.
    pub fn destroy_later(self: &Arc<Self>, conn: &Connection, path: OwnedObjectPath) {
        let registry = self.clone();
        let conn = conn.clone();
        tokio::spawn(async move { registry.destroy(conn.object_server(), &path).await });
    }

    /// A client left the bus without destroying its contexts (crash, exit).
    pub async fn owner_vanished(&self, server: &ObjectServer, owner: &OwnedUniqueName) {
        let paths = self.contexts.lock().unwrap().remove(owner);

        for path in paths.into_iter().flatten() {
            Self::remove_objects(server, &path).await;
        }
    }
}

/// `org.freedesktop.IBus.Service`, which every IBus object implements;
/// `ibus_proxy_destroy()` calls its `Destroy` on input contexts.
pub struct Service {
    owner: OwnedUniqueName,
    registry: Arc<Registry>,
}

#[interface(name = "org.freedesktop.IBus.Service", spawn = false)]
impl Service {
    async fn destroy(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<()> {
        check_sender(&self.owner, Some(&header))?;
        if let Some(path) = header.path() {
            self.registry.destroy_later(conn, path.to_owned().into());
        }
        Ok(())
    }
}

/// `org.freedesktop.IBus`, the daemon interface regular clients use.
pub struct IBus {
    pub registry: Arc<Registry>,
}

#[interface(name = "org.freedesktop.IBus", spawn = false)]
impl IBus {
    async fn create_input_context(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &ObjectServer,
        client_name: String,
    ) -> fdo::Result<OwnedObjectPath> {
        self.registry.create(server, &header, &client_name).await
    }
}

/// `org.freedesktop.IBus.Portal`, what `ibus-portal` offers to sandboxed
/// clients. kime serves it directly instead of proxying to a daemon.
pub struct Portal {
    pub registry: Arc<Registry>,
}

#[interface(name = "org.freedesktop.IBus.Portal", spawn = false)]
impl Portal {
    async fn create_input_context(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &ObjectServer,
        client_name: String,
    ) -> fdo::Result<OwnedObjectPath> {
        self.registry.create(server, &header, &client_name).await
    }
}
