use crate::node::model::protocol::v1::{
    Message,
    NodeInfo,
};
use crate::node::model::protocol::ChallengeResponse;
use crate::node::model::protocol::Protocol;
use crate::es;
use crate::model::identity::{
    Identity,
    IdentityMethods,
};
use crate::utils::{
    BincodeSerializable,
};
use chrono::Utc;
use futures::channel::mpsc::unbounded;
use futures::channel::mpsc::UnboundedSender;
use generic_array::ArrayLength;
use generic_array::GenericArray;
use loga::{
    Log,
    ea,
    ResultContext,
    DebugDisplay,
    ErrContext,
};
use manual_future::ManualFuture;
use manual_future::ManualFutureCompleter;
use rand::RngCore;
use taskmanager::TaskManager;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fs;
use std::io::{
    ErrorKind,
};
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::path::PathBuf;
use std::sync::atomic::{
    AtomicUsize,
    AtomicBool,
};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::Instant;
use self::model::nodeidentity::{
    NodeIdentity,
    NodeSecret,
    NodeSecretMethods,
    NodeIdentityMethods,
};
use self::model::protocol::v1::{
    FindRequest,
    TempChallengeSigBody,
    Value,
};
use self::model::protocol::{
    Addr,
    ValueBody,
};
use self::model::protocol::FindMode;
use self::model::protocol::FindResponse;
use self::model::protocol::FindResponseBody;
use self::model::protocol::FindResponseModeBody;
use self::model::protocol::StoreRequest;

pub mod model;

const HASH_SIZE: usize = 256usize;
type DhtHash = GenericArray<u8, generic_array::typenum::U32>;
const NEIGHBORHOOD: usize = 8usize;
const PARALLEL: usize = 3;
const REQ_TIMEOUT: Duration = Duration::from_secs(5);
const SUPER_FRESH_DURATION: Duration = Duration::from_secs(60 * 60);
pub const DEFAULT_PORT: u16 = 40399;

pub fn default_bind() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), DEFAULT_PORT))
}

pub fn default_bootstrap() -> Vec<(SocketAddr, NodeIdentity)> {
    vec![]
}

fn diff<N: ArrayLength<u8>>(a: &GenericArray<u8, N>, b: &GenericArray<u8, N>) -> (usize, GenericArray<u8, N>) {
    let mut leading_zeros = 0usize;
    let mut first_one = false;
    let mut out: GenericArray<u8, N> = GenericArray::default();
    for i in 0 .. N::to_usize() {
        out[i] = a[i] ^ b[i];
        if !first_one {
            let byte_leading_zeros = out[i].leading_zeros();
            leading_zeros += byte_leading_zeros as usize;
            if byte_leading_zeros < 8 {
                first_one = true;
            }
        }
    }
    return (leading_zeros, out);
}

#[cfg(test)]
mod diff_tests {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;

    type SmallBytes = GenericArray<u8, generic_array::typenum::U2>;

    #[test]
    fn test_same() {
        let (lz, d) = diff(SmallBytes::from_slice(&[0u8, 0u8]), SmallBytes::from_slice(&[0u8, 0u8]));
        assert_eq!(lz, 16);
        assert_eq!(d.as_slice(), &[0u8, 0u8]);
    }

    #[test]
    fn test_lsb_diff() {
        let (lz, d) = diff(SmallBytes::from_slice(&[0u8, 1u8]), SmallBytes::from_slice(&[0u8, 0u8]));
        assert_eq!(lz, 15);
        assert_eq!(d.as_slice(), &[0u8, 1u8]);
    }

    #[test]
    fn test_msb_diff() {
        let (lz, d) = diff(SmallBytes::from_slice(&[128u8, 0u8]), SmallBytes::from_slice(&[0u8, 0u8]));
        assert_eq!(lz, 0);
        assert_eq!(d.as_slice(), &[128u8, 0u8]);
    }
}

fn hash(x: &dyn BincodeSerializable) -> DhtHash {
    let mut hash = sha2::Sha256::new();
    x.serialize_into(&mut hash);
    return hash.finalize();
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NodeState {
    node: NodeInfo,
    unresponsive: bool,
}

struct NextFindTimeout {
    end: Instant,
    key: (FindMode, usize),
}

#[derive(Clone)]
struct ValueState {
    value: Value,
    updated: Instant,
}

struct NextPingTimeout {
    end: Instant,
    key: (NodeIdentity, usize),
}

struct NextChallengeTimeout {
    end: Instant,
    key: (NodeIdentity, usize),
}

pub struct NodeInner {
    log: Log,
    own_id: NodeIdentity,
    own_id_hash: DhtHash,
    own_secret: NodeSecret,
    buckets: Mutex<[Vec<NodeState>; HASH_SIZE]>,
    store: Mutex<HashMap<Identity, ValueState>>,
    dirty: AtomicBool,
    socket: UdpSocket,
    next_req_id: AtomicUsize,
    find_timeouts: UnboundedSender<NextFindTimeout>,
    find_states: Mutex<HashMap<FindMode, FindState>>,
    ping_states: Mutex<HashMap<NodeIdentity, PingState>>,
    challenge_timeouts: UnboundedSender<NextChallengeTimeout>,
    challenge_states: Mutex<HashMap<NodeIdentity, ChallengeState>>,
}

#[derive(Clone)]
pub struct Node(Arc<NodeInner>);

#[derive(Clone)]
struct OutstandingNodeEntry {
    dist: DhtHash,
    leading_zeros: usize,
    challenge: Box<[u8]>,
    node: NodeInfo,
}

enum BestNodeEntryNode {
    Self_,
    Node(NodeInfo),
}

struct BestNodeEntry {
    dist: DhtHash,
    node: BestNodeEntryNode,
}

struct FindState {
    req_id: usize,
    mode: FindMode,
    target_hash: DhtHash,
    timeout: Instant,
    best: Vec<BestNodeEntry>,
    outstanding: Vec<OutstandingNodeEntry>,
    // for storing value, or retrieving value
    value: Option<Value>,
    futures: Vec<ManualFutureCompleter<Option<Value>>>,
}

struct PingState {
    req_id: usize,
    leading_zeros: usize,
}

struct ChallengeState {
    req_id: usize,
    challenge: Box<[u8]>,
    node: NodeInfo,
}

fn generate_challenge() -> Box<[u8]> {
    let mut out = Box::new([0u8; 32]);
    rand::thread_rng().fill_bytes(out.as_mut());
    return out;
}

fn sign_challenge<B: Serialize>(secret: &NodeSecret, t: TempChallengeSigBody<B>) -> Box<[u8]> {
    secret.sign(&t.to_bytes())
}

fn verify_challenge<
    B: Serialize,
>(id: &NodeIdentity, t: TempChallengeSigBody<B>, sig: Box<[u8]>) -> Result<(), loga::Error> {
    id.verify(&t.to_bytes(), &sig)
}

#[derive(Serialize, Deserialize)]
struct Persisted {
    own_secret: NodeSecret,
    initial_buckets: Vec<Vec<NodeState>>,
}

impl Node {
    pub async fn new(
        log: &Log,
        tm: TaskManager,
        addr: SocketAddr,
        bootstrap: &[NodeInfo],
        persist_path: Option<PathBuf>,
    ) -> Result<Node, loga::Error> {
        let log = log.fork(ea!(sys = "node"));
        let do_bootstrap;
        let initial_dirty;
        let (own_id, own_secret, initial_buckets) = {
            let persisted = match es!({
                if let Some(persist_path) = &persist_path {
                    let log = log.fork(ea!(path = persist_path.to_string_lossy()));
                    let b = match fs::read(persist_path) {
                        Ok(b) => b,
                        Err(e) => if e.kind() == ErrorKind::NotFound {
                            return Ok(None);
                        } else {
                            return Err(e.into());
                        },
                    };
                    let p = serde_json::from_slice::<Persisted>(&b)?;
                    let own_identity = p.own_secret.get_identity();
                    return Ok(
                        Some(
                            (
                                own_identity,
                                p.own_secret,
                                <[Vec<NodeState>; HASH_SIZE]>::try_from(
                                    p.initial_buckets,
                                ).map_err(
                                    |e| log.new_err("Bucket count mismatch", ea!(got = e.len(), want = HASH_SIZE)),
                                )?,
                            ),
                        ),
                    );
                } else {
                    return Ok(None);
                }
            }) {
                Ok(p) => p,
                Err(e) => {
                    log.warn_e(e, "Failed to load persisted state", ea!());
                    None
                },
            };
            if let Some(p) = persisted {
                initial_dirty = false;
                do_bootstrap = false;
                p
            } else {
                do_bootstrap = true;
                initial_dirty = true;
                let (own_id, own_secret) = NodeIdentity::new();
                (own_id, own_secret, array_init::array_init(|_| vec![]))
            }
        };
        log.info("Starting", ea!(id = own_id));
        let sock = {
            let log = log.fork(ea!(addr = addr));
            UdpSocket::bind(addr).await.log_context(&log, "Failed to open gnocchi UDP port", ea!())?
        };
        let own_id_hash = hash(&own_id);
        let (find_timeout_write, find_timeout_recv) = unbounded::<NextFindTimeout>();
        let (ping_timeout_write, ping_timeout_recv) = unbounded::<NextPingTimeout>();
        let (challenge_timeout_write, challenge_timeout_recv) = unbounded::<NextChallengeTimeout>();
        let dir = Node(Arc::new(NodeInner {
            log: log.clone(),
            own_id: own_id,
            own_secret: own_secret,
            own_id_hash: own_id_hash,
            buckets: Mutex::new(initial_buckets),
            dirty: AtomicBool::new(initial_dirty),
            store: Mutex::new(HashMap::new()),
            socket: sock,
            next_req_id: AtomicUsize::new(0),
            find_timeouts: find_timeout_write,
            find_states: Mutex::new(HashMap::new()),
            ping_states: Mutex::new(HashMap::new()),
            challenge_timeouts: challenge_timeout_write,
            challenge_states: Mutex::new(HashMap::new()),
        }));
        if do_bootstrap {
            for b in bootstrap {
                if !dir.add_good_node(b.id.clone(), Some(b.clone())) {
                    panic!("");
                }
            }
        }
        if let Some(persist_path) = persist_path {
            // Periodically save
            let tm = tm.clone();
            let dir = dir.clone();
            let log = log.fork(ea!(path = persist_path.to_string_lossy()));
            tm.periodic(Duration::from_secs(60 * 10), move || {
                let log = log.clone();
                let dir = dir.clone();
                let persist_path = persist_path.clone();
                async move {
                    if !dir.0.dirty.swap(false, Ordering::Relaxed) {
                        return;
                    }
                    let buckets = dir.0.buckets.lock().unwrap().clone();
                    match tokio::fs::write(persist_path, &serde_json::to_vec(&Persisted {
                        own_secret: dir.0.own_secret.clone(),
                        initial_buckets: buckets.into(),
                    }).unwrap()).await.context("Failed to write state to file", ea!()) {
                        Ok(_) => { },
                        Err(e) => log.warn_e(e, "Failed to persist state", ea!()),
                    };
                }
            });
        }
        {
            // Find timeouts
            let dir = dir.clone();
            tm.stream(find_timeout_recv, move |e| {
                let dir = dir.clone();
                async move {
                    tokio::time::sleep_until(e.end).await;
                    let state = {
                        let mut borrowed_states = dir.0.find_states.lock().unwrap();
                        let mut state_entry = match borrowed_states.entry(e.key.0.clone()) {
                            Entry::Occupied(s) => s,
                            Entry::Vacant(_) => return,
                        };
                        let state = state_entry.get_mut();
                        if state.req_id != e.key.1 {
                            // for old request, out of date
                            return;
                        }
                        if state.timeout > Instant::now() {
                            // time pushed back while this timeout was in the queue
                            return;
                        }
                        dir.0.log.info("Find timed out", ea!(key = &e.key.0.dbg_str()));
                        state_entry.remove()
                    };
                    for o in &state.outstanding {
                        dir.mark_node_unresponsive(o.node.id.clone(), o.leading_zeros, true);
                    }
                    dir.complete_state(state).await;
                }
            });
        }
        {
            // Stored data re-propagation
            let dir = dir.clone();
            tm.periodic(Duration::from_secs(60 * 60), move || {
                let dir = dir.clone();
                async move {
                    let mut unfresh = vec![];
                    dir.0.store.lock().unwrap().retain(|k, v| {
                        let now = Instant::now();
                        if v.updated + SUPER_FRESH_DURATION > now {
                            return false;
                        }
                        let signed = v.value.parse().unwrap();
                        if signed.expires < Utc::now() {
                            return false;
                        }
                        v.updated = now;
                        unfresh.push((k.clone(), v.clone()));
                        return true;
                    });
                    for (k, v) in unfresh {
                        dir.put(k.clone(), v.value.clone()).await;
                    }
                }
            })
        }
        {
            // Pings
            let dir = dir.clone();
            tm.periodic(Duration::from_secs(10 * 60), move || {
                let ping_timeout_write = ping_timeout_write.clone();
                let dir = dir.clone();
                async move {
                    for i in 0 .. NEIGHBORHOOD {
                        for leading_zeros in 0 .. HASH_SIZE {
                            let (id, addr) =
                                if let Some(node) = dir.0.buckets.lock().unwrap()[leading_zeros].get(i) {
                                    (node.node.id.clone(), node.node.address.clone())
                                } else {
                                    continue;
                                };
                            let req_id = dir.0.next_req_id.fetch_add(1, Ordering::Relaxed);
                            match dir.0.ping_states.lock().unwrap().entry(id.clone()) {
                                Entry::Occupied(_) => continue,
                                Entry::Vacant(e) => e.insert(PingState {
                                    req_id: req_id,
                                    leading_zeros: leading_zeros,
                                }),
                            };
                            dir.send(&addr.0, &Protocol::V1(Message::Ping).to_bytes()).await;
                            ping_timeout_write.unbounded_send(NextPingTimeout {
                                end: Instant::now() + REQ_TIMEOUT,
                                key: (id, req_id),
                            }).unwrap();
                        }
                    }
                }
            });
        }
        {
            // Ping timeouts
            let dir = dir.clone();
            tm.stream(ping_timeout_recv, move |e| {
                let dir = dir.clone();
                async move {
                    tokio::time::sleep_until(e.end).await;
                    let state = {
                        let mut borrowed_states = dir.0.ping_states.lock().unwrap();
                        let mut state_entry = match borrowed_states.entry(e.key.0.clone()) {
                            Entry::Occupied(s) => s,
                            Entry::Vacant(_) => return,
                        };
                        let state = state_entry.get_mut();
                        if state.req_id != e.key.1 {
                            // for old request, out of date
                            return;
                        }
                        state_entry.remove()
                    };
                    dir.mark_node_unresponsive(e.key.0, state.leading_zeros, true);
                }
            });
        }
        {
            // Challenge timeouts
            let dir = dir.clone();
            tm.stream(challenge_timeout_recv, move |e| {
                let dir = dir.clone();
                async move {
                    tokio::time::sleep_until(e.end).await;
                    let mut borrowed_states = dir.0.challenge_states.lock().unwrap();
                    let mut state_entry = match borrowed_states.entry(e.key.0.clone()) {
                        Entry::Occupied(s) => s,
                        Entry::Vacant(_) => return,
                    };
                    let state = state_entry.get_mut();
                    if state.req_id != e.key.1 {
                        // for old request, out of date
                        return;
                    }
                    state_entry.remove();
                }
            });
        }
        {
            // Listen loop
            let log = log.fork(ea!(subsys = "listen"));
            let dir = dir.clone();
            let tm1 = tm.clone();
            tm.task(async move {
                let mut buf = [0u8; 1024];
                loop {
                    let packet = match tm1.if_alive(dir.0.as_ref().socket.recv_from(&mut buf)).await {
                        None => return,
                        Some(p) => p,
                    };
                    match packet {
                        Ok((len, addr)) => {
                            match match Protocol::from_bytes(&buf[..len]) {
                                Ok(ver) => match dir.handle(ver, &addr).await {
                                    Ok(()) => Ok(()),
                                    Err(e) => Err(e),
                                },
                                Err(e) => Err(e.context("Failed to bincode deserialize packet", ea!())),
                            } {
                                Ok(()) => { },
                                Err(e) => {
                                    log.warn_e(e, "Received invalid directory message", ea!(addr = addr));
                                },
                            }
                        },
                        Err(e) => {
                            log.warn_e(e.into(), "Error receiving packet", ea!());
                        },
                    };
                }
            });
        }
        dir.start_find(FindMode::Nodes(dir.0.own_id.clone()), None, None).await;
        return Ok(dir);
    }

    pub async fn get(&self, key: Identity) -> Option<ValueBody> {
        let (f, c) = ManualFuture::new();
        self.start_find(FindMode::Get(key), None, Some(c)).await;
        return f.await.map(|v| v.parse().unwrap());
    }

    pub async fn put(&self, key: Identity, value: Value) {
        self.start_find(FindMode::Put(key), Some(value), None).await;
    }

    fn mark_node_unresponsive(&self, key: NodeIdentity, leading_zeros: usize, unresponsive: bool) {
        let mut buckets = self.0.buckets.lock().unwrap();
        let bucket = &mut buckets[leading_zeros];
        for n in bucket {
            if n.node.id == key {
                n.unresponsive = unresponsive;
                return;
            }
        }
        self.0.dirty.store(true, Ordering::Relaxed);
    }

    async fn start_challenge(&self, id: NodeIdentity, addr: &SocketAddr) {
        // store state by key, with futures
        let timeout = Instant::now() + REQ_TIMEOUT;
        let (challenge, req_id) = {
            let mut borrowed_states = self.0.challenge_states.lock().unwrap();
            let (challenge, state) = match borrowed_states.entry(id.clone()) {
                Entry::Occupied(_) => {
                    return;
                },
                Entry::Vacant(e) => {
                    let challenge = generate_challenge();
                    (challenge.clone(), e.insert(ChallengeState {
                        challenge: challenge,
                        req_id: self.0.next_req_id.fetch_add(1, Ordering::Relaxed),
                        node: NodeInfo {
                            id: id.clone(),
                            address: Addr(addr.clone()),
                        },
                    }))
                },
            };
            (challenge, state.req_id)
        };
        self.send(addr, &Protocol::V1(Message::Challenge(challenge)).to_bytes()).await;
        self.0.challenge_timeouts.unbounded_send(NextChallengeTimeout {
            end: timeout,
            key: (id, req_id),
        }).unwrap();
    }

    async fn start_find(
        &self,
        mode: FindMode,
        store: Option<Value>,
        fut: Option<ManualFutureCompleter<Option<Value>>>,
    ) {
        // store state by key, with futures
        let timeout = Instant::now() + REQ_TIMEOUT;
        let mut defer = vec![];
        let req_id = {
            let key_hash = match &mode {
                FindMode::Nodes(k) => hash(k),
                FindMode::Put(k) => hash(k),
                FindMode::Get(k) => hash(k),
            };
            let mut borrowed_states = self.0.find_states.lock().unwrap();
            let state = match borrowed_states.entry(mode.clone()) {
                Entry::Occupied(mut e) => {
                    if let Some(f) = fut {
                        e.get_mut().futures.push(f);
                    }
                    return;
                },
                Entry::Vacant(e) => e.insert(FindState {
                    req_id: self.0.next_req_id.fetch_add(1, Ordering::Relaxed),
                    mode: mode.clone(),
                    target_hash: key_hash,
                    timeout: timeout.clone(),
                    best: vec![BestNodeEntry {
                        dist: diff(&key_hash, &self.0.own_id_hash).1,
                        node: BestNodeEntryNode::Self_,
                    }],
                    outstanding: vec![],
                    value: store,
                    futures: vec![],
                }),
            };
            if let Some(f) = fut {
                state.futures.push(f);
            }
            let closest_peers = self.get_closest_peers(key_hash, PARALLEL);
            for p in closest_peers {
                eprintln!("DEBUG closest peer for find {:?}: {}", mode, p.id);
                let challenge = generate_challenge();
                let (leading_zeros, dist) = diff(&hash(&p.id), &state.target_hash);
                state.outstanding.push(OutstandingNodeEntry {
                    dist: dist,
                    leading_zeros: leading_zeros,
                    challenge: challenge.clone(),
                    node: p.clone(),
                });

                struct Defer {
                    challenge: Box<[u8]>,
                    addr: SocketAddr,
                }

                defer.push(Defer {
                    challenge: challenge,
                    addr: p.address.0.clone(),
                });
            }
            state.req_id
        };
        for d in defer {
            self.send(&d.addr, &Protocol::V1(Message::FindRequest(FindRequest {
                challenge: d.challenge,
                mode: mode.clone(),
                sender: self.0.own_id.clone(),
            })).to_bytes()).await;
        }
        self.0.find_timeouts.unbounded_send(NextFindTimeout {
            end: timeout,
            key: (mode, req_id),
        }).unwrap();
    }

    async fn complete_state(&self, state: FindState) {
        match state.mode {
            FindMode::Nodes(_) => {
                // Do nothing, this is just internal initial route population
            },
            FindMode::Put(k) => {
                let v = state.value.unwrap();
                for best in state.best {
                    match best.node {
                        BestNodeEntryNode::Self_ => {
                            self.store(k.clone(), v.clone());
                        },
                        BestNodeEntryNode::Node(node) => {
                            self.send(&node.address.0, &Protocol::V1(Message::Store(StoreRequest {
                                key: k.clone(),
                                value: v.clone(),
                            })).to_bytes()).await;
                        },
                    }
                }
            },
            FindMode::Get(_) => {
                for f in state.futures {
                    f.complete(state.value.clone()).await;
                }
            },
        }
    }

    async fn handle_challenge_resp(&self, resp: ChallengeResponse) {
        let log = self.0.log.fork(ea!(action = "challenge_response", node = resp.sender.dbg_str()));

        // Lookup request state
        let mut borrowed_states = self.0.challenge_states.lock().unwrap();
        let state_entry = match borrowed_states.entry(resp.sender.clone()) {
            Entry::Occupied(s) => s,
            Entry::Vacant(_) => {
                log.warn("No request state matching response target", ea!());
                return;
            },
        };
        let state = state_entry.get();

        // Confirm sender is legit routable, add to own routing table
        match verify_challenge(&resp.sender, TempChallengeSigBody {
            challenge: &state.challenge,
            body: &resp.sender,
        }, resp.signature) {
            Ok(()) => { },
            Err(e) => {
                log.warn_e(e, "Bad sender signature", ea!());
                return;
            },
        };
        let state = state_entry.remove();
        self.add_good_node(resp.sender.clone(), Some(state.node));
    }

    async fn handle_find_resp(&self, resp: FindResponse) {
        let log = self.0.log.fork(ea!(action = "find_response", node = &resp.body.mode.dbg_str()));
        let mut defer_next_req = vec![];
        let mut transfer_stored_addr: Option<SocketAddr> = None;
        let state = {
            // Lookup request state
            let mut borrowed_states = self.0.find_states.lock().unwrap();
            let mut state_entry = match borrowed_states.entry(resp.body.mode.clone()) {
                Entry::Occupied(s) => s,
                Entry::Vacant(_) => {
                    log.warn("No request state matching response target", ea!());
                    return;
                },
            };
            let mut state = state_entry.get_mut();
            let mut outstanding_entry: Option<OutstandingNodeEntry> = None;
            state.outstanding.retain(|e| {
                if e.node.id == resp.body.sender {
                    outstanding_entry = Some(e.clone());
                    return false;
                }
                return true;
            });
            let outstanding_entry = match outstanding_entry {
                Some(e) => e,
                None => {
                    log.warn("No outstanding request in state for sender", ea!(sender = resp.body.sender.dbg_str()));
                    return;
                },
            };

            // Confirm sender is legit routable, possibly add to own routing table
            match verify_challenge(&resp.body.sender, TempChallengeSigBody {
                challenge: &outstanding_entry.challenge,
                body: &resp.body,
            }, resp.sig) {
                Ok(()) => { },
                Err(e) => {
                    log.warn_e(e, "Bad sender signature", ea!());
                    return;
                },
            };
            let (_, sender_dist) = diff(&hash(&outstanding_entry.node.id), &self.0.own_id_hash);
            if self.add_good_node(outstanding_entry.node.id.clone(), Some(outstanding_entry.node.clone())) {
                if !self
                    .get_closest_peers(self.0.own_id_hash, NEIGHBORHOOD)
                    .iter()
                    .any(|p| diff(&hash(&p.id), &self.0.own_id_hash).1 < sender_dist) {
                    transfer_stored_addr = Some(outstanding_entry.node.address.0.clone());
                }
            }

            // Add node that responded to best list, if there's space or it's higher priority
            // and it's not already in there 1
            loop {
                let mut replace_best = false;
                if state.best.len() == NEIGHBORHOOD {
                    if state.best.last().unwrap().dist > sender_dist {
                        break;
                    }
                    replace_best = true;
                }
                if state.best.iter().any(|e| match &e.node {
                    BestNodeEntryNode::Self_ => self.0.own_id == outstanding_entry.node.id,
                    BestNodeEntryNode::Node(f) => f.id == outstanding_entry.node.id,
                }) {
                    break;
                }
                if replace_best {
                    state.best.pop();
                }
                state.best.push(BestNodeEntry {
                    dist: sender_dist,
                    node: BestNodeEntryNode::Node(outstanding_entry.node.clone()),
                });
                state.best.sort_by_key(|e| e.dist);
                break;
            }

            // Gather data for response processing (response deferred due to borrow checker
            // issue with mutex guards)
            match resp.body.inner {
                FindResponseModeBody::Nodes(nodes) => {
                    for n in nodes {
                        // Fan out to new nodes
                        let candidate_hash = hash(&n.id);
                        let (leading_zeros, dist) = diff(&candidate_hash, &state.target_hash);

                        // If best list is full and this node is farther away than any current nodes, skip
                        // it
                        if state.best.len() == NEIGHBORHOOD && dist > state.best.last().unwrap().dist {
                            continue;
                        }

                        // If outstanding list is full and this node is farther away than any current
                        // nodes, skip it
                        let mut replace_outstanding = false;
                        if state.outstanding.len() == PARALLEL {
                            if state.outstanding.last().unwrap().dist < dist {
                                continue;
                            }
                            replace_outstanding = true;
                        }

                        // If this node already in best, skip it
                        if state.best.iter().any(|e| match &e.node {
                            BestNodeEntryNode::Self_ => self.0.own_id == n.id,
                            BestNodeEntryNode::Node(f) => f.id == n.id,
                        }) {
                            continue;
                        }

                        // If this node already in outstanding, skip it
                        if state.outstanding.iter().any(|e| e.node.id == n.id) {
                            continue;
                        }
                        if replace_outstanding {
                            state.outstanding.pop();
                        }
                        let challenge = generate_challenge();
                        state.outstanding.push(OutstandingNodeEntry {
                            dist: dist,
                            challenge: challenge.clone(),
                            node: n.clone(),
                            leading_zeros: leading_zeros,
                        });
                        state.outstanding.sort_by_key(|e| e.dist);

                        struct Defer {
                            challenge: Box<[u8]>,
                            addr: SocketAddr,
                        }

                        defer_next_req.push(Defer {
                            challenge: challenge,
                            addr: n.address.0.clone(),
                        });
                    }
                },
                FindResponseModeBody::Value(value) => {
                    match &resp.body.mode {
                        // bad response
                        FindMode::Nodes(_) => { },
                        // bad response
                        FindMode::Put(_) => { },
                        FindMode::Get(k) => 
                        // 1
                        loop {
                            if !k.verify(&value.message, &value.signature) {
                                log.warn("Got value with bad signature", ea!());
                                break;
                            }
                            let signed = match value.parse() {
                                Ok(s) => s,
                                Err(e) => {
                                    log.warn_e(e, "Failed to parse body from signature", ea!());
                                    break;
                                },
                            };
                            if signed.expires > Utc::now() {
                                log.warn("Got expired value", ea!(expires = signed.expires.to_rfc3339()));
                                break;
                            }
                            state.value = Some(value);
                            break;
                        },
                    }
                },
            }

            // If done cleanup or else update timeouts
            if state.outstanding.is_empty() {
                Some(state_entry.remove())
            } else {
                state.timeout = Instant::now() + REQ_TIMEOUT;
                self.0.find_timeouts.unbounded_send(NextFindTimeout {
                    end: state.timeout,
                    key: (state.mode.clone(), state.req_id),
                }).unwrap();
                None
            }
        };

        // Send deferred messages now that locks are released
        if let Some(addr) = transfer_stored_addr {
            let mut store = HashMap::new();
            {
                let lock = self.0.store.lock().unwrap();
                store.extend(lock.iter().map(|(k, v)| (k.clone(), v.value.clone())));
            }
            for (k, v) in store.into_iter() {
                self.send(&addr, &Protocol::V1(Message::Store(StoreRequest {
                    key: k,
                    value: v,
                })).to_bytes()).await;
            }
        }
        if let Some(s) = state {
            self.complete_state(s).await;
        }
        for d in defer_next_req {
            self.send(&d.addr, &Protocol::V1(Message::FindRequest(FindRequest {
                challenge: d.challenge,
                mode: resp.body.mode.clone(),
                sender: self.0.own_id.clone(),
            })).to_bytes()).await;
        }
    }

    fn get_closest_peers(&self, target_hash: DhtHash, count: usize) -> Vec<NodeInfo> {
        let buckets = self.0.buckets.lock().unwrap();
        let (leading_zeros, _) = diff(&target_hash, &self.0.own_id_hash);
        let mut nodes: Vec<NodeInfo> = vec![];
        'outer1: for bucket in leading_zeros .. HASH_SIZE {
            for state in &buckets[bucket] {
                if nodes.len() >= count {
                    break 'outer1;
                }
                nodes.push(state.node.clone());
            }
        }
        if leading_zeros > 0 {
            'outer: for bucket in (0 .. leading_zeros - 1).rev() {
                for state in &buckets[bucket] {
                    if nodes.len() >= count {
                        break 'outer;
                    }
                    nodes.push(state.node.clone());
                }
            }
        }
        return nodes;
    }

    fn store(&self, k: Identity, v: Value) {
        self.0.log.debug("Storing", ea!(value = k.dbg_str()));
        self.0.store.lock().unwrap().insert(k, ValueState {
            value: v,
            updated: Instant::now(),
        });
    }

    async fn handle(&self, m: Protocol, reply_to: &SocketAddr) -> Result<(), loga::Error> {
        let log = self.0.log.fork(ea!(addr = reply_to, message = m.dbg_str()));
        log.debug("Received", ea!());
        match m {
            Protocol::V1(v1) => match v1 {
                Message::FindRequest(m) => {
                    let body = FindResponseBody {
                        mode: m.mode.clone(),
                        sender: self.0.own_id.clone(),
                        inner: match &m.mode {
                            FindMode::Nodes(k) => FindResponseModeBody::Nodes(
                                self.get_closest_peers(hash(k), NEIGHBORHOOD),
                            ),
                            FindMode::Put(k) => FindResponseModeBody::Nodes(
                                self.get_closest_peers(hash(k), NEIGHBORHOOD),
                            ),
                            FindMode::Get(k) => match self.0.store.lock().unwrap().entry(k.clone()) {
                                Entry::Occupied(v) => {
                                    FindResponseModeBody::Value(v.get().value.clone())
                                },
                                Entry::Vacant(_) => FindResponseModeBody::Nodes(
                                    self.get_closest_peers(hash(k), NEIGHBORHOOD),
                                ),
                            },
                        },
                    };
                    let sig = sign_challenge(&self.0.own_secret, TempChallengeSigBody {
                        challenge: &m.challenge,
                        body: &body,
                    });
                    self.send(reply_to, &Protocol::V1(Message::FindResponse(FindResponse {
                        body: body,
                        sig: sig,
                    })).to_bytes()).await;
                    if self.add_good_node(m.sender.clone(), None) {
                        self.start_challenge(m.sender, reply_to).await;
                    }
                },
                Message::FindResponse(m) => {
                    self.handle_find_resp(m).await;
                },
                Message::Store(m) => {
                    if !m.key.verify(&m.value.message, &m.value.signature) {
                        return Err(self.0.log.new_err("Store request failed signature validation", ea!()))
                    };
                    self.store(m.key, m.value);
                },
                Message::Ping => {
                    self.send(reply_to, &Protocol::V1(Message::Pung(self.0.own_id.clone())).to_bytes()).await;
                },
                Message::Pung(k) => {
                    let state = match self.0.ping_states.lock().unwrap().entry(k.clone()) {
                        Entry::Occupied(s) => s.remove(),
                        Entry::Vacant(_) => return Ok(()),
                    };
                    self.mark_node_unresponsive(k, state.leading_zeros, false);
                },
                Message::Challenge(challenge) => {
                    self.send(reply_to, &Protocol::V1(Message::ChallengeResponse(ChallengeResponse {
                        sender: self.0.own_id.clone(),
                        signature: sign_challenge(&self.0.own_secret, TempChallengeSigBody {
                            challenge: &challenge,
                            body: &self.0.own_id,
                        }),
                    })).to_bytes()).await;
                },
                Message::ChallengeResponse(resp) => {
                    self.handle_challenge_resp(resp).await;
                },
            },
        };
        Ok(())
    }

    pub fn add_good_node(&self, id: NodeIdentity, node: Option<NodeInfo>) -> bool {
        let log = self.0.log.fork(ea!(activity = "add_good_node", node = id.dbg_str()));
        if id == self.0.own_id {
            log.info("Own node id, ignoring", ea!());
            return false;
        }
        let (leading_zeros, _) = diff(&hash(&id), &self.0.own_id_hash);
        let buckets = &mut self.0.buckets.lock().unwrap();
        let new_node = 'logic : loop {
            let bucket = &mut buckets[leading_zeros];
            let mut last_unresponsive: Option<usize> = None;

            // Updated or already known
            for i in 0 .. bucket.len() {
                let n = &mut bucket[i];
                if n.node.id == id {
                    if let Some(node) = node {
                        *n = NodeState {
                            node: node.clone(),
                            unresponsive: false,
                        };
                        log.info("Updated existing node", ea!());
                    }
                    break 'logic false;
                }
                if n.unresponsive {
                    last_unresponsive = Some(i);
                }
            }

            // Empty slot
            if bucket.len() < NEIGHBORHOOD {
                if let Some(node) = node {
                    bucket.insert(0, NodeState {
                        node: node.clone(),
                        unresponsive: false,
                    });
                    log.info("Added node to empty slot", ea!());
                }
                break true;
            }

            // Replacing dead
            if let Some(i) = last_unresponsive {
                if let Some(node) = node {
                    bucket.remove(i);
                    bucket.push(NodeState {
                        node: node.clone(),
                        unresponsive: false,
                    });
                    log.info("Replaced dead node", ea!());
                }
                break 'logic true;
            }
            log.info("Nowhere to place, dropping", ea!());
            break false;
        };
        return new_node;
    }

    async fn send(&self, addr: &SocketAddr, data: &[u8]) {
        self.0.log.debug("Sending", ea!(addr = addr, message = data.dbg_str()));
        self.0.socket.send_to(data, addr).await.unwrap();
    }
}
