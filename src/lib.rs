#[macro_use]
extern crate bitflags;
#[macro_use]
extern crate cstr_macro;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate janus_plugin as janus;
#[macro_use]
extern crate serde_json;
#[macro_use]
extern crate serde_derive;

mod userid;
mod messages;
mod sessions;
mod subscriptions;

use userid::{AtomicUserId, UserId};
use messages::{JsepKind, MessageKind, SessionRole, RawMessage};
use sessions::Session;
use subscriptions::{ContentKind, SubscriptionMap};

use std::collections::{HashSet, HashMap};
use std::error::Error;
use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::sync::atomic::Ordering;
use std::thread;
use serde_json::Value as JsonValue;
use serde_json::Result as JsonResult;
use janus::{JanssonValue, RawJanssonValue,
            LogLevel, Plugin, PluginCallbacks, PluginMetadata,
            PluginResultInfo, PluginResultType, PluginSession};

/// Inefficiently converts a serde JSON value to a Jansson JSON value.
fn from_serde_json(input: JsonValue) -> JanssonValue {
    JanssonValue::from_str(&input.to_string(), 0).unwrap()
}

type MessageProcessingResult = Result<JsonValue, Box<Error>>;

const METADATA: PluginMetadata = PluginMetadata {
    version: 1,
    version_str: cstr!("0.0.1"),
    description: cstr!("Janus SFU for game networking."),
    name: cstr!("Janus SFU plugin"),
    author: cstr!("Marshall Quander"),
    package: cstr!("janus.plugin.sfu"),
};

static mut CALLBACKS: Option<&PluginCallbacks> = None;

/// Returns a ref to the callback struct provided by Janus containing function pointers to pass data back to the gateway.
fn gateway_callbacks() -> &'static PluginCallbacks {
    unsafe { CALLBACKS.expect("Callbacks not initialized -- did plugin init() succeed?") }
}

#[derive(Debug)]
struct State {
    pub sessions: RwLock<Vec<Box<Arc<Session>>>>,
    pub subscriptions: RwLock<SubscriptionMap>,
    pub message_channel: Mutex<Option<mpsc::SyncSender<RawMessage>>>,
    pub next_user_id: AtomicUserId,
}

lazy_static! {
    static ref STATE: State = State {
        sessions: RwLock::new(Vec::new()),
        subscriptions: RwLock::new(HashMap::new()),
        message_channel: Mutex::new(None),
        next_user_id: AtomicUserId::first()
    };
}

fn get_user_list() -> HashSet<UserId> {
    let sessions = STATE.sessions.read().unwrap();
    sessions.iter().filter_map(|c| c.user_id.load(Ordering::Relaxed)).collect()
}

fn notify(myself: UserId, msg: JanssonValue) -> Result<(), Box<Error>> {
    let push_event = gateway_callbacks().push_event;
    for other in STATE.sessions.read()?.iter() {
        if other.user_id.load(Ordering::Relaxed) != Some(myself) {
            janus::get_result(push_event(other.handle, &mut PLUGIN, ptr::null(), msg.ptr, ptr::null_mut()))?
        }
    }
    Ok(())
}

fn push_response(sess: &Session, txn: *mut c_char, result: MessageProcessingResult) -> Result<(), Box<Error>> {
    let push_event = gateway_callbacks().push_event;
    let response = match result {
        Ok(resp) => json!({ "success": true, "data": resp }),
        Err(err) => json!({ "success": false, "error": format!("{}", err) })
    };
    janus::get_result(push_event(sess.handle, &mut PLUGIN, txn, from_serde_json(response).ptr, ptr::null_mut()))
}

fn relay<T>(from: *mut PluginSession, kind: ContentKind, send: T) -> Result<(), Box<Error>> where T: Fn(&Session) {
    let sess = Session::from_ptr(from)?;
    if let Some(user_id) = sess.user_id.load(Ordering::Relaxed) {
        janus::log(LogLevel::Dbg, &format!("Packet of kind {:?} received from {:?}.", kind, user_id));
        let subscriptions = STATE.subscriptions.read()?;
        subscriptions::for_each_subscriber(&subscriptions, user_id, kind, |s| {
            if Some(user_id) != s.user_id.load(Ordering::Relaxed) {
                send(s)
            }
        });
        Ok(())
    } else {
        Err(From::from("No user ID associated with connection; can't relay."))
    }
}

extern "C" fn init(callbacks: *mut PluginCallbacks, _config_path: *const c_char) -> c_int {
    match unsafe { callbacks.as_ref() } {
        Some(c) => {
            unsafe { CALLBACKS = Some(c) };
            let (messages_tx, messages_rx) = mpsc::sync_channel(0);
            *(STATE.message_channel.lock().unwrap()) = Some(messages_tx);

            thread::spawn(move || {
                janus::log(LogLevel::Verb, "Message processing thread is alive.");
                for msg in messages_rx.iter() {
                    janus::log(LogLevel::Verb, &format!("Processing message: {:?}", msg));
                    handle_message_async(msg).err().map(|e| {
                        janus::log(LogLevel::Err, &format!("Error processing message: {}", e));
                    });
                }
            });

            janus::log(LogLevel::Info, "Janus SFU plugin initialized!");
            0
        },
        None => {
            janus::log(LogLevel::Err, "Invalid parameters for SFU plugin initialization!");
            -1
        }
    }
}

extern "C" fn destroy() {
    janus::log(LogLevel::Info, "Janus SFU plugin destroyed!");
}

extern "C" fn create_session(handle: *mut PluginSession, error: *mut c_int) {
    match Session::associate(handle, Default::default()) {
        Ok(sess) => {
            janus::log(LogLevel::Info, &format!("Initializing SFU session {:?}...", sess));
            STATE.sessions.write().unwrap().push(sess);
        },
        Err(e) => {
            janus::log(LogLevel::Err, &format!("{}", e));
            unsafe { *error = -1 };
        }
    }
}

extern "C" fn destroy_session(handle: *mut PluginSession, error: *mut c_int) {
    match Session::from_ptr(handle) {
        Ok(sess) => {
            janus::log(LogLevel::Info, &format!("Destroying SFU session {:?}...", sess));
            let user_id = sess.user_id.load(Ordering::Relaxed);
            STATE.sessions.write().unwrap().retain(|ref s| s.handle != handle);

            if let Some(user_id) = user_id {
                let user_exists = STATE.sessions.read().unwrap().iter().any(|ref s| Some(user_id) == s.user_id.load(Ordering::Relaxed));
                if !user_exists {
                    let mut subscriptions = STATE.subscriptions.write().unwrap();
                    subscriptions::unpublish(&mut subscriptions, user_id);
                    let response = from_serde_json(json!({ "event": "leave", "user_id": user_id }));
                    notify(user_id, response).unwrap_or_else(|e| {
                        janus::log(LogLevel::Err, &format!("Error notifying publishers on leave: {}", e));
                    });
                }
            }
        },
        Err(e) => {
            janus::log(LogLevel::Err, &format!("{}", e));
            unsafe { *error = -1 };
        }
    }
}

extern "C" fn query_session(_handle: *mut PluginSession) -> *mut RawJanssonValue {
    ptr::null_mut()
}

extern "C" fn setup_media(_handle: *mut PluginSession) {
    janus::log(LogLevel::Verb, "WebRTC media is now available.");
}

extern "C" fn incoming_rtp(handle: *mut PluginSession, video: c_int, buf: *mut c_char, len: c_int) {
    let content_kind = if video == 1 { ContentKind::VIDEO } else { ContentKind::AUDIO };
    let relay_rtp = gateway_callbacks().relay_rtp;
    if let Err(e) = relay(handle, content_kind, |other| { relay_rtp(other.handle, video, buf, len); }) {
        janus::log(LogLevel::Err, &format!("Discarding RTP packet: {}", e))
    }
}

extern "C" fn incoming_rtcp(handle: *mut PluginSession, video: c_int, buf: *mut c_char, len: c_int) {
    let content_kind = if video == 1 { ContentKind::VIDEO } else { ContentKind::AUDIO };
    let relay_rtcp = gateway_callbacks().relay_rtcp;
    if let Err(e) = relay(handle, content_kind, |other| { relay_rtcp(other.handle, video, buf, len); }) {
        janus::log(LogLevel::Err, &format!("Discarding RTCP packet: {}", e))
    }
}

extern "C" fn incoming_data(handle: *mut PluginSession, buf: *mut c_char, len: c_int) {
    let relay_data = gateway_callbacks().relay_data;
    if let Err(e) = relay(handle, ContentKind::DATA, |other| { relay_data(other.handle, buf, len); }) {
        janus::log(LogLevel::Err, &format!("Discarding data packet: {}", e))
    }
}

extern "C" fn slow_link(_handle: *mut PluginSession, _uplink: c_int, _video: c_int) {
    janus::log(LogLevel::Verb, "Slow link message received!");
}

extern "C" fn hangup_media(_handle: *mut PluginSession) {
    janus::log(LogLevel::Verb, "Hanging up WebRTC media.");
}

fn handle_join(sess: &Arc<Session>, user_id: Option<UserId>, role: SessionRole) -> MessageProcessingResult {
    match role {
        SessionRole::Subscriber { publisher_id } => {
            let mut subscriptions = STATE.subscriptions.write()?;
            let kind = ContentKind::AUDIO | ContentKind::VIDEO;
            subscriptions::subscribe(&mut subscriptions, &sess, kind, Some(publisher_id));
        }
        SessionRole::Publisher => {
            let mut subscriptions = STATE.subscriptions.write()?;
            let kind = ContentKind::DATA;
            subscriptions::subscribe(&mut subscriptions, &sess, kind, None);
        },
    };

    let new_user_id = match user_id {
        Some(n) => { sess.user_id.store(n, Ordering::Relaxed); n },
        None => {
            let new_user_id = STATE.next_user_id.next(Ordering::Relaxed)
                .expect("next_user_id is always a non-empty user ID.");
            sess.user_id.store(new_user_id, Ordering::Relaxed);
            let notification = from_serde_json(json!({ "event": "join", "user_id": new_user_id }));
            if let Err(e) = notify(new_user_id, notification) {
                janus::log(LogLevel::Err, &format!("Error sending notification for user join: {:?}", e))
            }
            new_user_id
        }
    };

    Ok(json!({
        "user_id": new_user_id,
        "user_ids": get_user_list()
    }))
}

fn handle_list() -> MessageProcessingResult {
    Ok(json!({ "user_ids": get_user_list() }))
}

fn handle_subscribe(sess: &Arc<Session>, publisher: Option<UserId>, kind: u8) -> MessageProcessingResult {
    match ContentKind::from_bits(kind) {
        Some(k) => {
            let mut subscriptions = STATE.subscriptions.write()?;
            subscriptions::subscribe(&mut subscriptions, &sess, k, publisher);
            Ok(json!({}))
        },
        None => Err(From::from("Invalid content kind."))
    }
}

fn handle_unsubscribe(sess: &Arc<Session>, publisher: Option<UserId>, kind: u8) -> MessageProcessingResult {
    match ContentKind::from_bits(kind) {
        Some(k) => {
            let mut subscriptions = STATE.subscriptions.write()?;
            subscriptions::unsubscribe(&mut subscriptions, &sess, k, publisher);
            Ok(json!({}))
        },
        None => Err(From::from("Invalid content kind."))
    }
}

fn handle_offer(sess: &Arc<Session>, txn: *mut c_char, sdp: String) -> Result<(), Box<Error>> {
    let offer = janus::sdp::parse_sdp(CString::new(sdp)?)?;
    let answer = answer_sdp!(offer, janus::sdp::OfferAnswerParameters::Video, 0);
    let answer_str = janus::sdp::write_sdp(&answer);
    let answer_msg = from_serde_json(json!({}));
    let answer_jsep = from_serde_json(json!({
        "type": "answer",
        "sdp": answer_str.to_str()?
    }));
    let push_event = gateway_callbacks().push_event;
    janus::get_result(push_event(sess.handle, &mut PLUGIN, txn, answer_msg.ptr, answer_jsep.ptr))
}

fn handle_message_async(RawMessage { jsep, msg, txn, sess }: RawMessage) -> Result<(), Box<Error>> {
    if let Some(ref sess) = sess.upgrade() {
        // if we have a JSEP, handle it independently of whether or not we have a message
        jsep.map_or(Ok(()), |x| {
            let result: JsonResult<JsepKind> = serde_json::from_str(&x.to_string(0));
            match result {
                Ok(kind) => {
                    janus::log(LogLevel::Info, &format!("Processing {:?} on connection {:?}.", kind, sess));
                    match kind {
                        JsepKind::Offer { sdp } => handle_offer(&sess, txn, sdp),
                        JsepKind::Answer { .. } => {
                            push_response(sess, txn, Err(From::from("JSEP answers not yet supported.")))
                        }
                    }
                },
                Err(e) => push_response(sess, txn, Err(Box::new(e)))
            }
        })?;
        // if we have a message, handle that
        msg.map_or(Ok(()), |x| {
            let result: JsonResult<MessageKind> = serde_json::from_str(&x.to_string(0));
            let response: MessageProcessingResult = match result {
                Ok(kind) => {
                    janus::log(LogLevel::Info, &format!("Processing {:?} on connection {:?}.", kind, sess));
                    match kind {
                        MessageKind::List => handle_list(),
                        MessageKind::Join { user_id, role } => handle_join(&sess, user_id, role),
                        MessageKind::Subscribe { publisher_id, content_kind } => handle_subscribe(&sess, publisher_id, content_kind),
                        MessageKind::Unsubscribe { publisher_id, content_kind } => handle_unsubscribe(&sess, publisher_id, content_kind)
                    }
                },
                Err(e) => Err(Box::new(e))
            };
            push_response(sess, txn, response)
        })
    } else {
        // getting messages for destroyed connections is slightly concerning,
        // because messages shouldn't be backed up for that long, so warn if it happens
        Ok(janus::log(LogLevel::Warn, "Message received for destroyed session; discarding."))
    }
}

extern "C" fn handle_message(handle: *mut PluginSession, transaction: *mut c_char,
                             message: *mut RawJanssonValue, jsep: *mut RawJanssonValue) -> *mut PluginResultInfo {
    janus::log(LogLevel::Verb, "Queueing signalling message.");
    Box::into_raw(
        match Session::from_ptr(handle) {
            Ok(sess) => {
                let msg = RawMessage {
                    sess: Arc::downgrade(&sess),
                    txn: transaction,
                    msg: JanssonValue::new(message),
                    jsep: JanssonValue::new(jsep)
                };
                STATE.message_channel.lock().unwrap().as_ref().unwrap().send(msg).ok();
                janus::create_result(PluginResultType::JANUS_PLUGIN_OK_WAIT, cstr!("Processing."), None)
            },
            Err(_) => janus::create_result(PluginResultType::JANUS_PLUGIN_ERROR, cstr!("No handle associated with message!"), None)
        }
    )
}

const PLUGIN: Plugin = build_plugin!(
    METADATA,
    init,
    destroy,
    create_session,
    handle_message,
    setup_media,
    incoming_rtp,
    incoming_rtcp,
    incoming_data,
    slow_link,
    hangup_media,
    destroy_session,
    query_session
);

export_plugin!(&PLUGIN);

#[cfg(test)]
mod tests {

    use super::*;

    mod jsep_parsing {

        use super::*;

        #[test]
        fn parse_offer() {
            let jsep = r#"{"type": "offer", "sdp": "..."}"#;
            let result: JsepKind = serde_json::from_str(jsep).unwrap();
            assert_eq!(result, JsepKind::Offer { sdp: "...".to_owned() });
        }

        #[test]
        fn parse_answer() {
            let jsep = r#"{"type": "answer", "sdp": "..."}"#;
            let result: JsepKind = serde_json::from_str(jsep).unwrap();
            assert_eq!(result, JsepKind::Answer { sdp: "...".to_owned() });
        }
    }

    mod message_parsing {

        use super::*;

        #[test]
        fn parse_list() {
            let json = r#"{"kind": "list"}"#;
            let result: MessageKind = serde_json::from_str(json).unwrap();
            assert_eq!(result, MessageKind::List);
        }

        #[test]
        fn parse_publisher() {
            let json = r#"{"kind": "join", "role": {"kind": "publisher"}}"#;
            let result: MessageKind = serde_json::from_str(json).unwrap();
            assert_eq!(result, MessageKind::Join { user_id: None, role: SessionRole::Publisher });
        }

        #[test]
        fn parse_subscriber() {
            let json = r#"{"kind": "join", "user_id": 1, "role": {"kind": "subscriber", "publisher_id": 2}}"#;
            let result: MessageKind = serde_json::from_str(json).unwrap();
            assert_eq!(result, MessageKind::Join {
                user_id: Some(UserId::try_from(1).unwrap()),
                role: SessionRole::Subscriber { publisher_id: UserId::try_from(2).unwrap() }
            });
        }

        #[test]
        fn parse_subscribe() {
            let json = r#"{"kind": "subscribe", "publisher_id": 2, "content_kind": 1}"#;
            let result: MessageKind = serde_json::from_str(json).unwrap();
            assert_eq!(result, MessageKind::Subscribe {
                publisher_id: Some(UserId::try_from(2).unwrap()),
                content_kind: 1
            });
        }

        #[test]
        fn parse_unsubscribe() {
            let json = r#"{"kind": "unsubscribe", "content_kind": 2}"#;
            let result: MessageKind = serde_json::from_str(json).unwrap();
            assert_eq!(result, MessageKind::Unsubscribe {
                publisher_id: None,
                content_kind: 2
            });
        }
    }
}
