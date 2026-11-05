//! `org.freedesktop.IBus.InputContext` objects.

use std::sync::{Arc, Mutex};

use kime_engine_core::Config;
use zbus::{
    fdo, interface,
    message::Header,
    names::{BusName, OwnedUniqueName},
    object_server::SignalEmitter,
    zvariant::{OwnedValue, Value},
    Connection,
};

use crate::bus::Registry;
use crate::session::{Event, Session};
use crate::text;

/// `IBUS_ENGINE_PREEDIT_CLEAR`: the client drops the preedit on focus-out or
/// reset instead of committing it itself. kime commits it on `FocusOut` and
/// `Reset` like every other frontend does, so it must not be committed twice.
const PREEDIT_MODE_CLEAR: u32 = 0;

/// One element of the `PostProcessKeyEvent` queue: a type byte plus an
/// `IBusText` (`bus_input_context_make_post_process_key_event()`).
type PostEvent = (u8, OwnedValue);

pub struct InputContext {
    owner: OwnedUniqueName,
    registry: Arc<Registry>,
    session: Session,
    content_type: (u32, u32),
    /// Client asked for `UpdatePreeditTextWithMode` (`ClientCommitPreedit`).
    client_commit_preedit: bool,
    /// Client fetches the outcome of `ProcessKeyEvent` from the
    /// `PostProcessKeyEvent` property instead of from signals — GTK4's
    /// default synchronous mode, where signals would only arrive after the
    /// client already handed the key to the widget.
    post_process: bool,
    queue: Mutex<Vec<PostEvent>>,
}

pub(crate) fn check_sender(
    owner: &OwnedUniqueName,
    header: Option<&Header<'_>>,
) -> fdo::Result<()> {
    // Contexts live on the shared session bus: only the client that created
    // one may drive it (or read the text typed into it).
    match header.and_then(|h| h.sender()) {
        Some(sender) if *sender == *owner => Ok(()),
        _ => Err(fdo::Error::AccessDenied("Access denied".into())),
    }
}

fn owned(value: Value<'static>) -> OwnedValue {
    OwnedValue::try_from(value).expect("IBusText holds no file descriptors")
}

impl InputContext {
    pub fn new(owner: OwnedUniqueName, registry: Arc<Registry>, config: Arc<Config>) -> Self {
        Self {
            owner,
            registry,
            session: Session::new(config),
            content_type: (0, 0),
            client_commit_preedit: false,
            post_process: false,
            queue: Mutex::new(Vec::new()),
        }
    }

    fn queue_events(&self, events: Vec<Event>) {
        let mut queue = self.queue.lock().unwrap();

        for event in events {
            match event {
                Event::Commit(s) => queue.push((b'c', owned(text::plain(&s)))),
                Event::Preedit(s) => {
                    let cursor = s.chars().count();
                    let visible = u32::from(!s.is_empty());
                    let (kind, position) = if self.client_commit_preedit {
                        (b'm', format!("{cursor},{visible},{PREEDIT_MODE_CLEAR}"))
                    } else {
                        (b'u', format!("{cursor},{visible}"))
                    };
                    queue.push((kind, owned(text::preedit(&s))));
                    queue.push((kind, owned(text::plain(&position))));
                }
            }
        }
    }

    async fn emit_events(
        &self,
        emitter: &SignalEmitter<'_>,
        events: Vec<Event>,
    ) -> fdo::Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        // Unicast: the session bus must not broadcast what the user types.
        let emitter = emitter
            .clone()
            .set_destination(BusName::Unique(self.owner.as_ref()));

        for event in events {
            match event {
                Event::Commit(s) => Self::commit_text(&emitter, text::plain(&s)).await?,
                Event::Preedit(s) => {
                    let cursor = s.chars().count() as u32;
                    let visible = !s.is_empty();
                    let preedit = text::preedit(&s);
                    if self.client_commit_preedit {
                        Self::update_preedit_text_with_mode(
                            &emitter,
                            preedit,
                            cursor,
                            visible,
                            PREEDIT_MODE_CLEAR,
                        )
                        .await?
                    } else {
                        Self::update_preedit_text(&emitter, preedit, cursor, visible).await?
                    }
                }
            }
        }

        Ok(())
    }
}

// `spawn = false`: calls are handled one at a time in arrival order. Key
// events must reach the engine in the order they were typed, and the signals
// a call emits must precede its reply.
#[interface(name = "org.freedesktop.IBus.InputContext", spawn = false)]
impl InputContext {
    async fn process_key_event(
        &mut self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        keyval: u32,
        keycode: u32,
        state: u32,
    ) -> fdo::Result<bool> {
        check_sender(&self.owner, Some(&header))?;

        let (consumed, events) = self.session.process_key_event(keyval, keycode, state);

        if self.post_process {
            // Leftovers belong to a key whose outcome the client never read.
            self.queue.lock().unwrap().clear();
            self.queue_events(events);
        } else {
            self.emit_events(&emitter, events).await?;
        }

        Ok(consumed)
    }

    async fn focus_in(
        &mut self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        check_sender(&self.owner, Some(&header))?;
        let events = self.session.focus_in();
        self.emit_events(&emitter, events).await
    }

    async fn focus_out(
        &mut self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        check_sender(&self.owner, Some(&header))?;
        let events = self.session.focus_out();
        self.emit_events(&emitter, events).await
    }

    async fn reset(
        &mut self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        check_sender(&self.owner, Some(&header))?;
        let events = self.session.reset();
        self.emit_events(&emitter, events).await
    }

    // The preedit is always sent (clients without IBUS_CAP_PREEDIT_TEXT have
    // nowhere else to show it: there is no IBus panel), the client draws it,
    // and the hanja candidate window places itself — so capabilities, the
    // cursor location and the surrounding text are accepted but not needed.

    async fn set_capabilities(
        &self,
        #[zbus(header)] header: Header<'_>,
        _caps: u32,
    ) -> fdo::Result<()> {
        check_sender(&self.owner, Some(&header))
    }

    async fn set_cursor_location(
        &self,
        #[zbus(header)] header: Header<'_>,
        _x: i32,
        _y: i32,
        _w: i32,
        _h: i32,
    ) -> fdo::Result<()> {
        check_sender(&self.owner, Some(&header))
    }

    async fn set_cursor_location_relative(
        &self,
        #[zbus(header)] header: Header<'_>,
        _x: i32,
        _y: i32,
        _w: i32,
        _h: i32,
    ) -> fdo::Result<()> {
        check_sender(&self.owner, Some(&header))
    }

    async fn set_surrounding_text(
        &self,
        #[zbus(header)] header: Header<'_>,
        _text: Value<'_>,
        _cursor_pos: u32,
        _anchor_pos: u32,
    ) -> fdo::Result<()> {
        check_sender(&self.owner, Some(&header))
    }

    async fn property_activate(
        &self,
        #[zbus(header)] header: Header<'_>,
        _name: String,
        _state: u32,
    ) -> fdo::Result<()> {
        check_sender(&self.owner, Some(&header))
    }

    /// kime is the only engine; switching engines is not supported.
    async fn set_engine(
        &self,
        #[zbus(header)] header: Header<'_>,
        _name: String,
    ) -> fdo::Result<()> {
        check_sender(&self.owner, Some(&header))
    }

    async fn get_engine(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<OwnedValue> {
        check_sender(&self.owner, Some(&header))?;
        Err(fdo::Error::Failed("kime-ibus has no IBus engines".into()))
    }

    // `ibus_proxy_destroy()` calls `org.freedesktop.IBus.Service.Destroy`
    // (see `crate::bus::Service`); Qt's IBus plugin calls it here.
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

    /// Drains the queue `ProcessKeyEvent` filled. ibus introspects this as
    /// `(a(yv))` but `_ic_get_post_process_key_event()` returns, and
    /// `ibus_input_context_post_process_key_event()` iterates, a bare
    /// `a(yv)` — a tuple here makes the client read nothing.
    #[zbus(property(emits_changed_signal = "false"))]
    async fn post_process_key_event(
        &self,
        #[zbus(header)] header: Option<Header<'_>>,
    ) -> fdo::Result<Vec<PostEvent>> {
        check_sender(&self.owner, header.as_ref())?;
        Ok(std::mem::take(&mut *self.queue.lock().unwrap()))
    }

    #[zbus(property(emits_changed_signal = "false"))]
    async fn content_type(&self) -> (u32, u32) {
        self.content_type
    }

    #[zbus(property)]
    async fn set_content_type(
        &mut self,
        #[zbus(header)] header: Option<Header<'_>>,
        value: (u32, u32),
    ) -> fdo::Result<()> {
        check_sender(&self.owner, header.as_ref())?;
        self.content_type = value;
        Ok(())
    }

    #[zbus(property(emits_changed_signal = "false"))]
    async fn client_commit_preedit(&self) -> (bool,) {
        (self.client_commit_preedit,)
    }

    #[zbus(property)]
    async fn set_client_commit_preedit(
        &mut self,
        #[zbus(header)] header: Option<Header<'_>>,
        value: (bool,),
    ) -> fdo::Result<()> {
        check_sender(&self.owner, header.as_ref())?;
        self.client_commit_preedit = value.0;
        Ok(())
    }

    #[zbus(property(emits_changed_signal = "false"))]
    async fn effective_post_process_key_event(&self) -> (bool,) {
        (self.post_process,)
    }

    #[zbus(property)]
    async fn set_effective_post_process_key_event(
        &mut self,
        #[zbus(header)] header: Option<Header<'_>>,
        value: (bool,),
    ) -> fdo::Result<()> {
        check_sender(&self.owner, header.as_ref())?;
        self.post_process = value.0;
        Ok(())
    }

    #[zbus(signal)]
    async fn commit_text(emitter: &SignalEmitter<'_>, text: Value<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn update_preedit_text(
        emitter: &SignalEmitter<'_>,
        text: Value<'_>,
        cursor_pos: u32,
        visible: bool,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn update_preedit_text_with_mode(
        emitter: &SignalEmitter<'_>,
        text: Value<'_>,
        cursor_pos: u32,
        visible: bool,
        mode: u32,
    ) -> zbus::Result<()>;
}
