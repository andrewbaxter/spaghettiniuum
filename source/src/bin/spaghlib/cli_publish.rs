use {
    loga::{
        ea,
        Log,
        ResultContext,
    },
    serde::Serialize,
    spaghettinuum::{
        interface::{
            stored::{
                self,
                record::{
                    delegate_record::build_delegate_key,
                    dns_record::{
                        build_dns_key,
                        RecordType,
                    },
                    record_utils::{
                        split_dns_name,
                        split_dns_path,
                        split_record_key,
                        RecordRoot,
                    },
                },
            },
            wire,
        },
        publishing::system_publisher_url_pairs,
        resolving::system_resolver_url_pairs,
        utils::{
            identity_secret::get_identity_signer,
            publish_util,
        },
    },
    std::{
        collections::HashMap,
        net::{
            IpAddr,
            Ipv4Addr,
            Ipv6Addr,
        },
        str::FromStr,
    },
};

pub mod args {
    use {
        aargvark::{
            Aargvark,
            AargvarkJson,
        },
        spaghettinuum::interface::{
            config::{
                shared::IdentitySecretArg,
            },
            stored,
        },
        std::{
            collections::{
                HashMap,
                HashSet,
            },
            path::PathBuf,
        },
    };

    #[derive(Aargvark)]
    pub struct NewLocalIdentity {
        /// Store the new id and secret in a file at this path
        pub path: PathBuf,
    }

    #[derive(Aargvark)]
    pub struct UnsetAll {
        /// Identity whose records to wipe
        pub identity: IdentitySecretArg,
    }

    #[derive(Aargvark)]
    pub struct Set {
        /// Identity to publish as
        pub identity: IdentitySecretArg,
        /// Data to publish.  Must be json in the structure
        /// `{KEY: {"ttl": MINUTES, "value": DATA}, ...}`. `KEY` is a string that's a
        /// dotted list of key segments, with `/` to escape dots and escape characters.
        pub data: AargvarkJson<HashMap<String, stored::record::latest::RecordValue>>,
    }

    #[derive(Aargvark)]
    pub struct SetDns {
        /// Identity to publish
        pub identity: IdentitySecretArg,
        /// Dotted list of subdomains to publish under (ex: `a.b.c` will publish
        /// `a.b.c.IDENT.s`).
        pub subdomain: String,
        /// TTL for hits and misses, in minutes
        pub ttl: u32,
        /// A list of other DNS names.
        pub delegate: Vec<String>,
        /// A list of Ipv4 addresses
        pub dns_a: Vec<String>,
        /// A list of Ipv6 addresses
        pub dns_aaaa: Vec<String>,
        /// A list of valid TXT record strings
        pub dns_txt: Vec<String>,
        /// Mail server names. These are automatically prioritized, with the first having
        /// priority 0, second 1, etc.
        pub dns_mx: Vec<String>,
    }

    #[derive(Aargvark)]
    pub struct Unset {
        /// Identity whose keys to stop publishing
        pub identity: IdentitySecretArg,
        /// Keys to stop publishing
        pub keys: HashSet<String>,
    }

    #[derive(Aargvark)]
    pub struct ListKeys {
        pub identity: String,
    }

    #[derive(Aargvark)]
    pub struct Announce {
        /// Identity to advertise this publisher for
        pub identity: IdentitySecretArg,
    }

    #[derive(Aargvark)]
    #[vark(break_help)]
    pub enum Publish {
        /// Announce the publisher server as the authority for this identity. This must be
        /// done before any values published on this publisher can be queried, and replaces
        /// the previous publisher.
        Announce(Announce),
        /// Create or replace existing publish data for an identity on a publisher server
        Set(Set),
        /// A shortcut for publishing DNS data, generating the key values for you
        SetDns(SetDns),
        /// Stop publishing specific records
        Unset(Unset),
        /// Stop publishing all records for an identity
        UnsetAll(UnsetAll),
    }
}

pub async fn run(log: &Log, config: args::Publish) -> Result<(), loga::Error> {
    let resolvers = system_resolver_url_pairs(log)?;
    let publishers = system_publisher_url_pairs(log)?;
    match config {
        args::Publish::Announce(config) => {
            let signer =
                get_identity_signer(config.identity)
                    .await
                    .stack_context(&log, "Error constructing signer for identity")?;
            publish_util::announce(log, &resolvers, &publishers, &signer).await?;
        },
        args::Publish::Set(config) => {
            let signer =
                get_identity_signer(config.identity)
                    .await
                    .stack_context(&log, "Error constructing signer for identity")?;
            publish_util::publish(
                log,
                &resolvers,
                &publishers,
                &signer,
                wire::api::publish::latest::PublishRequestContent {
                    set: config
                        .data
                        .value
                        .into_iter()
                        .map(|(k, v)| (split_record_key(&k), stored::record::RecordValue::V1(v)))
                        .collect(),
                    ..Default::default()
                },
            ).await?;
        },
        args::Publish::SetDns(config) => {
            let path = split_dns_path(&config.subdomain)?;

            fn rec_val(ttl: u32, data: impl Serialize) -> stored::record::RecordValue {
                return stored::record::RecordValue::latest(stored::record::latest::RecordValue {
                    ttl: ttl as i32,
                    data: Some(serde_json::to_value(&data).unwrap()),
                });
            }

            let mut kvs = HashMap::new();
            if !config.delegate.is_empty() {
                let mut values = vec![];
                for v in config.delegate {
                    if let Ok(ip) = IpAddr::from_str(&v) {
                        values.push((RecordRoot::Ip(ip), vec![]));
                    } else {
                        values.push(
                            split_dns_name(
                                hickory_resolver::Name::from_utf8(&v).context("Invalid DNS name for delegation")?,
                            ).context_with("Invalid delegation", ea!(value = v))?,
                        );
                    }
                }
                kvs.insert(
                    build_delegate_key(path.clone()),
                    rec_val(
                        config.ttl,
                        stored::record::delegate_record::Delegate::latest(
                            stored::record::delegate_record::latest::Delegate(values),
                        ),
                    ),
                );
            }
            if !config.dns_a.is_empty() {
                let mut v = vec![];
                for r in config.dns_a {
                    v.push(Ipv4Addr::from_str(&r).context("Invalid IP address for A record")?);
                }
                kvs.insert(
                    build_dns_key(path.clone(), RecordType::A),
                    rec_val(
                        config.ttl,
                        stored::record::dns_record::DnsA::V1(stored::record::dns_record::latest::DnsA(v)),
                    ),
                );
            }
            if !config.dns_aaaa.is_empty() {
                let mut v = vec![];
                for r in config.dns_aaaa {
                    v.push(Ipv6Addr::from_str(&r).context("Invalid IP address for AAAA record")?);
                }
                kvs.insert(
                    build_dns_key(path.clone(), RecordType::Aaaa),
                    rec_val(
                        config.ttl,
                        &stored::record::dns_record::DnsAaaa::V1(stored::record::dns_record::latest::DnsAaaa(v)),
                    ),
                );
            }
            if !config.dns_txt.is_empty() {
                kvs.insert(
                    build_dns_key(path.clone(), RecordType::Txt),
                    rec_val(
                        config.ttl,
                        &stored::record::dns_record::DnsTxt::V1(
                            stored::record::dns_record::latest::DnsTxt(config.dns_txt),
                        ),
                    ),
                );
            }
            if !config.dns_mx.is_empty() {
                kvs.insert(
                    build_dns_key(path.clone(), RecordType::Mx),
                    rec_val(
                        config.ttl,
                        &stored::record::dns_record::DnsMx::V1(
                            stored::record::dns_record::latest::DnsMx(config.dns_mx),
                        ),
                    ),
                );
            }
            let signer =
                get_identity_signer(config.identity)
                    .await
                    .stack_context(&log, "Error constructing signer for identity")?;
            publish_util::publish(
                log,
                &resolvers,
                &publishers,
                &signer,
                wire::api::publish::latest::PublishRequestContent {
                    set: kvs,
                    ..Default::default()
                },
            ).await?;
        },
        args::Publish::Unset(config) => {
            let signer =
                get_identity_signer(config.identity)
                    .await
                    .stack_context(&log, "Error constructing signer for identity")?;
            publish_util::publish(
                log,
                &resolvers,
                &publishers,
                &signer,
                wire::api::publish::latest::PublishRequestContent {
                    clear: config.keys.into_iter().map(|k| split_record_key(&k)).collect(),
                    ..Default::default()
                },
            ).await?;
        },
        args::Publish::UnsetAll(config) => {
            let signer =
                get_identity_signer(config.identity)
                    .await
                    .stack_context(&log, "Error constructing signer for identity")?;
            publish_util::publish(
                log,
                &resolvers,
                &publishers,
                &signer,
                wire::api::publish::latest::PublishRequestContent {
                    clear_all: true,
                    ..Default::default()
                },
            ).await?;
        },
    }
    return Ok(());
}
