//! kime-ibus on a private `dbus-daemon`, driven over the wire the way the
//! GTK/Qt IBus clients drive it. Skipped when `dbus-daemon` is not installed
//! (the nix package sandbox).

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use kime_engine_core::{Config, InputCategory};
use kime_ibus::Server;
use zbus::{
    message::Type as MessageType,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
    Connection, Message, MessageStream,
};

const IC_IFACE: &str = "org.freedesktop.IBus.InputContext";

// evdev codes: r = ㄱ, k = ㅏ on dubeolsik
const KEY_R: u32 = 19;
const KEY_K: u32 = 37;

struct Bus {
    daemon: Child,
    address: String,
    _dir: TempDir,
}

impl Drop for Bus {
    fn drop(&mut self) {
        self.daemon.kill().ok();
        self.daemon.wait().ok();
    }
}

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn start_bus(name: &str) -> Option<Bus> {
    let dir = std::env::temp_dir().join(format!("kime-ibus-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = TempDir(dir);

    let config = dir.0.join("bus.conf");
    std::fs::write(
        &config,
        format!(
            "<busconfig>\
               <type>session</type>\
               <listen>unix:dir={}</listen>\
               <auth>EXTERNAL</auth>\
               <policy context=\"default\">\
                 <allow send_destination=\"*\"/>\
                 <allow receive_sender=\"*\"/>\
                 <allow own=\"*\"/>\
               </policy>\
             </busconfig>",
            dir.0.display()
        ),
    )
    .unwrap();

    let mut daemon = match Command::new("dbus-daemon")
        .arg(format!("--config-file={}", config.display()))
        .args(["--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(daemon) => daemon,
        Err(err) => {
            eprintln!("skipping: can't run dbus-daemon: {err}");
            return None;
        }
    };

    let mut address = String::new();
    BufReader::new(daemon.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();

    Some(Bus {
        daemon,
        address: address.trim().to_owned(),
        _dir: dir,
    })
}

fn hangul_config() -> Arc<Config> {
    let config = Config {
        default_category: InputCategory::Hangul,
        ..Default::default()
    };
    Arc::new(config)
}

async fn client(bus: &Bus) -> Connection {
    zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap()
}

async fn create_context(conn: &Connection, service: &str, iface: &str) -> OwnedObjectPath {
    conn.call_method(
        Some(service),
        "/org/freedesktop/IBus",
        Some(iface),
        "CreateInputContext",
        &("kime-ibus-test",),
    )
    .await
    .unwrap()
    .body()
    .deserialize()
    .unwrap()
}

async fn call<B>(
    conn: &Connection,
    path: &OwnedObjectPath,
    iface: &str,
    method: &str,
    body: &B,
) -> zbus::Result<Message>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    conn.call_method(
        Some("org.freedesktop.IBus"),
        path,
        Some(iface),
        method,
        body,
    )
    .await
}

async fn press(conn: &Connection, path: &OwnedObjectPath, keycode: u32) -> zbus::Result<bool> {
    call(
        conn,
        path,
        IC_IFACE,
        "ProcessKeyEvent",
        &(0u32, keycode, 0u32),
    )
    .await?
    .body()
    .deserialize()
}

/// The string of a serialized `IBusText`.
fn ibus_text(value: &Value<'_>) -> String {
    match value {
        Value::Value(inner) => ibus_text(inner),
        Value::Structure(s) => {
            assert_eq!(s.signature().to_string(), "(sa{sv}sv)");
            assert_eq!(s.fields()[0], Value::from("IBusText"));
            match &s.fields()[2] {
                Value::Str(text) => text.to_string(),
                other => panic!("IBusText string field is {other:?}"),
            }
        }
        other => panic!("not an IBusText: {other:?}"),
    }
}

/// `(member, text)` of the InputContext signals that arrive within `wait`.
async fn signals(stream: &mut MessageStream, wait: Duration) -> Vec<(String, String)> {
    let mut out = Vec::new();

    while let Ok(Some(Ok(msg))) = tokio::time::timeout(wait, stream.next()).await {
        let header = msg.header();
        if header.message_type() != MessageType::Signal
            || header.interface().map(|i| i.as_str()) != Some(IC_IFACE)
        {
            continue;
        }

        let member = header.member().unwrap().to_string();
        let body = msg.body();
        // zvariant prints a multi-argument body signature as a struct
        let signature = body.signature().to_string();
        let signature = signature.trim_start_matches('(').trim_end_matches(')');
        let text = match member.as_str() {
            "CommitText" => {
                assert_eq!(signature, "v");
                ibus_text(&body.deserialize::<OwnedValue>().unwrap())
            }
            "UpdatePreeditText" => {
                assert_eq!(signature, "vub");
                let (text, cursor, visible) =
                    body.deserialize::<(OwnedValue, u32, bool)>().unwrap();
                let text = ibus_text(&text);
                assert_eq!(cursor as usize, text.chars().count());
                assert_eq!(visible, !text.is_empty());
                text
            }
            other => panic!("unexpected signal {other}"),
        };
        out.push((member, text));
    }

    out
}

fn sig(member: &str, text: &str) -> (String, String) {
    (member.to_owned(), text.to_owned())
}

#[tokio::test]
async fn ibus_protocol() {
    let Some(bus) = start_bus("protocol") else {
        return;
    };
    let server = Arc::new(Server::start(&bus.address, hangul_config()).await.unwrap());
    tokio::spawn({
        let server = server.clone();
        async move { server.reap_contexts().await }
    });

    let a = client(&bus).await;
    let b = client(&bus).await;
    let mut a_signals = MessageStream::from(&a);
    let mut b_signals = MessageStream::from(&b);

    let path = create_context(&a, "org.freedesktop.IBus", "org.freedesktop.IBus").await;

    // Composition reaches the creator as unicast signals before the reply.
    assert!(press(&a, &path, KEY_R).await.unwrap());
    assert!(press(&a, &path, KEY_K).await.unwrap());
    call(&a, &path, IC_IFACE, "FocusOut", &()).await.unwrap();

    assert_eq!(
        signals(&mut a_signals, Duration::from_millis(300)).await,
        [
            sig("UpdatePreeditText", "ㄱ"),
            sig("UpdatePreeditText", "가"),
            sig("UpdatePreeditText", ""),
            sig("CommitText", "가"),
        ]
    );

    // Another client can neither drive the context nor see what is typed.
    let denied = press(&b, &path, KEY_R).await.unwrap_err();
    assert!(denied.to_string().contains("AccessDenied"), "{denied}");
    assert!(signals(&mut b_signals, Duration::from_millis(100))
        .await
        .is_empty());

    // The creator leaving the bus destroys its contexts.
    drop(a_signals);
    a.close().await.unwrap();
    let mut gone = false;
    for _ in 0..50 {
        match call(&b, &path, IC_IFACE, "FocusIn", &()).await {
            Err(err) if err.to_string().contains("UnknownObject") => {
                gone = true;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    assert!(gone, "context outlived its owner");
}

#[tokio::test]
async fn post_process_key_event_queue() {
    let Some(bus) = start_bus("postprocess") else {
        return;
    };
    let _server = Server::start(&bus.address, hangul_config()).await.unwrap();

    // GTK4's default synchronous mode, through the sandbox portal.
    let a = client(&bus).await;
    let mut a_signals = MessageStream::from(&a);
    let path = create_context(
        &a,
        "org.freedesktop.portal.IBus",
        "org.freedesktop.IBus.Portal",
    )
    .await;

    let set = |name: &'static str, value: (bool,)| {
        let a = a.clone();
        let path = path.clone();
        async move {
            call(
                &a,
                &path,
                "org.freedesktop.DBus.Properties",
                "Set",
                &(IC_IFACE, name, Value::from(value)),
            )
            .await
            .unwrap();
        }
    };
    set("ClientCommitPreedit", (true,)).await;
    set("EffectivePostProcessKeyEvent", (true,)).await;

    assert!(press(&a, &path, KEY_R).await.unwrap());

    let reply = call(
        &a,
        &path,
        "org.freedesktop.DBus.Properties",
        "Get",
        &(IC_IFACE, "PostProcessKeyEvent"),
    )
    .await
    .unwrap();
    let value: OwnedValue = reply.body().deserialize().unwrap();
    // ibus' client iterates the property value itself as the array.
    assert_eq!(value.value_signature().to_string(), "a(yv)");

    let events: Vec<(u8, OwnedValue)> = value.try_into().unwrap();
    let events: Vec<(char, String)> = events
        .iter()
        .map(|(kind, text)| (*kind as char, ibus_text(text)))
        .collect();
    assert_eq!(
        events,
        [('m', "ㄱ".to_owned()), ('m', "1,1,0".to_owned())],
        "preedit, then cursor,visible,mode"
    );

    // Nothing went out as a signal, and the queue is drained.
    assert!(signals(&mut a_signals, Duration::from_millis(100))
        .await
        .is_empty());
    let again: OwnedValue = call(
        &a,
        &path,
        "org.freedesktop.DBus.Properties",
        "Get",
        &(IC_IFACE, "PostProcessKeyEvent"),
    )
    .await
    .unwrap()
    .body()
    .deserialize()
    .unwrap();
    assert!(Vec::<(u8, OwnedValue)>::try_from(again).unwrap().is_empty());
}

#[tokio::test]
async fn second_instance_is_refused() {
    let Some(bus) = start_bus("second") else {
        return;
    };
    let _first = Server::start(&bus.address, hangul_config()).await.unwrap();

    assert!(matches!(
        Server::start(&bus.address, hangul_config()).await,
        Err(zbus::Error::NameTaken)
    ));
}
