/// Types and code related to handling signalling messages.
use super::{Sdp, JanssonEncodingFlags, JanssonValue};
use super::serde_json;
use std::error::Error;
use std::ffi::CStr;
use std::fmt;
use std::os::raw::c_char;
use serde::de::DeserializeOwned;

/// A Janus transaction ID. Used to correlate signalling requests and responses.
#[derive(Debug)]
pub struct TransactionId(pub *mut c_char);

unsafe impl Send for TransactionId {}

impl fmt::Display for TransactionId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        unsafe {
            if self.0.is_null() {
                f.write_str("<null>")
            } else {
                let contents = CStr::from_ptr(self.0);
                f.write_str(&contents.to_string_lossy())
            }
        }
    }
}

/// A room ID representing a Janus multicast room.
pub type RoomId = String;

/// A user ID representing a single Janus client. Used to correlate multiple Janus connections back to the same
/// conceptual user for managing subscriptions.
pub type UserId = String;

/// Useful to represent a JSON message field which may or may not be present.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
#[serde(deny_unknown_fields)]
pub enum OptionalField<T> {
    Some(T),
    None {}
}

impl<T> Into<Option<T>> for OptionalField<T> {
    fn into(self) -> Option<T> {
        match self {
            OptionalField::None {} => None,
            OptionalField::Some(x) => Some(x)
        }
    }
}

impl<T> OptionalField<T> where T: DeserializeOwned {
    pub fn try_parse(val: &Option<JanssonValue>) -> Option<Result<T, Box<Error>>> {
        val.as_ref().and_then(|x| match parse_json::<OptionalField<T>>(x).map(|x| x.into()) {
            Ok(None) => None,
            Ok(Some(y)) => Some(Ok(y)),
            Err(e) => Some(Err(e))
        })
    }
}

fn parse_json<T>(json: &JanssonValue) -> Result<T, Box<Error>> where T: DeserializeOwned {
    let json_str = json.to_libcstring(JanssonEncodingFlags::empty());
    Ok(serde_json::from_str::<T>(json_str.to_str()?)?)
}

/// A signalling message carrying a JSEP SDP offer or answer.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase", tag = "type")]
pub enum JsepKind {
    /// An offer to establish a connection.
    Offer { sdp: Sdp },

    /// An answer responding to an offer.
    Answer { sdp: Sdp },
}

/// The enumeration of all (non-JSEP) signalling messages which can be received from a client.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase", tag = "kind")]
pub enum MessageKind {
    /// Indicates that a client wishes to "join" a room on the server. Prior to this, no audio, video, or data
    /// received from the client will be forwarded to anyone.
    ///
    /// The "subscribe" field specifies which kind of traffic this client will receive. (Useful for saving a round
    /// trip if you wanted to both join and subscribe, as is typical.)
    Join {
        room_id: RoomId,
        user_id: UserId,
        subscribe: Option<Subscription>,
    },

    /// Indicates that a client wishes to subscribe to traffic described by the given subscription specification.
    Subscribe { what: Subscription },

    /// Indicates that a given user should be blocked from receiving your traffic, and that you should not
    /// receive their traffic (superseding any subscriptions you have.)
    Block { whom: UserId },

    /// Undoes a block targeting the given user.
    Unblock { whom: UserId },
}

/// Information about which traffic a client will get pushed to them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Subscription {
    /// Whether to subscribe to server-wide notifications (e.g. user joins and leaves, room creates and destroys).
    pub notifications: bool,

    /// Whether to subscribe to data in the currently-joined room.
    pub data: bool,

    /// Whether to subscribe to media (audio and video) from a particular user.
    pub media: Option<UserId>,
}

#[cfg(test)]
mod tests {

    use super::*;

    mod message_parsing {

        use super::*;
        use ::serde_json;

        #[test]
        fn parse_empty() {
            let json = r#"{}"#;
            let result: OptionalField<MessageKind> = serde_json::from_str(json).unwrap();
            assert_eq!(result, OptionalField::None {});
        }

        #[test]
        fn parse_inner_error() {
            let json = r#"{"kind": "join"}"#;
            let result: serde_json::Result<OptionalField<MessageKind>> = serde_json::from_str(json);
            assert!(result.is_err());
        }

        #[test]
        fn parse_outer_error() {
            let json = r#"{"kind": "fiddle"}"#;
            let result: serde_json::Result<OptionalField<MessageKind>> = serde_json::from_str(json);
            assert!(result.is_err());
        }

        #[test]
        fn parse_join_user_id() {
            let json = r#"{"kind": "join", "user_id": "10", "room_id": "alpha"}"#;
            let result: MessageKind = serde_json::from_str(json).unwrap();
            assert_eq!(result, MessageKind::Join {
                user_id: "10".into(),
                room_id: "alpha".into(),
                subscribe: None
            });
        }

        #[test]
        fn parse_join_subscriptions() {
            let json = r#"{"kind": "join", "user_id": "10", "room_id": "5", "subscribe": {"notifications": true, "data": false}}"#;
            let result: MessageKind = serde_json::from_str(json).unwrap();
            assert_eq!(result, MessageKind::Join {
                user_id: "10".into(),
                room_id: "5".into(),
                subscribe: Some(Subscription {
                    notifications: true,
                    data: false,
                    media: None
                })
            });
        }

        #[test]
        fn parse_subscribe() {
            let json = r#"{"kind": "subscribe", "what": {"notifications": false, "data": true, "media": "steve"}}"#;
            let result: MessageKind = serde_json::from_str(json).unwrap();
            assert_eq!(result, MessageKind::Subscribe {
                what: Subscription {
                    notifications: false,
                    data: true,
                    media: Some("steve".into())
                }
            });
        }
    }
}
