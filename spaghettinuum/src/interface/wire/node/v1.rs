use generic_array::GenericArray;
use serde::Serializer;
use serde::de::DeserializeOwned;
use serde::{
    Serialize,
    Deserialize,
};
use core::fmt::Debug;
use std::marker::PhantomData;
use sha2::Digest;
use crate::interface::stored::announcement::Announcement;
use crate::interface::stored::identity::Identity;
use crate::interface::stored::node_identity::NodeIdentity;
use crate::interface::stored::shared::SerialAddr;
use crate::utils::blob::{
    Blob,
    ToBlob,
};

#[derive(Serialize, Deserialize, PartialEq, Eq, Hash, Clone)]
#[serde(rename_all = "snake_case")]
pub struct BincodeSignature<T: Serialize + DeserializeOwned, I> {
    pub message: Blob,
    pub signature: Blob,
    #[serde(skip)]
    pub _p: PhantomData<(T, I)>,
}

impl<T: Serialize + DeserializeOwned, I> BincodeSignature<T, I> {
    pub fn parse_unwrap(&self) -> T {
        return bincode::deserialize(&self.message).unwrap();
    }
}

impl<T: Serialize + DeserializeOwned, I> std::fmt::Debug for BincodeSignature<T, I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f
            .debug_struct("BincodeSignature")
            .field("message", &self.message)
            .field("signature", &self.signature)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct DhtCoord(pub GenericArray<u8, generic_array::typenum::U32>);

impl Serialize for DhtCoord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer {
        return self.0.as_slice().serialize(serializer);
    }
}

impl<'a> Deserialize<'a> for DhtCoord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'a> {
        let bytes = <Vec<u8>>::deserialize(deserializer)?;
        return Ok(
            Self(
                <GenericArray<u8, generic_array::typenum::U32>>::from_exact_iter(
                    bytes.into_iter(),
                ).ok_or_else(|| serde::de::Error::custom("DhtCoord has the wrong number of bytes"))?,
            ),
        );
    }
}

// Ed25519
pub struct Hash(Blob);

impl Hash {
    pub fn new(data: &[u8]) -> Self {
        return Self(<sha2::Sha256 as Digest>::digest(data).blob());
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Hash, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum FindGoal {
    Coord(DhtCoord),
    Identity(Identity),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FindRequest {
    pub sender: NodeIdentity,
    pub challenge: Blob,
    pub goal: FindGoal,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct NodeInfo {
    pub ident: NodeIdentity,
    pub address: SerialAddr,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub struct FindResponseContent {
    pub goal: FindGoal,
    pub challenge: Blob,
    pub sender: NodeIdentity,
    pub nodes: Vec<NodeInfo>,
    pub value: Option<Announcement>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub struct FindResponse {
    pub sender: NodeIdentity,
    pub content: BincodeSignature<FindResponseContent, NodeIdentity>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StoreRequest {
    pub key: Identity,
    pub value: Announcement,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StoreResponse {
    pub key: Identity,
    pub value: Announcement,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ChallengeResponse {
    pub sender: NodeIdentity,
    pub signature: Blob,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Message {
    FindRequest(FindRequest),
    FindResponse(FindResponse),
    Store(StoreRequest),
    Ping,
    Pung(NodeIdentity),
    Challenge(Blob),
    ChallengeResponse(ChallengeResponse),
}

impl Message {
    pub fn from_bytes(bytes: &[u8]) -> Result<Message, loga::Error> {
        return Ok(bincode::deserialize(bytes)?);
    }

    pub fn to_bytes(&self) -> Blob {
        return bincode::serialize(self).unwrap().blob();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct NodeState {
    pub node: NodeInfo,
    pub unresponsive: bool,
}
