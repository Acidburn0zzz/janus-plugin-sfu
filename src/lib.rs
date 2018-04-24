extern crate atom;
extern crate ini;
extern crate multimap;
#[macro_use]
extern crate janus_plugin as janus;
#[macro_use]
extern crate lazy_static;
extern crate serde;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate serde_json;

mod messages;
mod sessions;
mod switchboard;
mod config;

use atom::AtomSetOnce;
use messages::{RoomId, UserId};
use config::Config;
use janus::{JanusError, JanusResult, JanssonDecodingFlags, JanssonEncodingFlags, JanssonValue, Plugin, PluginCallbacks,
            LibraryMetadata, PluginResult, PluginSession, RawPluginResult, RawJanssonValue};
use janus::sdp::{AudioCodec, MediaDirection, OfferAnswerParameters, Sdp, VideoCodec};
use messages::{JsepKind, MessageKind, OptionalField, Subscription};
use serde_json::Value as JsonValue;
use sessions::{JoinState, Session, SessionState};
use std::error::Error;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::path::Path;
use std::ptr;
use std::slice;
use std::sync::{mpsc, Arc, Mutex, RwLock, Weak};
use std::sync::atomic::{Ordering, AtomicIsize};
use std::thread;
use switchboard::Switchboard;

// courtesy of c_string crate, which also has some other stuff we aren't interested in
// taking in as a dependency here.
macro_rules! c_str {
    ($lit:expr) => {
        unsafe {
            CStr::from_ptr(concat!($lit, "\0").as_ptr() as *const $crate::c_char)
        }
    }
}

/// A Janus transaction ID. Used to correlate signalling requests and responses.
#[derive(Debug)]
struct TransactionId(pub *mut c_char);

unsafe impl Send for TransactionId {}

/// A single signalling message that came in off the wire, associated with one session.
///
/// These will be queued up asynchronously and processed in order later.
#[derive(Debug)]
struct RawMessage {
    /// A reference to the sender's session. Possibly null if the session has been destroyed
    /// in between receiving and processing this message.
    pub from: Weak<Session>,

    /// The transaction ID used to mark any responses to this message.
    pub txn: TransactionId,

    /// An arbitrary message from the client. Will be deserialized as a MessageKind.
    pub msg: Option<JanssonValue>,

    /// A JSEP message (SDP offer or answer) from the client. Will be deserialized as a JsepKind.
    pub jsep: Option<JanssonValue>,
}

/// Inefficiently converts a serde JSON value to a Jansson JSON value.
fn from_serde_json(input: &JsonValue) -> JanssonValue {
    JanssonValue::from_str(&input.to_string(), JanssonDecodingFlags::empty()).unwrap()
}

/// A response to a signalling message. May carry either a response body, a JSEP, or both.
struct MessageResponse {
    pub body: Option<JsonValue>,
    pub jsep: Option<JsonValue>, // todo: make this an Option<JsepKind>?
}

impl MessageResponse {
    fn new(body: JsonValue, jsep: JsonValue) -> Self {
        Self { body: Some(body), jsep: Some(jsep) }
    }
    fn msg(body: JsonValue) -> Self {
        Self { body: Some(body), jsep: None }
    }
}

/// A result which carries a signalling message response to send to a client.
type MessageResult = Result<MessageResponse, Box<Error>>;

/// A result which carries a JSEP to send to a client.
type JsepResult = Result<JsonValue, Box<Error>>;

/// The audio codec Janus will negotiate with all participants. Opus is cross-compatible with everything we care about.
static AUDIO_CODEC: AudioCodec = AudioCodec::Opus;

/// The video codec Janus will negotiate with all participants. H.264 is cross-compatible with modern Firefox, Chrome,
/// Safari, and Edge; VP8/9 unfortunately isn't compatible with Safari.
static VIDEO_CODEC: VideoCodec = VideoCodec::H264;

static mut CALLBACKS: Option<&PluginCallbacks> = None;

/// Returns a ref to the callback struct provided by Janus containing function pointers to pass data back to the gateway.
fn gateway_callbacks() -> &'static PluginCallbacks {
    unsafe { CALLBACKS.expect("Callbacks not initialized -- did plugin init() succeed?") }
}

#[derive(Debug)]
struct State {
    pub switchboard: RwLock<Switchboard>,
    pub message_channel: AtomSetOnce<Box<mpsc::SyncSender<RawMessage>>>,
    pub config: AtomSetOnce<Box<Config>>,
}

lazy_static! {
    static ref STATE: State = State {
        switchboard: RwLock::new(Switchboard::new()),
        message_channel: AtomSetOnce::empty(),
        config: AtomSetOnce::empty(),
    };
}

fn notify_user<T: IntoIterator<Item=U>, U: AsRef<Session>>(json: &JsonValue, target: &UserId, everyone: T) -> JanusResult {
    let notifiees = everyone.into_iter().filter(|s| {
        let subscription_state = s.as_ref().subscription.get();
        let join_state = s.as_ref().join_state.get();
        match (subscription_state, join_state) {
            (Some(subscription), Some(joined)) => {
                subscription.notifications && &joined.user_id == target
            }
            _ => false
        }
    });
    send_notification(json, notifiees)
}

fn notify_except<T: IntoIterator<Item=U>, U: AsRef<Session>>(json: &JsonValue, myself: &UserId, everyone: T) -> JanusResult {
    let notifiees = everyone.into_iter().filter(|s| {
        let subscription_state = s.as_ref().subscription.get();
        let join_state = s.as_ref().join_state.get();
        match (subscription_state, join_state) {
            (Some(subscription), Some(joined)) => {
                subscription.notifications && &joined.user_id != myself
            }
            _ => false
        }
    });
    send_notification(json, notifiees)
}

fn send_notification<T: IntoIterator<Item=U>, U: AsRef<Session>>(body: &JsonValue, sessions: T) -> JanusResult {
    let mut msg = from_serde_json(body);
    let push_event = gateway_callbacks().push_event;
    for session in sessions {
        janus_info!("Notification going to {:?}: {:?}.", session.as_ref(), msg);
        // probably a hack -- we shouldn't stop notifying if we fail one
        JanusError::from(push_event(session.as_ref().as_ptr(), &mut PLUGIN, ptr::null(), msg.as_mut_ref(), ptr::null_mut()))?
    }
    Ok(())
}

fn send_offer<T: IntoIterator<Item=U>, U: AsRef<Session>>(offer: &JsonValue, sessions: T) -> JanusResult {
    let mut msg = from_serde_json(&json!({}));
    let mut jsep = from_serde_json(offer);
    let push_event = gateway_callbacks().push_event;
    for session in sessions {
        janus_info!("Offer going to {:?}: {:?}.", session.as_ref(), jsep);
        // probably a hack -- we shouldn't stop notifying if we fail one
        JanusError::from(push_event(session.as_ref().as_ptr(), &mut PLUGIN, ptr::null(), msg.as_mut_ref(), jsep.as_mut_ref()))?
    }
    Ok(())
}

fn send_pli<T: IntoIterator<Item=U>, U: AsRef<Session>>(publishers: T) {
    let relay_rtcp = gateway_callbacks().relay_rtcp;
    for publisher in publishers {
        let mut pli = janus::rtcp::gen_pli();
        relay_rtcp(publisher.as_ref().as_ptr(), 1, pli.as_mut_ptr(), pli.len() as i32);
    }
}

fn send_fir<T: IntoIterator<Item=U>, U: AsRef<Session>>(publishers: T) {
    let relay_rtcp = gateway_callbacks().relay_rtcp;
    for publisher in publishers {
        let mut seq = publisher.as_ref().fir_seq.fetch_add(1, Ordering::Relaxed) as i32;
        let mut fir = janus::rtcp::gen_fir(&mut seq);
        relay_rtcp(publisher.as_ref().as_ptr(), 1, fir.as_mut_ptr(), fir.len() as i32);
    }
}

fn get_config(config_root: *const c_char) -> Result<Config, Box<Error>> {
    let config_path = unsafe { Path::new(CStr::from_ptr(config_root).to_str()?) };
    let config_file = config_path.join("janus.plugin.sfu.cfg");
    Config::from_path(config_file)
}

extern "C" fn init(callbacks: *mut PluginCallbacks, config_path: *const c_char) -> c_int {
    let config = match get_config(config_path) {
        Ok(c) => {
            janus_info!("Loaded SFU plugin configuration: {:?}", c);
            c
        }
        Err(e) => {
            janus_warn!("Error loading configuration for SFU plugin: {}", e);
            Config::default()
        }
    };
    STATE.config.set_if_none(Box::new(config));
    match unsafe { callbacks.as_ref() } {
        Some(c) => {
            unsafe { CALLBACKS = Some(c) };
            let (messages_tx, messages_rx) = mpsc::sync_channel(0);
            STATE.message_channel.set_if_none(Box::new(messages_tx));

            thread::spawn(move || {
                janus_verb!("Message processing thread is alive.");
                for msg in messages_rx.iter() {
                    janus_verb!("Processing message: {:?}", msg);
                    handle_message_async(msg).err().map(|e| {
                        janus_err!("Error processing message: {}", e);
                    });
                }
            });

            janus_info!("Janus SFU plugin initialized!");
            0
        }
        None => {
            janus_err!("Invalid parameters for SFU plugin initialization!");
            -1
        }
    }
}

extern "C" fn destroy() {
    janus_info!("Janus SFU plugin destroyed!");
}

extern "C" fn create_session(handle: *mut PluginSession, error: *mut c_int) {
    let initial_state = SessionState {
        destroyed: Mutex::new(false),
        join_state: AtomSetOnce::empty(),
        subscriber_offer: Arc::new(Mutex::new(None)),
        subscription: AtomSetOnce::empty(),
        fir_seq: AtomicIsize::new(0),
    };

    match unsafe { Session::associate(handle, initial_state) } {
        Ok(sess) => {
            janus_info!("Initializing SFU session {:?}...", sess);
            STATE.switchboard.write().expect("Switchboard is poisoned :(").connect(sess);
        }
        Err(e) => {
            janus_err!("{}", e);
            unsafe { *error = -1 };
        }
    }
}

extern "C" fn destroy_session(handle: *mut PluginSession, error: *mut c_int) {
    match unsafe { Session::from_ptr(handle) } {
        Ok(sess) => {
            janus_info!("Destroying SFU session {:?}...", sess);
            let mut destroyed = sess.destroyed.lock().expect("Session destruction mutex is poisoned :(");
            let mut switchboard = STATE.switchboard.write().expect("Switchboard is poisoned :(");
            switchboard.remove_session(&sess);
            if let Some(joined) = sess.join_state.get() {
                // if they are entirely disconnected, notify their roommates
                if !switchboard.is_connected(&joined.user_id) {
                    let response = json!({ "event": "leave", "user_id": &joined.user_id, "room_id": &joined.room_id });
                    let occupants = switchboard.occupants_of(&joined.room_id);
                    match notify_except(&response, &joined.user_id, occupants) {
                        Ok(_) => (),
                        Err(JanusError { code: 458 }) /* session not found */ => (),
                        Err(e) => janus_err!("Error notifying publishers on leave: {}", e)
                    };
                }
            }
            *destroyed = true;
        }
        Err(e) => {
            janus_err!("{}", e);
            unsafe { *error = -1 };
        }
    }
}

extern "C" fn query_session(_handle: *mut PluginSession) -> *mut RawJanssonValue {
    let output = json!({});
    from_serde_json(&output).into_raw()
}

extern "C" fn setup_media(handle: *mut PluginSession) {
    let sess = unsafe { Session::from_ptr(handle).expect("Session can't be null!") };
    let switchboard = STATE.switchboard.read().expect("Switchboard is poisoned :(");
    send_fir(switchboard.media_senders_to(&sess));
    janus_verb!("WebRTC media is now available on {:?}.", sess);
}

extern "C" fn incoming_rtp(handle: *mut PluginSession, video: c_int, buf: *mut c_char, len: c_int) {
    let sess = unsafe { Session::from_ptr(handle).expect("Session can't be null!") };
    let switchboard = STATE.switchboard.read().expect("Switchboard lock poisoned; can't continue.");
    let relay_rtp = gateway_callbacks().relay_rtp;
    for other in switchboard.media_recipients_for(&sess) {
        relay_rtp(other.as_ptr(), video, buf, len);
    }
}

extern "C" fn incoming_rtcp(handle: *mut PluginSession, video: c_int, buf: *mut c_char, len: c_int) {
    let sess = unsafe { Session::from_ptr(handle).expect("Session can't be null!") };
    let switchboard = STATE.switchboard.read().expect("Switchboard lock poisoned; can't continue.");
    let packet = unsafe { slice::from_raw_parts(buf, len as usize) };
    match video {
        1 if janus::rtcp::has_pli(packet) => {
            send_pli(switchboard.media_senders_to(&sess));
        }
        1 if janus::rtcp::has_fir(packet) => {
            send_fir(switchboard.media_senders_to(&sess));
        }
        _ => {
            let relay_rtcp = gateway_callbacks().relay_rtcp;
            for subscriber in switchboard.media_recipients_for(&sess) {
                relay_rtcp(subscriber.as_ptr(), video, buf, len);
            }
        }
    }
}

extern "C" fn incoming_data(handle: *mut PluginSession, buf: *mut c_char, len: c_int) {
    let sess = unsafe { Session::from_ptr(handle).expect("Session can't be null!") };
    let switchboard = STATE.switchboard.read().expect("Switchboard lock poisoned; can't continue.");
    let relay_data = gateway_callbacks().relay_data;
    for other in switchboard.data_recipients_for(&sess) {
        relay_data(other.as_ptr(), buf, len);
    }
}

extern "C" fn slow_link(_handle: *mut PluginSession, _uplink: c_int, _video: c_int) {
    janus_verb!("Slow link message received!");
}

extern "C" fn hangup_media(_handle: *mut PluginSession) {
    janus_verb!("Hanging up WebRTC media.");
}

fn process_join(from: &Arc<Session>, room_id: RoomId, user_id: UserId, subscribe: Option<Subscription>) -> MessageResult {
    // todo: holy shit clean this function up somehow
    let mut switchboard = STATE.switchboard.write()?;
    let body = json!({ "users": { room_id.as_str(): switchboard.get_users(&room_id) }});

    let already_joined = !from.join_state.is_none();
    let already_subscribed = !from.subscription.is_none();
    if already_joined {
        return Err(From::from("Handles may only join once!"))
    }
    if already_subscribed && subscribe.is_some() {
        return Err(From::from("Handles may only subscribe once!"))
    }

    let mut is_master_handle = false;
    if let Some(subscription) = subscribe.as_ref() {
        let max_room_size = STATE.config.get().unwrap().max_room_size;
        let room_is_full = switchboard.occupants_of(&room_id).len() >= max_room_size;
        is_master_handle = subscription.data; // hack -- assume there is only one "master" data connection per user
        if is_master_handle && room_is_full {
            return Err(From::from("Room is full."))
        }
    }

    from.join_state.set_if_none(Box::new(JoinState::new(room_id.clone(), user_id.clone())));
    if let Some(subscription) = subscribe {
        from.subscription.set_if_none(Box::new(subscription.clone()));
        if is_master_handle {
            let notification = json!({ "event": "join", "user_id": user_id, "room_id": room_id });
            switchboard.join_room(Arc::clone(from), room_id.clone());
            if let Err(e) = notify_except(&notification, &user_id, switchboard.occupants_of(&room_id)) {
                janus_err!("Error sending notification for user join: {:?}", e)
            }
        }
        if let Some(ref publisher_id) = subscription.media {
            let publisher = switchboard.get_publisher(publisher_id).ok_or("Can't subscribe to a nonexistent publisher.")?.clone();
            let jsep = json!({
                "type": "offer",
                "sdp": publisher.subscriber_offer.lock().unwrap().as_ref().unwrap()
            });
            switchboard.subscribe_to_user(Arc::clone(from), publisher);
            return Ok(MessageResponse::new(body, jsep));
        }
    }
    Ok(MessageResponse::msg(body))
}

fn process_block(from: &Arc<Session>, whom: UserId) -> MessageResult {
    if let Some(joined) = from.join_state.get() {
        let mut switchboard = STATE.switchboard.write()?;
        let event = json!({ "event": "blocked", "by": &joined.user_id });
        match notify_user(&event, &whom, switchboard.occupants_of(&joined.room_id)) {
            Ok(_) => (),
            Err(JanusError { code: 458 }) /* session not found */ => (),
            Err(e) => janus_err!("Error notifying user about block: {}", e)
        };
        switchboard.establish_block(joined.user_id.clone(), whom);
        Ok(MessageResponse::msg(json!({})))
    } else {
        Err(From::from("Cannot block when not in a room."))
    }
}

fn process_unblock(from: &Arc<Session>, whom: UserId) -> MessageResult {
    if let Some(joined) = from.join_state.get() {
        let mut switchboard = STATE.switchboard.write()?;
        switchboard.lift_block(&joined.user_id, &whom);
        if let Some(publisher) = switchboard.get_publisher(&whom) {
            send_fir(&[publisher]);
        }
        let event = json!({ "event": "unblocked", "by": &joined.user_id });
        match notify_user(&event, &whom, switchboard.occupants_of(&joined.room_id)) {
            Ok(_) => (),
            Err(JanusError { code: 458 }) /* session not found */ => (),
            Err(e) => janus_err!("Error notifying user about unblock: {}", e)
        };
        Ok(MessageResponse::msg(json!({})))
    } else {
        Err(From::from("Cannot unblock when not in a room."))
    }
}

fn process_subscribe(from: &Arc<Session>, what: Subscription) -> MessageResult {
    let subscription_state = Box::new(what.clone());
    if from.subscription.set_if_none(subscription_state).is_some() {
        return Err(From::from("Users may only subscribe once!"))
    }

    let mut switchboard = STATE.switchboard.write()?;
    if let Some(ref publisher_id) = what.media {
        let publisher = switchboard.get_publisher(publisher_id).ok_or("Can't subscribe to a nonexistent publisher.")?.clone();
        let jsep = json!({
            "type": "offer",
            "sdp": publisher.subscriber_offer.lock().unwrap().as_ref().unwrap()
        });
        switchboard.subscribe_to_user(from.clone(), publisher);
        return Ok(MessageResponse::new(json!({}), jsep));
    }
    Ok(MessageResponse::msg(json!({})))
}

fn process_message(from: &Arc<Session>, msg: &JanssonValue) -> MessageResult {
    let msg_str = msg.to_libcstring(JanssonEncodingFlags::empty());
    let msg_contents: OptionalField<MessageKind> = serde_json::from_str(msg_str.to_str()?)?;
    match msg_contents {
        OptionalField::None {} => Ok(MessageResponse::msg(json!({}))),
        OptionalField::Some(kind) => {
            janus_info!("Processing {:?} on connection {:?}.", kind, from);
            match kind {
                MessageKind::Subscribe { what } => process_subscribe(from, what),
                MessageKind::Block { whom } => process_block(from, whom),
                MessageKind::Unblock { whom } => process_unblock(from, whom),
                MessageKind::Join { room_id, user_id, subscribe } => process_join(from, room_id, user_id, subscribe),
            }
        }
    }
}

fn process_offer(from: &Session, offer: &Sdp) -> JsepResult {
    // enforce publication of the codecs that we know our client base will be compatible with
    let answer = answer_sdp!(
        offer,
        OfferAnswerParameters::AudioCodec, AUDIO_CODEC.to_cstr().as_ptr(),
        OfferAnswerParameters::AudioDirection, MediaDirection::JANUS_SDP_RECVONLY,
        OfferAnswerParameters::VideoCodec, VIDEO_CODEC.to_cstr().as_ptr(),
        OfferAnswerParameters::VideoDirection, MediaDirection::JANUS_SDP_RECVONLY,
    );
    janus_huge!("Providing answer to {:?}: {}", from, answer.to_string().to_str().unwrap());

    // it's fishy, but we provide audio and video streams to subscribers regardless of whether the client is sending
    // audio and video right now or not -- this is basically working around pains in renegotiation to do with
    // reordering/removing media streams on an existing connection. to improve this, we'll want to keep the same offer
    // around and mutate it, instead of generating a new one every time the publisher changes something.

    let audio_payload_type = answer.get_payload_type(AUDIO_CODEC.to_cstr());
    let video_payload_type = answer.get_payload_type(VIDEO_CODEC.to_cstr());
    let subscriber_offer = offer_sdp!(
        ptr::null(),
        answer.c_addr as *const _,
        OfferAnswerParameters::Data, 1,
        OfferAnswerParameters::Audio, 1,
        OfferAnswerParameters::AudioCodec, AUDIO_CODEC.to_cstr().as_ptr(),
        OfferAnswerParameters::AudioPayloadType, audio_payload_type.unwrap_or(100),
        OfferAnswerParameters::AudioDirection, MediaDirection::JANUS_SDP_SENDONLY,
        OfferAnswerParameters::Video, 1,
        OfferAnswerParameters::VideoCodec, VIDEO_CODEC.to_cstr().as_ptr(),
        OfferAnswerParameters::VideoPayloadType, video_payload_type.unwrap_or(100),
        OfferAnswerParameters::VideoDirection, MediaDirection::JANUS_SDP_SENDONLY,
    );
    janus_huge!("Storing subscriber offer for {:?}: {}", from, subscriber_offer.to_string().to_str().unwrap());

    let switchboard = STATE.switchboard.read().expect("Switchboard lock poisoned; can't continue.");
    let jsep = json!({ "type": "offer", "sdp": subscriber_offer });
    match send_offer(&jsep, switchboard.subscribers_to(from)) {
        Ok(_) => (),
        Err(JanusError { code: 458 }) /* session not found */ => (),
        Err(e) => janus_err!("Error notifying subscribers about new offer: {}", e)
    };
    *from.subscriber_offer.lock().unwrap() = Some(subscriber_offer);
    Ok(json!({ "type": "answer", "sdp": answer }))
}

fn process_answer(_from: &Session, _answer: &Sdp) -> JsepResult {
    Ok(json!({})) // todo: check that this guy should actually be sending us an answer?
}

fn process_jsep(from: &Session, jsep: &JanssonValue) -> JsepResult {
    let jsep_str = jsep.to_libcstring(JanssonEncodingFlags::empty());
    let jsep_contents: OptionalField<JsepKind> = serde_json::from_str(jsep_str.to_str()?)?;
    match jsep_contents {
        OptionalField::None {} => Ok(json!({})),
        OptionalField::Some(kind) => {
            janus_info!("Processing {:?} from {:?}.", kind, from);
            match kind {
                JsepKind::Offer { sdp } => process_offer(from, &sdp),
                JsepKind::Answer { sdp } => process_answer(from, &sdp),
            }
        }
    }
}

fn push_response(from: &Session, txn: TransactionId, body: &JsonValue, jsep: Option<JsonValue>) -> JanusResult {
    let push_event = gateway_callbacks().push_event;
    let jsep = jsep.unwrap_or_else(|| json!({}));
    janus_info!("{:?} sending response to {:?}: body = {}.", from.as_ptr(), txn, body);
    JanusError::from(push_event(from.as_ptr(), &mut PLUGIN, txn.0, from_serde_json(body).as_mut_ref(), from_serde_json(&jsep).as_mut_ref()))
}

fn handle_message_async(RawMessage { jsep, msg, txn, from }: RawMessage) -> JanusResult {
    if let Some(ref from) = from.upgrade() {
        let destroyed = from.destroyed.lock().expect("Session destruction mutex is poisoned :(");
        if !*destroyed {
            // handle the message first, because handling a JSEP can cause us to want to send an RTCP
            // FIR to our subscribers, which may have been established in the message
            let msg_result = msg.map(|x| process_message(from, &x));
            let jsep_result = jsep.map(|x| process_jsep(from, &x));
            return match (msg_result, jsep_result) {
                (Some(Err(msg_err)), _) => {
                    let resp = json!({ "success": false, "error": { "msg": format!("{}", msg_err) }});
                    push_response(from, txn, &resp, None)
                }
                (_, Some(Err(jsep_err))) => {
                    let resp = json!({ "success": false, "error": { "msg": format!("{}", jsep_err) }});
                    push_response(from, txn, &resp, None)
                }
                (Some(Ok(msg_resp)), None) => {
                    let msg_body = msg_resp.body.map_or(json!({ "success": true }), |x| {
                        json!({ "success": true, "response": x })
                    });
                    push_response(from, txn, &msg_body, msg_resp.jsep)
                }
                (None, Some(Ok(jsep_resp))) => {
                    push_response(from, txn, &json!({ "success": true }), Some(jsep_resp))
                }
                (Some(Ok(msg_resp)), Some(Ok(jsep_resp))) => {
                    let msg_body = msg_resp.body.map_or(json!({ "success": true }), |x| {
                        json!({ "success": true, "response": x })
                    });
                    push_response(from, txn, &msg_body, Some(jsep_resp))
                }
                (None, None) => {
                    push_response(from, txn, &json!({ "success": true }), None)
                }
            }
        }
    }

    // getting messages for destroyed connections is slightly concerning,
    // because messages shouldn't be backed up for that long, so warn if it happens
    Ok(janus_warn!("Message received for destroyed session; discarding."))
}

extern "C" fn handle_message(handle: *mut PluginSession, transaction: *mut c_char,
                             message: *mut RawJanssonValue, jsep: *mut RawJanssonValue) -> *mut RawPluginResult {
    janus_verb!("Queueing signalling message.");
    let result = match unsafe { Session::from_ptr(handle) } {
        Ok(sess) => {
            let msg = RawMessage {
                from: Arc::downgrade(&sess),
                txn: TransactionId(transaction),
                msg: unsafe { JanssonValue::from_raw(message) },
                jsep: unsafe { JanssonValue::from_raw(jsep) }
            };
            STATE.message_channel.get().unwrap().send(msg).ok();
            PluginResult::ok_wait(Some(c_str!("Processing.")))
        },
        Err(_) => PluginResult::error(c_str!("No handle associated with message!"))
    };
    result.into_raw()
}

const PLUGIN: Plugin = build_plugin!(
    LibraryMetadata {
        api_version: 10,
        version: 1,
        name: c_str!("Janus SFU plugin"),
        package: c_str!("janus.plugin.sfu"),
        version_str: c_str!(env!("CARGO_PKG_VERSION")),
        description: c_str!(env!("CARGO_PKG_DESCRIPTION")),
        author: c_str!(env!("CARGO_PKG_AUTHORS")),
    },
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
