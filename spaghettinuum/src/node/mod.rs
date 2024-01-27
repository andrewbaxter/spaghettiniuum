use crate::interface::stored::shared::SerialAddr;
use crate::interface::wire::node::latest::FindGoal;
use crate::interface::wire::node::v1::DhtCoord;
use crate::{
    bb,
    cap_fn,
};
use crate::interface::config::shared::StrSocketAddr;
use crate::interface::stored::identity::Identity;
use crate::interface::stored::node_identity::{
    self,
    NodeIdentityMethods,
    NodeSecretMethods,
    NodeIdentity,
};
use crate::interface::{
    stored,
    wire,
};
use crate::utils::signed::{
    IdentSignatureMethods,
    NodeIdentSignatureMethods,
};
use crate::utils::blob::{
    Blob,
};
use crate::utils::log::{
    Log,
    INFO,
    WARN,
    DEBUG_NODE,
};
use crate::utils::time_util::ToInstant;
use constant_time_eq::constant_time_eq;
use tokio::select;
use tokio::time::sleep;
use crate::utils::db_util::setup_db;
use chrono::{
    Utc,
    DateTime,
    Duration,
};
use futures::channel::mpsc::unbounded;
use futures::channel::mpsc::UnboundedSender;
use generic_array::ArrayLength;
use generic_array::GenericArray;
use loga::{
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
use std::collections::{
    HashMap,
    HashSet,
};
use std::fmt::Debug;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{
    AtomicUsize,
    AtomicBool,
};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::net::UdpSocket;

pub mod db;

const HASH_SIZE: usize = 256usize;
const NEIGHBORHOOD: usize = 8usize;
const PARALLEL: usize = 3;

fn req_timeout() -> Duration {
    return Duration::seconds(5);
}

// Republish stored values once an hour
fn store_fresh_duration() -> Duration {
    return Duration::hours(1);
}

// All stored values expire after 24h
fn expiry() -> Duration {
    return Duration::hours(24);
}

fn dist_<N: ArrayLength<u8>>(a: &GenericArray<u8, N>, b: &GenericArray<u8, N>) -> (usize, GenericArray<u8, N>) {
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

fn dist(a: &DhtCoord, b: &DhtCoord) -> (usize, DhtCoord) {
    let (leading_zeros, out) = dist_(&a.0, &b.0);
    return (leading_zeros, DhtCoord(out));
}

#[cfg(test)]
mod dist_tests {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;

    type SmallBytes = GenericArray<u8, generic_array::typenum::U2>;

    #[test]
    fn test_same() {
        let (lz, d) = dist_(SmallBytes::from_slice(&[0u8, 0u8]), SmallBytes::from_slice(&[0u8, 0u8]));
        assert_eq!(lz, 16);
        assert_eq!(d.as_slice(), &[0u8, 0u8]);
    }

    #[test]
    fn test_lsb_dist() {
        let (lz, d) = dist_(SmallBytes::from_slice(&[0u8, 1u8]), SmallBytes::from_slice(&[0u8, 0u8]));
        assert_eq!(lz, 15);
        assert_eq!(d.as_slice(), &[0u8, 1u8]);
    }

    #[test]
    fn test_msb_dist() {
        let (lz, d) = dist_(SmallBytes::from_slice(&[128u8, 0u8]), SmallBytes::from_slice(&[0u8, 0u8]));
        assert_eq!(lz, 0);
        assert_eq!(d.as_slice(), &[128u8, 0u8]);
    }
}

fn node_ident_coord(x: &NodeIdentity) -> DhtCoord {
    return DhtCoord(<sha2::Sha256 as Digest>::digest(x.to_bytes()));
}

fn ident_coord(x: &Identity) -> DhtCoord {
    return DhtCoord(<sha2::Sha256 as Digest>::digest(x.to_bytes()));
}

#[derive(Debug)]
struct NextFindTimeout {
    updated: DateTime<Utc>,
    key: (FindGoal, usize),
}

#[derive(Clone)]
struct ValueState {
    value: stored::announcement::Announcement,
    updated: DateTime<Utc>,
}

struct NextPingTimeout {
    end: DateTime<Utc>,
    key: (node_identity::NodeIdentity, usize),
}

struct NextChallengeTimeout {
    end: DateTime<Utc>,
    key: (node_identity::NodeIdentity, usize),
}

struct Buckets {
    buckets: [Vec<wire::node::latest::NodeState>; HASH_SIZE],
    addrs: HashMap<SocketAddr, NodeIdentity>,
}

struct NodeInner {
    log: Log,
    own_ident: node_identity::NodeIdentity,
    own_coord: DhtCoord,
    own_secret: node_identity::NodeSecret,
    buckets: Mutex<Buckets>,
    store: Mutex<HashMap<Identity, ValueState>>,
    dirty: AtomicBool,
    socket: UdpSocket,
    next_req_id: AtomicUsize,
    find_timeouts: UnboundedSender<NextFindTimeout>,
    find_states: Mutex<HashMap<FindGoal, FindState>>,
    ping_states: Mutex<HashMap<node_identity::NodeIdentity, PingState>>,
    challenge_timeouts: UnboundedSender<NextChallengeTimeout>,
    challenge_states: Mutex<HashMap<node_identity::NodeIdentity, ChallengeState>>,
}

#[derive(Clone)]
pub struct Node(Arc<NodeInner>);

#[derive(Clone, Debug)]
struct OutstandingNodeEntry {
    dist: DhtCoord,
    leading_zeros: usize,
    challenge: Blob,
    node: wire::node::latest::NodeInfo,
}

#[derive(Clone)]
enum NearestNodeEntryNode {
    Self_,
    Node(wire::node::latest::NodeInfo),
}

#[derive(Clone)]
struct NearestNodeEntry {
    dist: DhtCoord,
    node: NearestNodeEntryNode,
}

struct FindState {
    req_id: usize,
    goal: FindGoal,
    updated: DateTime<Utc>,
    nearest: Vec<NearestNodeEntry>,
    outstanding: Vec<OutstandingNodeEntry>,
    // TODO rename -> seen
    requested: HashSet<node_identity::NodeIdentity>,
    // for storing value, or retrieving value
    value: Option<stored::announcement::Announcement>,
    futures: Vec<ManualFutureCompleter<FindResult>>,
}

struct FindResult {
    nearest: Vec<NearestNodeEntry>,
    value: Option<stored::announcement::Announcement>,
}

struct PingState {
    req_id: usize,
    leading_zeros: usize,
}

struct ChallengeState {
    req_id: usize,
    challenge: Blob,
    node: wire::node::latest::NodeInfo,
}

fn generate_challenge() -> Blob {
    let mut out = Blob::new(32);
    rand::thread_rng().fill_bytes(out.as_mut());
    return out;
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
struct Persisted {
    own_secret: node_identity::NodeSecret,
    initial_buckets: Vec<Vec<wire::node::latest::NodeState>>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HealthDetail {
    pub responsive_neighbors: usize,
    pub unresponsive_neighbors: usize,
    pub active_finds: usize,
    pub active_challenges: usize,
    pub active_pings: usize,
}

impl Node {
    /// Creates and starts a new node within the task manager. Waits until the socket
    /// is open. Bootstrapping is asynchronous; you should wait until a sufficient
    /// number of peers are found before doing anything automatically.
    ///
    /// * `bootstrap`: Nodes to connect to to join network. Ignored if restoring persisted
    ///   data. Ignores own id if present.
    ///
    /// * `persist_path`: Save state to this file before shutting down to make next startup
    ///   faster
    pub async fn new(
        log: &Log,
        tm: TaskManager,
        bind_addr: StrSocketAddr,
        bootstrap: &[wire::node::latest::NodeInfo],
        persistent_path: &Path,
    ) -> Result<Node, loga::Error> {
        let log = &log.fork(ea!(sys = "node"));
        let mut do_bootstrap = false;
        let own_ident;
        let own_secret;
        let mut initial_buckets = Buckets {
            buckets: array_init::array_init(|_| vec![]),
            addrs: HashMap::new(),
        };
        let db_pool =
            setup_db(&persistent_path.join("node.sqlite3"), db::migrate)
                .await
                .stack_context(log, "Error initializing database")?;
        let db = db_pool.get().await.stack_context(log, "Error getting database connection")?;
        match db
            .interact(|conn| db::secret_get(&conn))
            .await
            .stack_context(log, "Error interacting with database")?
            .stack_context(log, "Error retrieving secret")? {
            Some(s) => {
                own_ident = s.get_identity();
                own_secret = s;
            },
            None => {
                (own_ident, own_secret) = node_identity::NodeIdentity::new();
            },
        }
        let own_coord = node_ident_coord(&own_ident);
        {
            let mut no_neighbors = true;
            for e in db
                .interact(|conn| db::neighbors_get(&conn))
                .await
                .stack_context(log, "Error interacting with database")?
                .stack_context(log, "Error retrieving old neighbors")? {
                let state = match e {
                    wire::node::NodeState::V1(s) => s,
                };
                let (leading_zeros, _) = dist(&node_ident_coord(&state.node.ident), &own_coord);
                match initial_buckets.addrs.entry(state.node.address.0) {
                    Entry::Occupied(v) => {
                        log.log_with(
                            WARN,
                            "Duplicate neighbor address in database, skipping",
                            ea!(addr = state.node.address, ident1 = state.node.ident, ident2 = v.get()),
                        );
                        continue;
                    },
                    Entry::Vacant(v) => {
                        v.insert(state.node.ident);
                    },
                }
                log.log_with(
                    DEBUG_NODE,
                    "Restoring neighbor",
                    ea!(ident = state.node.ident, addr = state.node.address),
                );
                initial_buckets.buckets[leading_zeros].push(state);
                no_neighbors = false;
            }
            if no_neighbors {
                do_bootstrap = true;
            }
        }
        log.log_with(INFO, "Starting", ea!(own_node_ident = own_ident));
        let sock = {
            let log = log.fork(ea!(addr = bind_addr));
            UdpSocket::bind(bind_addr.resolve()?).await.stack_context(&log, "Failed to open node UDP port")?
        };
        let (find_timeout_write, find_timeout_recv) = unbounded::<NextFindTimeout>();
        let (ping_timeout_write, ping_timeout_recv) = unbounded::<NextPingTimeout>();
        let (challenge_timeout_write, challenge_timeout_recv) = unbounded::<NextChallengeTimeout>();
        let dir = Node(Arc::new(NodeInner {
            log: log.clone(),
            own_ident: node_identity::NodeIdentity::V1(match own_ident {
                node_identity::NodeIdentity::V1(i) => i,
            }),
            own_secret: node_identity::NodeSecret::V1(match own_secret {
                node_identity::NodeSecret::V1(s) => s,
            }),
            own_coord: own_coord,
            buckets: Mutex::new(initial_buckets),
            dirty: AtomicBool::new(do_bootstrap),
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
            log.log_with(DEBUG_NODE, "No neighbors, bootstrapping", ea!(count = bootstrap.len()));
            for b in bootstrap {
                if b.ident == dir.0.own_ident {
                    continue;
                }
                if !dir.add_good_node(b.ident.clone(), Some(b.clone())) {
                    panic!("");
                }
            }
        }

        // Periodically save
        tm.periodic("Node - persist state", Duration::minutes(10).to_std().unwrap(), cap_fn!(()(log, dir, db_pool) {
            if !dir.0.dirty.swap(false, Ordering::Relaxed) {
                return;
            }
            let db_pool = db_pool.clone();
            match async {
                db_pool.get().await.context("Error getting db connection")?.interact(move |conn| {
                    db::secret_ensure(conn, &dir.0.own_secret)?;
                    db::neighbors_clear(conn)?;
                    for bucket in dir.0.buckets.lock().unwrap().buckets.clone().into_iter() {
                        for n in bucket {
                            db::neighbors_insert(conn, &wire::node::NodeState::V1(n))?;
                        }
                    }
                    return Ok(()) as Result<_, loga::Error>;
                }).await??;
                return Ok(()) as Result<_, loga::Error>;
            }.await {
                Ok(_) => { },
                Err(e) => log.log_err(WARN, e.context("Failed to persist state")),
            }
        }));

        // Find timeouts
        tm.stream("Node - finish timed requests", find_timeout_recv, cap_fn!((e)(dir) {
            let deadline = e.updated + req_timeout();
            tokio::time::sleep_until(deadline.to_instant()).await;
            let state = {
                let mut borrowed_states = dir.0.find_states.lock().unwrap();
                let mut state_entry = match borrowed_states.entry(e.key.0) {
                    Entry::Occupied(s) => s,
                    Entry::Vacant(_) => return,
                };
                let state = state_entry.get_mut();
                if state.req_id != e.key.1 {
                    // for old request, out of date
                    return;
                }
                if state.updated + req_timeout() > Utc::now() {
                    // time pushed back while this timeout was in the queue
                    return;
                }
                dir.0.log.log_with(DEBUG_NODE, "Find timed out", ea!(key = &e.key.0.dbg_str()));
                state_entry.remove()
            };
            for o in &state.outstanding {
                dir.mark_node_unresponsive(o.node.ident, o.leading_zeros, true);
            }
            dir.complete_state(state).await;
        }));

        // Stored data expiry or maybe re-propagation
        tm.periodic("Node - re-propagate/expire stored data", Duration::hours(1).to_std().unwrap(), cap_fn!(()(dir) {
            let mut unfresh = vec![];
            let now = Utc::now();
            dir.0.store.lock().unwrap().retain(|k, v| {
                match &v.value {
                    stored::announcement::Announcement::V1(value) => {
                        if value.parse_unwrap().published + expiry() < now {
                            return false;
                        }
                    },
                }
                if v.updated + store_fresh_duration() < now {
                    v.updated = now;
                    unfresh.push((k.clone(), v.value.clone()));
                }
                return true;
            });
            for (k, v) in unfresh {
                dir.put(k, v).await;
            }
        }));

        // Pings
        tm.periodic("Node - neighbor aliveness", Duration::minutes(10).to_std().unwrap(), cap_fn!(()(dir, ping_timeout_write) {
            for i in 0 .. NEIGHBORHOOD {
                for leading_zeros in 0 .. HASH_SIZE {
                    let (id, addr) =
                        if let Some(node) = dir.0.buckets.lock().unwrap().buckets[leading_zeros].get(i) {
                            (node.node.ident.clone(), node.node.address.clone())
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
                    dir.send(&addr.0, wire::node::Protocol::V1(wire::node::latest::Message::Ping)).await;
                    ping_timeout_write.unbounded_send(NextPingTimeout {
                        end: Utc::now() + req_timeout(),
                        key: (id, req_id),
                    }).unwrap();
                }
            }
        }));

        // Ping timeouts
        tm.stream("Node - ping timeouts", ping_timeout_recv, cap_fn!((e)(dir) {
            tokio::time::sleep_until(e.end.to_instant()).await;
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
        }));

        // Challenge timeouts
        tm.stream("Node - challenge timeouts", challenge_timeout_recv, cap_fn!((e)(dir) {
            tokio::time::sleep_until(e.end.to_instant()).await;
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
        }));

        // Listen loop
        tm.task("Node - socket", {
            let log = log.fork(ea!(subsys = "listen"));
            let dir = dir.clone();
            let tm = tm.clone();
            async move {
                let mut buf = [0u8; 1024];
                loop {
                    let packet = select!{
                        _ = tm.until_terminate() => {
                            return;
                        }
                        p = dir.0.as_ref().socket.recv_from(&mut buf) => p,
                    };
                    match packet {
                        Ok((len, addr)) => {
                            match match wire::node::Protocol::from_bytes(&buf[..len]) {
                                Ok(ver) => match dir.handle(ver, &addr).await {
                                    Ok(()) => Ok(()),
                                    Err(e) => Err(e),
                                },
                                Err(e) => Err(e.context("Failed to bincode deserialize packet")),
                            } {
                                Ok(()) => { },
                                Err(e) => {
                                    log.log_err(
                                        DEBUG_NODE,
                                        e.context_with("Received invalid directory message", ea!(addr = addr)),
                                    );
                                },
                            }
                        },
                        Err(e) => {
                            log.log_err(WARN, e.context("Error receiving packet"));
                        },
                    };
                }
            }
        });
        dir.start_find(FindGoal::Coord(node_ident_coord(&dir.0.own_ident)), None).await;

        // If running in a container or at boot, packets may be lost immediately after
        // getting an ip address so do it again in a minute.
        tm.task("Node - retry startup find once", {
            let dir = dir.clone();
            async move {
                sleep(Duration::seconds(60).to_std().unwrap()).await;
                dir.start_find(FindGoal::Coord(node_ident_coord(&dir.0.own_ident)), None).await;
            }
        });
        return Ok(dir);
    }

    pub fn health_detail(&self) -> HealthDetail {
        let mut responsive = 0;
        let mut unresponsive = 0;
        for bucket in self.0.buckets.lock().unwrap().buckets.clone().into_iter() {
            for n in bucket {
                if n.unresponsive {
                    unresponsive += 1;
                } else {
                    responsive += 1;
                }
            }
        }
        return HealthDetail {
            responsive_neighbors: responsive,
            unresponsive_neighbors: unresponsive,
            active_challenges: self.0.challenge_states.lock().unwrap().len(),
            active_finds: self.0.find_states.lock().unwrap().len(),
            active_pings: self.0.ping_states.lock().unwrap().len(),
        };
    }

    /// Identity of node
    pub fn node_identity(&self) -> node_identity::NodeIdentity {
        return self.0.own_ident.clone();
    }

    /// Look up a value in the network
    pub async fn get(&self, key: Identity) -> Option<stored::announcement::Announcement> {
        let (f, c) = ManualFuture::new();
        self.start_find(FindGoal::Identity(key), Some(c)).await;
        return f.await.value;
    }

    /// Store a value in the network. `value` message must be `ValueBody::to_bytes()`
    /// and `signature` is the signature of those bytes using the corresponding
    /// `IdentitySecret`
    pub async fn put(
        &self,
        key: Identity,
        value: stored::announcement::Announcement,
    ) -> Option<stored::announcement::Announcement> {
        let (f, c) = ManualFuture::new();
        self.start_find(FindGoal::Identity(key), Some(c)).await;
        let res = f.await;

        bb!{
            'skip_store _;
            match &res.value {
                Some(accepted) => match accepted {
                    stored::announcement::Announcement::V1(accepted) => {
                        let new_published = match &value {
                            stored::announcement::Announcement::V1(a) => {
                                a.parse_unwrap().published
                            },
                        };
                        if accepted.parse_unwrap().published >= new_published {
                            break 'skip_store;
                        }
                    },
                },
                _ => (),
            }
            for nearest in res.nearest {
                match nearest.node {
                    NearestNodeEntryNode::Self_ => {
                        self.0.log.log_with(DEBUG_NODE, "Storing", ea!(value = key.dbg_str()));
                        self.0.store.lock().unwrap().insert(key.clone(), ValueState {
                            value: value.clone(),
                            updated: Utc::now(),
                        });
                    },
                    NearestNodeEntryNode::Node(node) => {
                        self
                            .send(
                                &node.address.0,
                                wire::node::Protocol::V1(
                                    wire::node::latest::Message::Store(wire::node::latest::StoreRequest {
                                        key: key.clone(),
                                        value: value.clone(),
                                    }),
                                ),
                            )
                            .await;
                    },
                }
            }
        };

        return res.value;
    }

    fn mark_node_unresponsive(&self, key: node_identity::NodeIdentity, leading_zeros: usize, unresponsive: bool) {
        let mut buckets = self.0.buckets.lock().unwrap();
        let bucket = &mut buckets.buckets[leading_zeros];
        for n in bucket {
            if n.node.ident == key {
                n.unresponsive = unresponsive;
                return;
            }
        }
        self.0.dirty.store(true, Ordering::Relaxed);
    }

    async fn start_challenge(&self, id: node_identity::NodeIdentity, addr: &SocketAddr) {
        // store state by key, with futures
        let timeout = Utc::now() + req_timeout();
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
                        node: wire::node::latest::NodeInfo {
                            ident: id.clone(),
                            address: SerialAddr(addr.clone()),
                        },
                    }))
                },
            };
            (challenge, state.req_id)
        };
        self.send(addr, wire::node::Protocol::V1(wire::node::latest::Message::Challenge(challenge))).await;
        self.0.challenge_timeouts.unbounded_send(NextChallengeTimeout {
            end: timeout,
            key: (id, req_id),
        }).unwrap();
    }

    async fn start_find(&self, goal: FindGoal, fut: Option<ManualFutureCompleter<FindResult>>) {
        let goal_coord = match goal {
            FindGoal::Coord(c) => c,
            FindGoal::Identity(i) => ident_coord(&i),
        };

        // store state by key, with futures
        let updated = Utc::now();
        let mut defer = vec![];
        let req_id = {
            let mut borrowed_states = self.0.find_states.lock().unwrap();
            let state = match borrowed_states.entry(goal) {
                Entry::Occupied(mut e) => {
                    if let Some(f) = fut {
                        e.get_mut().futures.push(f);
                    }
                    return;
                },
                Entry::Vacant(e) => e.insert(FindState {
                    req_id: self.0.next_req_id.fetch_add(1, Ordering::Relaxed),
                    goal: goal,
                    updated: updated.clone(),
                    nearest: vec![NearestNodeEntry {
                        dist: dist(&goal_coord, &self.0.own_coord).1,
                        node: NearestNodeEntryNode::Self_,
                    }],
                    outstanding: vec![],
                    requested: HashSet::new(),
                    value: None,
                    futures: vec![],
                }),
            };
            if let Some(f) = fut {
                state.futures.push(f);
            }
            let closest_peers = self.get_closest_peers(goal_coord, PARALLEL);
            for p in closest_peers {
                let challenge = generate_challenge();
                let (leading_zeros, dist) = dist(&node_ident_coord(&p.ident), &goal_coord);
                state.outstanding.push(OutstandingNodeEntry {
                    dist: dist,
                    leading_zeros: leading_zeros,
                    challenge: challenge.clone(),
                    node: p.clone(),
                });

                struct Defer {
                    challenge: Blob,
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
            self
                .send(
                    &d.addr,
                    wire::node::Protocol::V1(
                        wire::node::latest::Message::FindRequest(wire::node::latest::FindRequest {
                            challenge: d.challenge,
                            goal: goal,
                            sender: self.0.own_ident.clone(),
                        }),
                    ),
                )
                .await;
        }
        match self.0.find_timeouts.unbounded_send(NextFindTimeout {
            updated: updated,
            key: (goal, req_id),
        }) {
            Ok(_) => { },
            Err(e) => {
                let e = e.into_send_error();
                if e.is_disconnected() {
                    // nop
                } else if e.is_full() {
                    unreachable!();
                } else {
                    unreachable!();
                }
            },
        };
    }

    async fn complete_state(&self, state: FindState) {
        for f in state.futures {
            f.complete(FindResult {
                value: state.value.clone(),
                nearest: state.nearest.clone(),
            }).await;
        }
    }

    async fn handle_challenge_resp(&self, resp: wire::node::latest::ChallengeResponse) {
        let log = self.0.log.fork(ea!(action = "challenge_response", from_node_ident = resp.sender.dbg_str()));

        // Lookup request state
        let mut borrowed_states = self.0.challenge_states.lock().unwrap();
        let state_entry = match borrowed_states.entry(resp.sender.clone()) {
            Entry::Occupied(s) => s,
            Entry::Vacant(_) => {
                // Happens normally if outgoing replaced for a better peer and then the request is
                // resolved before resp comes back
                return;
            },
        };
        let state = state_entry.get();

        // Confirm sender is legit routable, add to own routing table
        if resp.sender.verify(&state.challenge, &resp.signature).is_err() {
            log.log(DEBUG_NODE, "Bad sender signature");
            return;
        }
        let state = state_entry.remove();
        self.add_good_node(resp.sender.clone(), Some(state.node));
    }

    async fn handle_find_resp(&self, resp: wire::node::latest::FindResponse) {
        let Ok(content) = resp.content.verify(&resp.sender) else {
            self.0.log.log(DEBUG_NODE, "Find response has invalid signature");
            return;
        };
        let log: Log = self.0.log.fork(ea!(action = "find_response", from_node_ident = resp.sender.dbg_str()));
        let goal;
        let mut defer_next_req = vec![];
        let mut transfer_stored_addr: Option<SocketAddr> = None;
        let state = {
            // Lookup request state, discard if unsolicited (or obsolete) find response
            let mut borrowed_states = self.0.find_states.lock().unwrap();
            let mut state_entry = match borrowed_states.entry(content.goal.clone()) {
                Entry::Occupied(s) => s,
                Entry::Vacant(_) => {
                    log.log(DEBUG_NODE, "No request state matching response target");
                    return;
                },
            };
            let state = state_entry.get_mut();
            goal = state.goal;
            let mut outstanding_entry: Option<OutstandingNodeEntry> = None;
            state.outstanding.retain(|e| {
                if e.node.ident == resp.sender {
                    if constant_time_eq(&content.challenge, &e.challenge) {
                        outstanding_entry = Some(e.clone());
                        return false;
                    } else {
                        log.log_with(
                            DEBUG_NODE,
                            "Wrong challenge",
                            ea!(want = e.challenge, got = content.challenge),
                        );
                    }
                }
                return true;
            });
            let outstanding_entry = match outstanding_entry {
                Some(e) => e,
                None => {
                    // 1. May have been dropped because there are better candidates
                    //
                    // 2. Entry skipped because wrong challenge
                    return;
                },
            };

            // Confirm sender is legit routable, possibly add to own routing table
            let (_, sender_dist) = dist(&node_ident_coord(&outstanding_entry.node.ident), &self.0.own_coord);
            if self.add_good_node(outstanding_entry.node.ident.clone(), Some(outstanding_entry.node.clone())) {
                if !self
                    .get_closest_peers(self.0.own_coord, NEIGHBORHOOD)
                    .iter()
                    .any(|p| dist(&node_ident_coord(&p.ident), &self.0.own_coord).1 < sender_dist) {
                    // Incidental work; added sender as a close peer, and sender is the closest peer
                    // so need to replicate all state to it (i.e. it is one of N closest nodes to all
                    // data on this node)
                    transfer_stored_addr = Some(outstanding_entry.node.address.0.clone());
                }
            }

            // The node responded and is legit, add it to the nearest node set
            loop {
                let mut replace_nearest = false;
                if state.nearest.len() == NEIGHBORHOOD {
                    if sender_dist >= state.nearest.last().unwrap().dist {
                        break;
                    }
                    replace_nearest = true;
                }
                if state.nearest.iter().any(|e| match &e.node {
                    NearestNodeEntryNode::Self_ => self.0.own_ident == outstanding_entry.node.ident,
                    NearestNodeEntryNode::Node(f) => f.ident == outstanding_entry.node.ident,
                }) {
                    break;
                }
                if replace_nearest {
                    state.nearest.pop();
                }
                state.nearest.push(NearestNodeEntry {
                    dist: sender_dist,
                    node: NearestNodeEntryNode::Node(outstanding_entry.node.clone()),
                });
                state.nearest.sort_by_key(|e| e.dist);
                break;
            }

            // Send requests to each of the next hop nodes that are closer than what we've
            // seen + that don't already have outgoing requests...
            let goal_coord = match &goal {
                FindGoal::Coord(c) => *c,
                FindGoal::Identity(i) => ident_coord(i),
            };
            for n in content.nodes {
                if !state.requested.insert(n.ident.clone()) {
                    // Already considered/requested this node previously - this overlaps info in
                    // nearest/outstanding partially, but if we reject a response (ex: bad signature)
                    // it will never go into the nearest/outstanding collections so we could request
                    // it repeatedly. This is an explicit check on that.
                    continue;
                }
                let candidate_hash = node_ident_coord(&n.ident);
                let (leading_zeros, candidate_dist) = dist(&candidate_hash, &goal_coord);

                // If nearest list is full and found node is farther away than any current nodes,
                // drop it
                if state.nearest.len() == NEIGHBORHOOD && candidate_dist >= state.nearest.last().unwrap().dist {
                    continue;
                }

                // If outstanding list is full and found node is farther away than any current
                // nodes, drop it
                let mut replace_outstanding = false;
                if state.outstanding.len() == PARALLEL {
                    if candidate_dist >= state.outstanding.last().unwrap().dist {
                        continue;
                    }

                    // Not farther away, we can pop the farther one off and add the found node below
                    replace_outstanding = true;
                }

                // If found node already in nearest, drop (ignore) it
                if state.nearest.iter().any(|e| n.ident == *match &e.node {
                    NearestNodeEntryNode::Self_ => &self.0.own_ident,
                    NearestNodeEntryNode::Node(f) => &f.ident,
                }) {
                    continue;
                }

                // If found node already in outstanding, drop (ignore) it
                if state.outstanding.iter().any(|e| e.node.ident == n.ident) {
                    continue;
                }
                let challenge = generate_challenge();
                if replace_outstanding {
                    state.outstanding.pop();
                }
                state.outstanding.push(OutstandingNodeEntry {
                    dist: candidate_dist,
                    challenge: challenge.clone(),
                    node: n.clone(),
                    leading_zeros: leading_zeros,
                });
                state.outstanding.sort_by_key(|e| e.dist);

                struct Defer {
                    challenge: Blob,
                    addr: SocketAddr,
                }

                defer_next_req.push(Defer {
                    challenge: challenge,
                    addr: n.address.0.clone(),
                });
            }

            // Process received value
            if let (Some(value), FindGoal::Identity(goal_identity)) = (content.value, goal) {
                bb!{
                    let found_published;
                    match &value {
                        stored::announcement::Announcement::V1(found) => {
                            let Ok(content) = found.verify(&goal_identity) else {
                                log.log(DEBUG_NODE, "Got value with bad signature");
                                break;
                            };
                            found_published = content.published;
                        },
                    }
                    if found_published + expiry() < Utc::now() {
                        log.log_with(DEBUG_NODE, "Got expired value", ea!(published = found_published.to_rfc3339()));
                        break;
                    }
                    match &mut state.value {
                        Some(state_value) => {
                            let have_published;
                            match state_value {
                                stored::announcement::Announcement::V1(have_value) => {
                                    have_published = have_value.parse_unwrap().published;
                                },
                            }
                            if have_published > found_published {
                                log.log_with(
                                    DEBUG_NODE,
                                    "Received value older than one we already have",
                                    ea!(
                                        have_published = have_published.to_rfc3339(),
                                        found_published = found_published.to_rfc3339()
                                    ),
                                );
                                break;
                            }
                        },
                        _ => (),
                    }
                    state.value = Some(value);
                };
            }

            // If done cleanup or else update timeouts
            if state.outstanding.is_empty() {
                // Remove outstanding state to complete it
                Some(state_entry.remove())
            } else {
                // New things to do, bump updated time and re-queue
                state.updated = Utc::now();
                match self.0.find_timeouts.unbounded_send(NextFindTimeout {
                    updated: state.updated,
                    key: (state.goal.clone(), state.req_id),
                }) {
                    Ok(_) => { },
                    Err(e) => {
                        let e = e.into_send_error();
                        if e.is_disconnected() {
                            // nop
                        } else if e.is_full() {
                            unreachable!();
                        } else {
                            unreachable!();
                        }
                    },
                };
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
                self
                    .send(
                        &addr,
                        wire::node::Protocol::V1(wire::node::latest::Message::Store(wire::node::latest::StoreRequest {
                            key: k,
                            value: v,
                        })),
                    )
                    .await;
            }
        }
        if let Some(s) = state {
            self.complete_state(s).await;
        }
        for d in defer_next_req {
            self
                .send(
                    &d.addr,
                    wire::node::Protocol::V1(
                        wire::node::latest::Message::FindRequest(wire::node::latest::FindRequest {
                            challenge: d.challenge,
                            goal: goal,
                            sender: self.0.own_ident.clone(),
                        }),
                    ),
                )
                .await;
        }
    }

    fn get_closest_peers(&self, goal_coord: DhtCoord, count: usize) -> Vec<wire::node::latest::NodeInfo> {
        let buckets = self.0.buckets.lock().unwrap();
        let (leading_zeros, _) = dist(&goal_coord, &self.0.own_coord);
        let mut nodes: Vec<wire::node::latest::NodeInfo> = vec![];
        'outer1: for bucket in leading_zeros .. HASH_SIZE {
            for state in &buckets.buckets[bucket] {
                if nodes.len() >= count {
                    break 'outer1;
                }
                nodes.push(state.node.clone());
            }
        }
        if leading_zeros > 0 {
            'outer: for bucket in (0 .. leading_zeros - 1).rev() {
                for state in &buckets.buckets[bucket] {
                    if nodes.len() >= count {
                        break 'outer;
                    }
                    nodes.push(state.node.clone());
                }
            }
        }
        return nodes;
    }

    async fn handle(&self, m: wire::node::Protocol, reply_to: &SocketAddr) -> Result<(), loga::Error> {
        let log = self.0.log.fork(ea!(from_addr = reply_to, message = m.dbg_str()));
        log.log(DEBUG_NODE, "Received");
        match m {
            wire::node::Protocol::V1(v1) => match v1 {
                wire::node::latest::Message::FindRequest(m) => {
                    let body = wire::node::latest::FindResponseContent {
                        challenge: m.challenge,
                        goal: m.goal,
                        sender: self.0.own_ident.clone(),
                        nodes: self.get_closest_peers(match m.goal {
                            FindGoal::Coord(c) => c,
                            FindGoal::Identity(i) => ident_coord(&i),
                        }, NEIGHBORHOOD),
                        value: bb!{
                            let FindGoal:: Identity(ident) = m.goal else {
                                break None;
                            };
                            break self.0.store.lock().unwrap().get(&ident).map(|v| v.value.clone());
                        },
                    };
                    self
                        .send(
                            reply_to,
                            wire::node::Protocol::V1(
                                wire::node::latest::Message::FindResponse(wire::node::latest::FindResponse {
                                    sender: self.0.own_ident.clone(),
                                    content: <wire
                                    ::node
                                    ::latest
                                    ::BincodeSignature<wire::node::latest::FindResponseContent, NodeIdentity>>::sign(
                                        &self.0.own_secret,
                                        body,
                                    ),
                                }),
                            ),
                        )
                        .await;
                    if self.add_good_node(m.sender.clone(), None) {
                        self.start_challenge(m.sender, reply_to).await;
                    }
                },
                wire::node::latest::Message::FindResponse(m) => {
                    self.handle_find_resp(m).await;
                },
                wire::node::latest::Message::Store(m) => {
                    self.0.log.log_with(DEBUG_NODE, "Storing", ea!(value = m.key.dbg_str()));
                    let new_published;
                    match &m.value {
                        stored::announcement::Announcement::V1(value) => {
                            let Ok(new_content) = value.verify(&m.key) else {
                                return Err(self.0.log.err("Store request failed signature validation"));
                            };
                            new_published = new_content.published;
                        },
                    }
                    if new_published > Utc::now() + Duration::minutes(1) {
                        return Err(self.0.log.err("Store request published date too far in the future"));
                    }
                    match self.0.store.lock().unwrap().entry(m.key) {
                        Entry::Occupied(mut e) => {
                            let have_published;
                            match &e.get().value {
                                stored::announcement::Announcement::V1(have_value) => {
                                    have_published = have_value.parse_unwrap().published;
                                },
                            }
                            if new_published > have_published {
                                e.insert(ValueState {
                                    value: m.value,
                                    updated: Utc::now(),
                                });
                            }
                        },
                        Entry::Vacant(e) => {
                            e.insert(ValueState {
                                value: m.value,
                                updated: Utc::now(),
                            });
                        },
                    };
                },
                wire::node::latest::Message::Ping => {
                    self
                        .send(
                            reply_to,
                            wire::node::Protocol::V1(wire::node::latest::Message::Pung(self.0.own_ident.clone())),
                        )
                        .await;
                },
                wire::node::latest::Message::Pung(k) => {
                    let state = match self.0.ping_states.lock().unwrap().entry(k.clone()) {
                        Entry::Occupied(s) => s.remove(),
                        Entry::Vacant(_) => return Ok(()),
                    };
                    self.mark_node_unresponsive(k, state.leading_zeros, false);
                },
                wire::node::latest::Message::Challenge(challenge) => {
                    self
                        .send(
                            reply_to,
                            wire::node::Protocol::V1(
                                wire::node::latest::Message::ChallengeResponse(wire::node::latest::ChallengeResponse {
                                    sender: self.0.own_ident.clone(),
                                    signature: self.0.own_secret.sign(&challenge),
                                }),
                            ),
                        )
                        .await;
                },
                wire::node::latest::Message::ChallengeResponse(resp) => {
                    self.handle_challenge_resp(resp).await;
                },
            },
        };
        Ok(())
    }

    /// Add a node, or check if adding a node would be new (returns whether id is new)
    fn add_good_node(&self, id: node_identity::NodeIdentity, node: Option<wire::node::latest::NodeInfo>) -> bool {
        let log = self.0.log.fork(ea!(activity = "add_good_node", node = id.dbg_str()));
        let log = &log;
        if id == self.0.own_ident {
            log.log(DEBUG_NODE, "Own node id, ignoring");
            return false;
        }
        let (leading_zeros, _) = dist(&node_ident_coord(&id), &self.0.own_coord);
        let mut buckets = self.0.buckets.lock().unwrap();
        let buckets = &mut *buckets;

        fn store_addr(
            log: &Log,
            buckets: &mut Buckets,
            own_coord: &DhtCoord,
            addr: SocketAddr,
            new_ident: NodeIdentity,
        ) {
            if let Some(old) = buckets.addrs.get(&addr) {
                let (leading_zeros, _) = dist(&node_ident_coord(old), own_coord);
                let bucket = &mut buckets.buckets[leading_zeros];
                for i in 0 .. bucket.len() {
                    let n = &mut bucket[i];
                    if &n.node.ident == old {
                        log.log_with(
                            DEBUG_NODE,
                            "Replaced node with same addr",
                            ea!(addr = addr, old_ident = old, new_ident = new_ident),
                        );
                        bucket.remove(i);
                        break;
                    }
                }
            };
            buckets.addrs.insert(addr, new_ident);
        }

        let new_node = 'logic : loop {
            let bucket = &mut buckets.buckets[leading_zeros];
            let mut last_unresponsive: Option<usize> = None;

            // Updated or already known
            for i in 0 .. bucket.len() {
                let bucket_entry = &mut bucket[i];
                if bucket_entry.node.ident == id {
                    if let Some(node) = node {
                        if bucket_entry.unresponsive {
                            bucket_entry.unresponsive = false;
                        }
                        buckets.addrs.remove(&bucket_entry.node.address.0);
                        let new_state = wire::node::latest::NodeState {
                            node: node.clone(),
                            unresponsive: false,
                        };
                        let changed = *bucket_entry == new_state;
                        *bucket_entry = new_state;
                        if changed {
                            self.0.dirty.store(true, Ordering::Relaxed);
                        }
                        log.log(DEBUG_NODE, "Updated existing node");
                        store_addr(log, buckets, &self.0.own_coord, node.address.0, node.ident);
                    }
                    break 'logic false;
                }
                if bucket_entry.unresponsive {
                    last_unresponsive = Some(i);
                }
            }

            // Empty slot
            if bucket.len() < NEIGHBORHOOD {
                if let Some(node) = node {
                    bucket.insert(0, wire::node::latest::NodeState {
                        node: node.clone(),
                        unresponsive: false,
                    });
                    self.0.dirty.store(true, Ordering::Relaxed);
                    log.log(DEBUG_NODE, "Added node to empty slot");
                    store_addr(log, buckets, &self.0.own_coord, node.address.0, node.ident);
                }
                break true;
            }

            // Replacing dead
            if let Some(i) = last_unresponsive {
                if let Some(node) = node {
                    buckets.addrs.remove(&bucket[i].node.address.0);
                    bucket.remove(i);
                    bucket.push(wire::node::latest::NodeState {
                        node: node.clone(),
                        unresponsive: false,
                    });
                    self.0.dirty.store(true, Ordering::Relaxed);
                    log.log(DEBUG_NODE, "Replaced dead node");
                    store_addr(log, buckets, &self.0.own_coord, node.address.0, node.ident);
                }
                break 'logic true;
            }
            log.log(DEBUG_NODE, "Nowhere to place, dropping");
            break false;
        };
        return new_node;
    }

    async fn send(&self, addr: &SocketAddr, data: wire::node::Protocol) {
        let bytes = data.to_bytes();
        self.0.log.log_with(DEBUG_NODE, "Sending", ea!(to_addr = addr, message = data.dbg_str()));
        self.0.socket.send_to(&bytes, addr).await.unwrap();
    }
}
