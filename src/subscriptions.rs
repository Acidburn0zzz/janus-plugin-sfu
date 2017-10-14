/// Types and code related to managing session subscriptions to incoming data.

use sessions::Session;
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use entityids::UserId;

bitflags! {
    /// A particular kind of traffic transported over a connection.
    #[derive(Serialize, Deserialize)]
    pub struct ContentKind: u8 {
        /// Audio traffic.
        const AUDIO = 0b00000001;
        /// Video traffic.
        const VIDEO = 0b00000010;
        /// Data channel traffic.
        const DATA = 0b00000100;
    }
}

/// Indicates that traffic of a particular kind should be routed to a particular session,
/// i.e. the session "subscribes" to the traffic.
#[derive(Debug)]
pub struct Subscription {

    /// The subscriber to this traffic. Null if the subscriber has been destroyed since subscribing.
    pub sess: Weak<Session>,

    /// The kind or kinds of traffic subscribed to.
    pub kind: ContentKind
}

impl Subscription {
    pub fn new(sess: &Arc<Session>, kind: ContentKind) -> Self {
        Self { sess: Arc::downgrade(sess), kind }
    }
}

/// A data structure mapping publishers to subscribers.
///
/// The special key None indicates that a given subscription is meant to subscribe to all publishers
/// (even ones that didn't exist when the subscription was established.)
pub type SubscriptionMap = HashMap<UserId, Vec<Subscription>>;

pub fn subscribe(subscriptions: &mut SubscriptionMap, sess: &Arc<Session>, kind: ContentKind, publisher: UserId) {
    subscriptions.entry(publisher).or_insert_with(Vec::new).push(Subscription::new(sess, kind));
}

pub fn unsubscribe(subscriptions: &mut SubscriptionMap, sess: &Arc<Session>, kind: ContentKind, publisher: UserId) {
    subscriptions.entry(publisher).or_insert_with(Vec::new).retain(|ref sub| {
        let matches = if let Some(s) = sub.sess.upgrade() { s.handle == sess.handle && sub.kind == kind } else { false };
        !matches
    });
}

pub fn unpublish(subscriptions: &mut SubscriptionMap, publisher: UserId) {
    subscriptions.remove(&publisher);
}

pub fn subscribers_to(subscriptions: &SubscriptionMap, publisher: UserId, kind: ContentKind) -> Vec<&Subscription> {
    let all_subscriptions = subscriptions.get(&publisher).map(Vec::as_slice).unwrap_or(&[]).iter();
    all_subscriptions.filter(|s| s.kind.contains(kind)).collect()
}

pub fn for_each_subscriber<T>(subscriptions: &SubscriptionMap, publisher: UserId, kind: ContentKind, send: T) where T: Fn(&Session) {
    for subscription in subscribers_to(subscriptions, publisher, kind) {
        if let Some(subscriber_sess) = subscription.sess.upgrade() {
            send(subscriber_sess.as_ref());
        }
    }
}
