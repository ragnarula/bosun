//! Web Push: the control plane's signing key, a message encrypted to one
//! browser (RFC 8291) and signed for its push service (RFC 8292), and the loop
//! that notifies every subscribed browser when a session tree asks the reader
//! a question or completes its tasks.

use std::collections::HashMap;
use std::time::Duration;
use std::time::SystemTime;

use anyhow::Context;
use anyhow::anyhow;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bosun_common::error::ErrorExt;
use bosun_common::session::Block;
use bosun_common::session::Session;
use bosun_common::session::SessionOverview;
use bosun_common::session::SessionState;
use bosun_store::store::PushSubscription;
use bosun_store::store::Store;
use bosun_store::store::StoreError;
use ring::aead;
use ring::agreement;
use ring::hmac;
use ring::rand::SecureRandom;
use ring::rand::SystemRandom;
use ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING;
use ring::signature::EcdsaKeyPair;
use ring::signature::KeyPair;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use tracing::debug;
use tracing::info;
use tracing::instrument;
use tracing::warn;

/// How often the loop reads the trees' state while a browser is subscribed.
/// The pane reads the session list as often.
const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// The record size written into each message's header. A push message is one
/// record, and RFC 8291 caps the whole body at 4096 bytes.
const RECORD_SIZE: u32 = 4096;

/// The longest notification text the loop sends, in characters. The body
/// stays well under the 4096-byte cap after the JSON, the header, the padding
/// delimiter and the tag.
const BODY_CHARS: usize = 300;

/// How long a push service keeps a message for a browser that is offline. A
/// question still waits after a day; a notice older than that is stale.
const TTL_SECS: u32 = 24 * 3600;

/// How long a VAPID signature is valid. RFC 8292 allows at most 24 hours.
const SIGNATURE_SECS: i64 = 12 * 3600;

/// The `sub` claim for a pane that subscribed from an origin that is not an
/// https URL, such as a pane on `http://localhost`. RFC 8292 asks for a
/// `mailto:` or `https:` contact.
const FALLBACK_SUBJECT: &str = "mailto:bosun@localhost";

/// The control plane's push signing key: the key browsers subscribe with, and
/// the key that signs each message for its push service.
pub struct PushKey {
    pair: EcdsaKeyPair,
}

impl PushKey {
    /// Reads the stored key, or stores a new one when there is none.
    pub async fn load(store: &Store) -> Result<Self, StoreError> {
        let rng = SystemRandom::new();
        let candidate = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .map_err(|_| anyhow!("failed to generate a push key"))?;
        let pkcs8 = store.keep_push_key(candidate.as_ref().to_vec()).await?;
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8, &rng)
            .map_err(|error| anyhow!("the stored push key does not parse: {error}"))?;
        Ok(Self { pair })
    }

    /// The public key, uncompressed and base64url, as a browser's
    /// `applicationServerKey` takes it.
    pub fn public_key(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.pair.public_key().as_ref())
    }

    /// The `Authorization` header value for a message to `endpoint`: a JWT
    /// whose audience is the push service's origin, signed with ES256.
    fn authorization(&self, endpoint: &reqwest::Url, subject: &str, now_secs: i64) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::json!({
            "aud": endpoint.origin().ascii_serialization(),
            "exp": now_secs + SIGNATURE_SECS,
            "sub": subject,
        });
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
        let signed = format!("{header}.{claims}");
        let signature = self
            .pair
            .sign(&SystemRandom::new(), signed.as_bytes())
            .expect("ECDSA signing with a valid key does not fail");
        format!(
            "vapid t={signed}.{}, k={}",
            URL_SAFE_NO_PAD.encode(signature.as_ref()),
            self.public_key()
        )
    }
}

/// A subscription the pane posts, in the shape `PushSubscription.toJSON()`
/// gives it.
#[derive(Debug, Deserialize)]
pub struct SubscriptionBody {
    pub endpoint: String,
    pub keys: SubscriptionKeys,
}

#[derive(Debug, Deserialize)]
pub struct SubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}

#[derive(Debug, Error)]
pub enum SubscriptionError {
    #[error("the push endpoint must be an https URL")]
    Endpoint,
    #[error("the subscription's {0} key is not a base64url key of the right length")]
    Key(&'static str),
}

/// Checks a posted subscription. The control plane posts every notification
/// to the endpoint, so only an https URL is kept, and the keys must be ones
/// a message can be encrypted to.
pub fn subscription(
    body: SubscriptionBody,
    origin: &str,
) -> Result<PushSubscription, SubscriptionError> {
    let endpoint = reqwest::Url::parse(&body.endpoint).map_err(|_| SubscriptionError::Endpoint)?;
    if endpoint.scheme() != "https" || endpoint.host_str().is_none() {
        return Err(SubscriptionError::Endpoint);
    }
    let decoded = |key: &str| URL_SAFE_NO_PAD.decode(key.trim_end_matches('='));
    if !decoded(&body.keys.p256dh).is_ok_and(|key| key.len() == 65 && key[0] == 4) {
        return Err(SubscriptionError::Key("p256dh"));
    }
    if !decoded(&body.keys.auth).is_ok_and(|key| key.len() == 16) {
        return Err(SubscriptionError::Key("auth"));
    }
    Ok(PushSubscription {
        endpoint: body.endpoint,
        p256dh: body.keys.p256dh,
        auth: body.keys.auth,
        origin: origin.to_string(),
    })
}

/// What one notification says, as the service worker reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notice {
    pub title: String,
    pub body: String,
    /// The tree's root id. A notification with the same tag replaces the
    /// last one, so a tree has one notification at a time.
    pub tag: String,
    /// The pane address that opens the tree.
    pub url: String,
}

/// The two states of a tree that notify, read at one moment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TreeState {
    /// The root's newest message is a question for the reader. A child's
    /// question reaches the reader through the root.
    pub asking: bool,
    /// The tree has tasks and every one of them is done.
    pub tasks_complete: bool,
}

/// Why a tree notifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Question,
    TasksComplete,
}

/// The trees that changed into a state that notifies between two reads. A
/// tree missing from `before` started after it, and notifies like one that
/// was in neither state. A tree that starts asking and completes its tasks in
/// one interval notifies once, for its question.
pub fn triggers(
    before: &HashMap<String, TreeState>,
    now: &HashMap<String, TreeState>,
) -> Vec<(String, Trigger)> {
    let mut fired: Vec<(String, Trigger)> = now
        .iter()
        .filter_map(|(tree, state)| {
            let was = before.get(tree).copied().unwrap_or_default();
            if state.asking && !was.asking {
                Some((tree.clone(), Trigger::Question))
            } else if state.tasks_complete && !was.tasks_complete {
                Some((tree.clone(), Trigger::TasksComplete))
            } else {
                None
            }
        })
        .collect();
    fired.sort_by(|a, b| a.0.cmp(&b.0));
    fired
}

/// Each live tree's state, by its root's id, from its members' overviews.
/// A member with no overview counts as one with no question and no tasks.
pub fn tree_states(members: &[(Session, SessionOverview)]) -> HashMap<String, TreeState> {
    let mut tasks: HashMap<&str, (usize, usize)> = HashMap::new();
    let mut states: HashMap<String, TreeState> = HashMap::new();
    for (session, overview) in members {
        let counts = tasks.entry(&session.owner_id).or_default();
        counts.0 += overview.tasks.total;
        counts.1 += overview.tasks.done;
        let state = states.entry(session.owner_id.clone()).or_default();
        if session.id == session.owner_id {
            state.asking = overview.asking;
        }
    }
    for (tree, state) in states.iter_mut() {
        let (total, done) = tasks[tree.as_str()];
        state.tasks_complete = total > 0 && done == total;
    }
    states
}

fn truncate(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(BODY_CHARS) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

/// The notification for a tree whose root is `root`. `question` is the root's
/// unanswered question, when it has one.
pub fn notice(root: &Session, trigger: Trigger, question: Option<&str>) -> Notice {
    let who = root.persona.as_deref().unwrap_or("Bosun");
    let what = root
        .summary
        .as_deref()
        .or(root.prompt.as_deref())
        .unwrap_or(&root.id);
    let (title, body) = match trigger {
        Trigger::Question => (format!("{who} asks"), question.unwrap_or(what)),
        Trigger::TasksComplete => ("All tasks complete".to_string(), what),
    };
    Notice {
        title,
        body: truncate(body),
        tag: root.id.clone(),
        url: format!("/#s={}", url_component(&root.id)),
    }
}

fn url_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> hmac::Tag {
    let mut context = hmac::Context::with_key(&hmac::Key::new(hmac::HMAC_SHA256, key));
    for part in parts {
        context.update(part);
    }
    context.sign()
}

/// The `aes128gcm` body of one push message (RFC 8291 section 3 and RFC 8188),
/// from the ECDH secret the sender's key and the browser's key share.
fn seal(
    ecdh_secret: &[u8],
    auth_secret: &[u8],
    ua_public: &[u8],
    as_public: &[u8],
    salt: &[u8; 16],
    plaintext: &[u8],
) -> Vec<u8> {
    let prk_key = hmac_sha256(auth_secret, &[ecdh_secret]);
    let ikm = hmac_sha256(
        prk_key.as_ref(),
        &[b"WebPush: info\0", ua_public, as_public, &[1]],
    );
    let prk = hmac_sha256(salt, &[ikm.as_ref()]);
    let cek = hmac_sha256(prk.as_ref(), &[b"Content-Encoding: aes128gcm\0", &[1]]);
    let nonce = hmac_sha256(prk.as_ref(), &[b"Content-Encoding: nonce\0", &[1]]);

    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_128_GCM, &cek.as_ref()[..16])
            .expect("a 16-byte key is an AES-128 key"),
    );
    let nonce = aead::Nonce::try_assume_unique_for_key(&nonce.as_ref()[..12])
        .expect("a 12-byte nonce is a GCM nonce");
    // One record, so the padding delimiter is the last-record one.
    let mut record = Vec::with_capacity(plaintext.len() + 1 + aead::AES_128_GCM.tag_len());
    record.extend_from_slice(plaintext);
    record.push(2);
    key.seal_in_place_append_tag(nonce, aead::Aad::empty(), &mut record)
        .expect("a message under the record size seals");

    let mut body = Vec::with_capacity(16 + 4 + 1 + as_public.len() + record.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(as_public.len() as u8);
    body.extend_from_slice(as_public);
    body.extend_from_slice(&record);
    body
}

/// Encrypts `plaintext` to one browser's keys with a new sender key and salt.
fn encrypt(subscription: &PushSubscription, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let ua_public = URL_SAFE_NO_PAD
        .decode(subscription.p256dh.trim_end_matches('='))
        .context("the subscription's p256dh key is not base64url")?;
    let auth_secret = URL_SAFE_NO_PAD
        .decode(subscription.auth.trim_end_matches('='))
        .context("the subscription's auth key is not base64url")?;
    let rng = SystemRandom::new();
    let as_private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)
        .map_err(|_| anyhow!("failed to generate a sender key"))?;
    let as_public = as_private
        .compute_public_key()
        .map_err(|_| anyhow!("failed to compute the sender's public key"))?;
    let mut salt = [0u8; 16];
    rng.fill(&mut salt)
        .map_err(|_| anyhow!("failed to generate a salt"))?;
    let ecdh_secret = agreement::agree_ephemeral(
        as_private,
        &agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, &ua_public),
        |secret| secret.to_vec(),
    )
    .map_err(|_| anyhow!("the subscription's p256dh key is not a P-256 point"))?;
    Ok(seal(
        &ecdh_secret,
        &auth_secret,
        &ua_public,
        as_public.as_ref(),
        &salt,
        plaintext,
    ))
}

/// A push service's `Topic` for a tree: a newer message with the same topic
/// replaces one the service still holds. RFC 8030 allows 32 characters of the
/// base64url alphabet.
fn topic(tree: &str) -> String {
    tree.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect()
}

/// What a push service said to one message.
#[derive(Debug, PartialEq, Eq)]
enum Delivery {
    Accepted,
    /// The subscription has expired or was removed by the browser.
    Gone,
}

async fn send(
    client: &reqwest::Client,
    key: &PushKey,
    subscription: &PushSubscription,
    notice: &Notice,
) -> anyhow::Result<Delivery> {
    let endpoint =
        reqwest::Url::parse(&subscription.endpoint).context("the endpoint is not a URL")?;
    let payload = serde_json::to_vec(notice).context("failed to serialize the notice")?;
    let body = encrypt(subscription, &payload)?;
    let subject = if subscription.origin.starts_with("https://") {
        subscription.origin.as_str()
    } else {
        FALLBACK_SUBJECT
    };
    let now_secs = bosun_common::time::unix_secs(SystemTime::now());
    let response = client
        .post(endpoint.clone())
        .header("TTL", TTL_SECS.to_string())
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("Urgency", "high")
        .header("Topic", topic(&notice.tag))
        .header(
            "Authorization",
            key.authorization(&endpoint, subject, now_secs),
        )
        .body(body)
        .send()
        .await
        .context("the push service did not answer")?;
    match response.status().as_u16() {
        200..=299 => Ok(Delivery::Accepted),
        404 | 410 => Ok(Delivery::Gone),
        status => {
            let text = response.text().await.unwrap_or_default();
            Err(anyhow!(
                "the push service answered {status}: {}",
                truncate(&text)
            ))
        }
    }
}

/// Sends `notice` to every subscription, and removes the ones the push
/// service says are gone. One failed browser does not stop the others.
async fn deliver(
    store: &Store,
    client: &reqwest::Client,
    key: &PushKey,
    subscriptions: &[PushSubscription],
    notice: &Notice,
) {
    for subscription in subscriptions {
        // The endpoint's path is the browser's address at its push service,
        // so only the host is logged.
        let host = reqwest::Url::parse(&subscription.endpoint)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .unwrap_or_default();
        match send(client, key, subscription, notice).await {
            Ok(Delivery::Accepted) => {
                debug!(push_host = %host, tree = %notice.tag, "notification sent")
            }
            Ok(Delivery::Gone) => {
                info!(push_host = %host, "push subscription gone; removing it");
                if let Err(error) = store.remove_push_subscription(&subscription.endpoint).await {
                    warn!(
                        error = %error.display_chain(),
                        "failed to remove a gone push subscription"
                    );
                }
            }
            Err(error) => warn!(
                push_host = %host,
                tree = %notice.tag,
                error = %error.display_chain(),
                "notification not sent"
            ),
        }
    }
}

/// Every live session with its overview. A session removed between the list
/// and its overview is left out.
async fn live_members(store: &Store) -> Result<Vec<(Session, SessionOverview)>, StoreError> {
    let mut members = Vec::new();
    for session in store.list_sessions().await? {
        if matches!(
            session.state,
            SessionState::Creating | SessionState::Stopped
        ) {
            continue;
        }
        match store.session_overview(&session.id).await {
            Ok(overview) => members.push((session, overview)),
            Err(StoreError::SessionNotFound { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(members)
}

/// The root's unanswered question, when its newest message is one.
async fn question(store: &Store, root: &str) -> Option<String> {
    match store.last_message(root).await {
        Ok(Some(message)) => match message.block {
            Block::Ask {
                message,
                answer: None,
                ..
            } => Some(message),
            _ => None,
        },
        _ => None,
    }
}

/// Reads the trees every `POLL_INTERVAL` while any browser is subscribed, and
/// notifies every subscribed browser of each tree that started asking or
/// completed its tasks since the read before. The first read after a start,
/// or after a time with no subscriptions, only records the trees' state, so a
/// restart does not repeat old notifications.
#[instrument(skip_all)]
pub async fn run(store: Store) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("a reqwest client with a timeout builds");
    let mut key: Option<PushKey> = None;
    let mut before: Option<HashMap<String, TreeState>> = None;
    let mut ticks = tokio::time::interval(POLL_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let result = async {
            let subscriptions = store.push_subscriptions().await?;
            if subscriptions.is_empty() {
                before = None;
                return Ok::<(), StoreError>(());
            }
            let members = live_members(&store).await?;
            let now = tree_states(&members);
            let Some(was) = before.replace(now.clone()) else {
                return Ok(());
            };
            let fired = triggers(&was, &now);
            if fired.is_empty() {
                return Ok(());
            }
            if key.is_none() {
                key = Some(PushKey::load(&store).await?);
            }
            let key = key.as_ref().expect("the key was loaded above");
            for (tree, trigger) in fired {
                let Some((root, _)) = members.iter().find(|(session, _)| session.id == tree) else {
                    continue;
                };
                let asked = match trigger {
                    Trigger::Question => question(&store, &tree).await,
                    Trigger::TasksComplete => None,
                };
                let notice = notice(root, trigger, asked.as_deref());
                deliver(&store, &client, key, &subscriptions, &notice).await;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            warn!(error = %error.display_chain(), "push notifications skipped a read");
        }
    }
}

#[cfg(test)]
mod tests {
    use bosun_common::session::Permission;
    use bosun_common::session::TaskCounts;
    use ring::signature::ECDSA_P256_SHA256_FIXED;
    use ring::signature::UnparsedPublicKey;

    use super::*;

    fn b64(text: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(text).unwrap()
    }

    /// RFC 8291 section 5 and appendix A: the example message, from its ECDH
    /// secret, salt and keys to the body sent.
    #[test]
    fn a_message_seals_to_the_rfc_8291_example() {
        let salt: [u8; 16] = b64("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let body = seal(
            &b64("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs"),
            &b64("BTBZMqHH6r4Tts7J_aSIgg"),
            &b64(
                "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
            ),
            &b64(
                "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8",
            ),
            &salt,
            b"When I grow up, I want to be a watermelon",
        );
        assert_eq!(
            URL_SAFE_NO_PAD.encode(body),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml\
             mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT\
             pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
    }

    #[test]
    fn a_message_to_a_browser_carries_a_new_sender_key_each_time() {
        let browser =
            agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &SystemRandom::new())
                .unwrap();
        let subscription = PushSubscription {
            endpoint: "https://push.example/a".into(),
            p256dh: URL_SAFE_NO_PAD.encode(browser.compute_public_key().unwrap().as_ref()),
            auth: URL_SAFE_NO_PAD.encode([7u8; 16]),
            origin: "https://pane.example".into(),
        };
        let first = encrypt(&subscription, b"hello").unwrap();
        let second = encrypt(&subscription, b"hello").unwrap();
        assert_eq!(
            &first[16..21],
            &[0, 0, 16, 0, 65],
            "record size and key length"
        );
        assert_ne!(
            &first[21..86],
            &second[21..86],
            "each message has its own sender key"
        );
        assert_eq!(first.len(), 86 + 5 + 1 + 16, "header, text, delimiter, tag");
    }

    #[tokio::test]
    async fn the_signature_verifies_with_the_key_browsers_subscribe_with() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("store.db")).unwrap();
        let key = PushKey::load(&store).await.unwrap();
        assert_eq!(
            PushKey::load(&store).await.unwrap().public_key(),
            key.public_key(),
            "the key survives a restart, so subscriptions stay valid"
        );

        let endpoint = reqwest::Url::parse("https://fcm.example:8443/send/abc").unwrap();
        let header = key.authorization(&endpoint, "https://pane.example", 1_000);
        let (token, k) = header
            .strip_prefix("vapid t=")
            .unwrap()
            .split_once(", k=")
            .unwrap();
        assert_eq!(k, key.public_key());
        let (signed, signature) = token.rsplit_once('.').unwrap();
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, b64(k))
            .verify(signed.as_bytes(), &b64(signature))
            .expect("the push service verifies the JWT with k");
        let claims: serde_json::Value =
            serde_json::from_slice(&b64(signed.split_once('.').unwrap().1)).unwrap();
        assert_eq!(
            claims,
            serde_json::json!({
                "aud": "https://fcm.example:8443",
                "exp": 1_000 + SIGNATURE_SECS,
                "sub": "https://pane.example",
            })
        );
    }

    fn body(endpoint: &str, p256dh: &str, auth: &str) -> SubscriptionBody {
        SubscriptionBody {
            endpoint: endpoint.into(),
            keys: SubscriptionKeys {
                p256dh: p256dh.into(),
                auth: auth.into(),
            },
        }
    }

    #[test]
    fn only_an_https_endpoint_with_usable_keys_is_kept() {
        let point = URL_SAFE_NO_PAD.encode([[4u8].as_slice(), &[9u8; 64]].concat());
        let auth = URL_SAFE_NO_PAD.encode([1u8; 16]);
        assert!(subscription(body("https://push.example/a", &point, &auth), "o").is_ok());
        assert!(matches!(
            subscription(body("http://push.example/a", &point, &auth), "o"),
            Err(SubscriptionError::Endpoint)
        ));
        assert!(matches!(
            subscription(body("not a url", &point, &auth), "o"),
            Err(SubscriptionError::Endpoint)
        ));
        assert!(matches!(
            subscription(body("https://push.example/a", "AAAA", &auth), "o"),
            Err(SubscriptionError::Key("p256dh"))
        ));
        assert!(matches!(
            subscription(body("https://push.example/a", &point, "AAAA"), "o"),
            Err(SubscriptionError::Key("auth"))
        ));
        assert!(matches!(
            subscription(body("https://push.example/a", &point, "!!"), "o"),
            Err(SubscriptionError::Key("auth"))
        ));
    }

    fn state(asking: bool, tasks_complete: bool) -> TreeState {
        TreeState {
            asking,
            tasks_complete,
        }
    }

    #[test]
    fn a_tree_notifies_when_it_starts_asking_or_completes_its_tasks() {
        let before = HashMap::from([
            ("asks".to_string(), state(false, false)),
            ("still-asks".to_string(), state(true, false)),
            ("completes".to_string(), state(false, false)),
            ("stays-complete".to_string(), state(false, true)),
            ("both".to_string(), state(false, false)),
        ]);
        let now = HashMap::from([
            ("asks".to_string(), state(true, false)),
            ("still-asks".to_string(), state(true, false)),
            ("completes".to_string(), state(false, true)),
            ("stays-complete".to_string(), state(false, true)),
            ("both".to_string(), state(true, true)),
            ("new-and-asking".to_string(), state(true, false)),
            ("new-and-idle".to_string(), state(false, false)),
        ]);
        assert_eq!(
            triggers(&before, &now),
            vec![
                ("asks".to_string(), Trigger::Question),
                ("both".to_string(), Trigger::Question),
                ("completes".to_string(), Trigger::TasksComplete),
                ("new-and-asking".to_string(), Trigger::Question),
            ]
        );
    }

    #[test]
    fn a_tree_that_answers_and_asks_again_notifies_again() {
        let asking = HashMap::from([("t".to_string(), state(true, false))]);
        let answered = HashMap::from([("t".to_string(), state(false, false))]);
        assert!(triggers(&asking, &answered).is_empty());
        assert_eq!(
            triggers(&answered, &asking),
            vec![("t".to_string(), Trigger::Question)]
        );
    }

    fn member(
        id: &str,
        owner: &str,
        asking: bool,
        total: usize,
        done: usize,
    ) -> (Session, SessionOverview) {
        (
            Session {
                id: id.into(),
                node: "n".into(),
                repo_url: None,
                git_ref: None,
                dir: "/w".into(),
                model: "m".into(),
                persona: Some("lead".into()),
                parent_id: (id != owner).then(|| owner.to_string()),
                owner_id: owner.into(),
                permission: Permission::ReadWrite,
                allowed_tools: "*".into(),
                mcp_servers: String::new(),
                state: SessionState::WaitingForInput,
                interrupt_cause: None,
                created_at_secs: 0,
                prompt: Some("fix the build".into()),
                summary: None,
            },
            SessionOverview {
                asking,
                tasks: TaskCounts {
                    total,
                    done,
                    in_progress: 0,
                },
                ..SessionOverview::default()
            },
        )
    }

    #[test]
    fn a_tree_asks_through_its_root_and_completes_when_all_its_members_tasks_are_done() {
        let states = tree_states(&[
            member("a", "a", false, 2, 2),
            member("a1", "a", true, 1, 1),
            member("b", "b", true, 2, 1),
            member("b1", "b", false, 1, 1),
            member("c", "c", false, 0, 0),
        ]);
        assert_eq!(
            states["a"],
            state(false, true),
            "a child's question is not the reader's"
        );
        assert_eq!(
            states["b"],
            state(true, false),
            "one open task keeps the tree open"
        );
        assert_eq!(states["c"], state(false, false), "no tasks is not complete");
    }

    #[test]
    fn a_notice_names_the_question_or_the_tree_and_opens_its_root() {
        let (mut root, _) = member("s 1", "s 1", true, 0, 0);
        let asked = notice(&root, Trigger::Question, Some("Merge now?"));
        assert_eq!(asked.title, "lead asks");
        assert_eq!(asked.body, "Merge now?");
        assert_eq!(asked.tag, "s 1");
        assert_eq!(asked.url, "/#s=s%201");

        root.summary = Some("Fixing the build".into());
        let done = notice(&root, Trigger::TasksComplete, None);
        assert_eq!(done.title, "All tasks complete");
        assert_eq!(done.body, "Fixing the build");

        let long = "x".repeat(BODY_CHARS + 50);
        let cut = notice(&root, Trigger::Question, Some(&long));
        assert_eq!(cut.body.chars().count(), BODY_CHARS + 1);
    }

    #[test]
    fn a_topic_keeps_only_base64url_characters_and_32_of_them() {
        assert_eq!(topic("ab.c-d_e"), "abc-d_e");
        assert_eq!(topic(&"x".repeat(40)).len(), 32);
    }
}
