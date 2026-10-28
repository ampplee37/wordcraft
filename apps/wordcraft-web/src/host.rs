//! The `postMessage` bridge to a same-origin parent page (`?host=parent`), for pages that embed
//! WordCraft in an `<iframe>` and keep its files (a Nextcloud app, a personal file server). The
//! same messages as GridCraft's and PdfCraft's bridges; each is a plain object with a `type`:
//!
//! - `wordcraft:ready` (to the parent): `{ version }`, sent once the app is running and
//!   listening; send a file now.
//! - `wordcraft:open` (from the parent): `{ name, bytes }`, bytes an `ArrayBuffer` or
//!   `Uint8Array`. It opens like a document picked with File › Open or dropped on the window.
//! - `wordcraft:save` (to the parent): `{ name, saveAs, bytes }` (a transferred `ArrayBuffer`) for
//!   every file WordCraft would otherwise download. The parent stores it and reports failures
//!   itself. `saveAs` says whether the file becomes the document's file:
//!   - Save: `false`. It writes the opened file under its own name and format, as the desktop
//!     app does. A name the parent never sent (an untitled document, an imported .doc saved as
//!     .docx) is a new file: `true`, as for Save As.
//!   - Save As, and File › Export to another document format (which runs Save As): `true` for a
//!     format that keeps everything (.docx and the other Word packages, .odt, .rtf). The document
//!     is now that file; the next Save sends its name with `false`.
//!   - Save As or Export to a format that doesn't (PDF, Markdown, plain text), Export as PDF and
//!     Export Page as PNG: `false`. They are copies; the document's file stays the same.
//! - `wordcraft:dirty` (to the parent): `{ dirty }` whenever unsaved changes appear or go away.
//! - `wordcraft:failed` (to the parent): `{ error }` when the app couldn't start.
//! - `wordcraft:control` (from the parent): `{ id, method, params }`, one request of the control
//!   protocol (docs/control-protocol.md): `engine.execute`, `engine.commands`, `document.inspect`
//!   and the rest. The app answers `wordcraft:control-reply` (to the parent): `{ id, reply }`, where
//!   `reply` is the protocol's own reply (`{ ok: true, result }` or `{ ok: false, error }`) and `id`
//!   is echoed as sent. These are the requests the desktop app's control port and `wordcraft-cli mcp
//!   --connect` carry, so a host page can drive the open document the way an agent drives the
//!   desktop app: an assistant that writes into it, a template filled from a form. No key is asked
//!   for: the port needs one because any local program can reach it, and here only the same-origin
//!   parent is heard. A request the app hasn't answered within `CONTROL_TIMEOUT_MS` is answered
//!   `{ ok: false, error }` instead, so the parent never waits on a reply that will not come.
//!
//! Only the parent window on the page's own origin is heard or answered. In host mode the parent
//! guards leaving the page, and `?author=` names the user for comments and tracked changes.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;
use wordcraft_ui_egui::{ControlRequest, Inbox, Services, WordApp};

/// The longest user name taken from `?author=`.
const MAX_AUTHOR_CHARS: usize = 200;
/// How long a `wordcraft:control` request may wait for its reply before it is answered with an error.
const CONTROL_TIMEOUT_MS: f64 = 60_000.0;
/// How often a pending control request looks for its reply (about once a frame).
const CONTROL_POLL_MS: i32 = 16;

#[derive(Clone)]
pub struct Bridge {
    window: web_sys::Window,
    parent: web_sys::Window,
    origin: String,
}

impl Bridge {
    /// `None` when the page isn't framed (there is no parent to talk to), or when the parent is
    /// on another origin: messages only go to this origin, so the browser would drop every save
    /// without an error while the document showed as saved.
    pub fn new() -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) {
            return None;
        }
        let origin = window.location().origin().ok()?;
        // An opaque origin (a `file://` page in some browsers) can't be a `postMessage` target.
        if origin == "null" {
            return None;
        }
        // Reading another origin's location throws, which is `None` too.
        if parent.location().origin().ok()? != origin {
            return None;
        }
        Some(Self { window, parent, origin })
    }

    /// Hand saved and exported files to the parent instead of downloading them, report unsaved
    /// changes to it, and hear the files and control requests it sends. The returned receiver
    /// carries those requests: give it to the app with [`WordApp::with_control`], which answers them
    /// every frame exactly as it answers the desktop app's control port.
    pub fn connect(&self, services: &mut Services, inbox: Inbox, ctx: egui::Context) -> Receiver<ControlRequest> {
        let (control_tx, control_rx) = channel();
        self.listen(inbox, control_tx, ctx);
        let bridge = self.clone();
        services.host_save = Some(Box::new(move |name: &str, bytes: &[u8], save_as: bool| bridge.post_save(name, bytes, save_as)));
        // Anything else that would download goes to the parent too.
        let bridge = self.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| bridge.post_save(name, bytes, false)));
        // The browser's save picker is never used, so no save finishes later.
        services.async_saves = None;
        let bridge = self.clone();
        let reported: Rc<Cell<Option<bool>>> = Rc::default();
        services.on_dirty = Some(Box::new(move |dirty: bool| {
            if reported.get() != Some(dirty) && bridge.send(&[("type", "wordcraft:dirty".into()), ("dirty", dirty.into())]) {
                reported.set(Some(dirty));
            }
        }));
        control_rx
    }

    /// `?author=`: the host's name for the user signs comments and tracked changes.
    pub fn set_author(app: &mut WordApp, author: &str) {
        let author: String = author.trim().chars().take(MAX_AUTHOR_CHARS).collect();
        if !author.is_empty() {
            app.session.author = author;
        }
    }

    pub fn post_ready(&self) {
        self.send(&[("type", "wordcraft:ready".into()), ("version", env!("CARGO_PKG_VERSION").into())]);
    }

    /// The app couldn't start (no WebGPU or WebGL2, for example): let the parent say so.
    pub fn post_failed(&self, error: &str) {
        self.send(&[("type", "wordcraft:failed".into()), ("error", error.into())]);
    }

    fn post_save(&self, path: &str, bytes: &[u8], save_as: bool) -> Result<(), String> {
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string());
        let buffer = js_sys::Uint8Array::from(bytes).buffer();
        let message = message(&[
            ("type", "wordcraft:save".into()),
            ("name", name.as_str().into()),
            ("saveAs", save_as.into()),
            ("bytes", buffer.clone().into()),
        ])
        .ok_or_else(|| "couldn't build the message".to_string())?;
        self.parent
            .post_message_with_transfer(&message, &self.origin, &js_sys::Array::of1(&buffer))
            .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))
    }

    /// Hear `wordcraft:open` from the parent and queue the file like a dropped one, and hear
    /// `wordcraft:control` and pass the request to the app.
    fn listen(&self, inbox: Inbox, control: Sender<ControlRequest>, ctx: egui::Context) {
        let parent = self.parent.clone();
        let origin = self.origin.clone();
        let bridge = self.clone();
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
                return;
            }
            let data = event.data();
            let kind = string(&data, "type");
            if kind.as_deref() == Some("wordcraft:control") {
                bridge.control(&data, &control, &ctx);
                return;
            }
            if kind.as_deref() != Some("wordcraft:open") {
                return;
            }
            let name = string(&data, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Document.docx".to_string());
            let Some(bytes) = bytes(&data) else { return };
            inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
            ctx.request_repaint();
        });
        if self.window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
            // The listener lives as long as the page.
            on_message.forget();
        }
    }

    /// One `wordcraft:control` request: queue it for the app, then wait (without blocking the page)
    /// for the app's reply and post it back with the request's `id`.
    fn control(&self, data: &JsValue, control: &Sender<ControlRequest>, ctx: &egui::Context) {
        let id = js_sys::Reflect::get(data, &JsValue::from_str("id")).unwrap_or(JsValue::NULL);
        let Some(method) = string(data, "method").filter(|m| !m.is_empty()) else {
            self.reply(&id, &serde_json::json!({"ok": false, "error": "method is required"}));
            return;
        };
        let (request, replies) = ControlRequest::new(method, params(data));
        if control.send(request).is_err() {
            self.reply(&id, &serde_json::json!({"ok": false, "error": "the app is not running"}));
            return;
        }
        ctx.request_repaint();
        let bridge = self.clone();
        let ctx = ctx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let started = js_sys::Date::now();
            let reply = loop {
                match replies.try_recv() {
                    Ok(reply) => break reply,
                    Err(TryRecvError::Disconnected) => break serde_json::json!({"ok": false, "error": "the request was dropped"}),
                    Err(TryRecvError::Empty) if js_sys::Date::now() - started > CONTROL_TIMEOUT_MS => {
                        break serde_json::json!({"ok": false, "error": "timed out waiting for the app"});
                    }
                    Err(TryRecvError::Empty) => {
                        // The app answers in its frame; make sure there is one.
                        ctx.request_repaint();
                        bridge.wait(CONTROL_POLL_MS).await;
                    }
                }
            };
            bridge.reply(&id, &reply);
        });
    }

    fn reply(&self, id: &JsValue, reply: &serde_json::Value) {
        let reply = js_sys::JSON::parse(&reply.to_string()).unwrap_or(JsValue::NULL);
        self.send(&[("type", "wordcraft:control-reply".into()), ("id", id.clone()), ("reply", reply)]);
    }

    async fn wait(&self, ms: i32) {
        let window = self.window.clone();
        let timer = js_sys::Promise::new(&mut |resolve, _| {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        });
        let _ = wasm_bindgen_futures::JsFuture::from(timer).await;
    }

    /// Post a message to the parent; false if it couldn't be sent.
    fn send(&self, fields: &[(&str, JsValue)]) -> bool {
        message(fields).is_some_and(|m| self.parent.post_message(&m, &self.origin).is_ok())
    }
}

fn message(fields: &[(&str, JsValue)]) -> Option<js_sys::Object> {
    let m = js_sys::Object::new();
    for (key, value) in fields {
        js_sys::Reflect::set(&m, &JsValue::from_str(key), value).ok()?;
    }
    Some(m)
}

/// A control request's `params`: an object (or a JSON string of one); anything else is `{}`.
fn params(data: &JsValue) -> serde_json::Value {
    let Ok(params) = js_sys::Reflect::get(data, &JsValue::from_str("params")) else {
        return serde_json::json!({});
    };
    let text = if let Some(text) = params.as_string() {
        Some(text)
    } else if params.is_object() {
        js_sys::JSON::stringify(&params).ok().and_then(|t| t.as_string())
    } else {
        None
    };
    text.and_then(|t| serde_json::from_str(&t).ok()).filter(serde_json::Value::is_object).unwrap_or_else(|| serde_json::json!({}))
}

fn string(data: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_string()
}

/// `bytes` as an `ArrayBuffer` or `Uint8Array`.
fn bytes(data: &JsValue) -> Option<Vec<u8>> {
    let bytes = js_sys::Reflect::get(data, &JsValue::from_str("bytes")).ok()?;
    if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
        Some(array.to_vec())
    } else if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
        Some(js_sys::Uint8Array::new(&bytes).to_vec())
    } else {
        None
    }
}
