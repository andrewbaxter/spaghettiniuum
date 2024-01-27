pub mod v1;

use loga::{
    ea,
    ResultContext,
};
pub use v1 as latest;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        Mutex,
    },
};
use aargvark::Aargvark;
use schemars::JsonSchema;
use serde::{
    Serialize,
    Deserialize,
};
use std::net::{
    SocketAddr,
    ToSocketAddrs,
};
use schemars::{
    schema::{
        SchemaObject,
        Metadata,
        InstanceType,
    },
};
use crate::utils::blob::{
    ToBlob,
    Blob,
};
use super::identity::Identity;

pub const ENV_API_ADDR: &'static str = "SPAGH";
pub const ENV_CONFIG: &'static str = "SPAGH_CONFIG";
pub const ENV_API_ADMIN_TOKEN: &'static str = "SPAGH_TOKEN";
pub const ENV_CERTIFIER: &'static str = "SPAGH_CERTIFIER";
pub const DEFAULT_CERTIFIER_URL: &'static str = "https://certipasta.isandrew.com";
pub const PORT_NODE: u16 = 43890;
pub const PORT_PUBLISHER: u16 = 43891;

#[derive(Clone)]
pub struct StrSocketAddr(pub String, Arc<Mutex<Option<SocketAddr>>>);

impl StrSocketAddr {
    /// Only for serialization, dummy socketaddr with no lookup
    pub fn new_fake(s: String) -> StrSocketAddr {
        return StrSocketAddr(s, Arc::new(Mutex::new(None)));
    }

    pub fn resolve(&self) -> Result<SocketAddr, loga::Error> {
        let mut resolved = self.1.lock().unwrap();
        match *resolved {
            Some(v) => return Ok(v),
            None => {
                let v =
                    self
                        .0
                        .to_socket_addrs()
                        .context_with("Error turning socket address into IP", ea!(name = self.0))?
                        .into_iter()
                        .next()
                        .context_with("No address resolved from name", ea!(name = self.0))?;
                *resolved = Some(v);
                return Ok(v);
            },
        }
    }
}

impl From<SocketAddr> for StrSocketAddr {
    fn from(value: SocketAddr) -> Self {
        return StrSocketAddr(value.to_string(), Arc::new(Mutex::new(Some(value))));
    }
}

impl std::fmt::Display for StrSocketAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        format!(
            "{} ({})",
            self.0,
            self.1.lock().unwrap().map(|x| x.to_string()).unwrap_or("unresolved".to_string())
        ).fmt(f)
    }
}

impl Serialize for StrSocketAddr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer {
        return self.0.serialize(serializer);
    }
}

impl<'t> Deserialize<'t> for StrSocketAddr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'t> {
        let s = String::deserialize(deserializer)?;
        return Ok(StrSocketAddr(s.clone(), Arc::new(Mutex::new(None))));
    }
}

impl JsonSchema for StrSocketAddr {
    fn schema_name() -> String {
        return "StrSocketAddr".to_string();
    }

    fn json_schema(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        return SchemaObject {
            instance_type: Some(InstanceType::String.into()),
            metadata: Some(Box::new(Metadata {
                description: Some(
                    "An ip address or domain (ex: \"localhost\") which resolves to an address".to_string(),
                ),
                ..Default::default()
            })),
            ..Default::default()
        }.into();
    }
}

/// An identity with its associated secret.
#[derive(Clone, Aargvark, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackedIdentityArg {
    /// A file containing a generated key
    Local(PathBuf),
    /// PC/SC card with ED25519 key
    #[cfg(feature = "card")]
    Card {
        /// Card to register, using id per pcscd (not identity id)
        pcsc_id: String,
        /// Card pin
        pin: String,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackedIdentityLocal {
    V1(v1::LocalIdentitySecret),
}

impl BackedIdentityLocal {
    pub fn new() -> (Identity, Self) {
        let (ident, secret) = v1::LocalIdentitySecret::new();
        return (Identity::V1(ident), BackedIdentityLocal::V1(secret));
    }

    pub fn identity(&self) -> Identity {
        match self {
            BackedIdentityLocal::V1(s) => Identity::V1(s.identity()),
        }
    }

    pub fn sign(&self, message: &[u8]) -> Blob {
        match self {
            BackedIdentityLocal::V1(v) => v.sign(message).blob(),
        }
    }
}
