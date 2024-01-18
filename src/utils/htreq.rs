use std::{
    collections::HashMap,
    str::FromStr,
    net::IpAddr,
};
use chrono::Duration;
use http_body_util::{
    Limited,
    BodyExt,
    Full,
    Empty,
};
use hyper::{
    Request,
    StatusCode,
    Uri,
    body::Bytes,
};
use hyper_rustls::HttpsConnectorBuilder;
use loga::{
    ea,
    ResultContext,
};
use rand::{
    seq::SliceRandom,
    thread_rng,
};
use tokio::{
    select,
    time::sleep,
};
use tower_service::Service;
use crate::ta_res;

pub type Conn = hyper_rustls::MaybeHttpsStream<hyper_util::rt::tokio::TokioIo<tokio::net::TcpStream>>;

pub fn uri_parts(uri: &Uri) -> Result<(String, String, u16), loga::Error> {
    let host = uri.authority().context("Url is missing host")?.to_string();
    let scheme = uri.scheme().context("Url is missing scheme")?.to_string();
    let port = match uri.port_u16() {
        Some(p) => p,
        None => match scheme.as_str() {
            "http" => 80,
            "https" => 443,
            _ => return Err(loga::err("Only http/https urls are supported")),
        },
    };
    return Ok((scheme, host, port));
}

pub async fn send<
    ID: Send,
    IE: std::error::Error + Send + Sync + 'static,
    I: http_body::Body<Data = ID, Error = IE> + 'static,
>(conn: Conn, max_size: usize, max_time: Duration, req: Request<I>) -> Result<Vec<u8>, loga::Error> {
    let read = async move {
        ta_res!((Vec < u8 >, StatusCode));
        let (mut sender, mut conn) =
            hyper::client::conn::http1::handshake(conn).await.context("Error completing http handshake")?;
        let work = sender.send_request(req);
        let resp = select!{
            _ =& mut conn => {
                return Err(loga::err("Connection failed while sending request"));
            }
            r = work => r,
        }.context("Error sending request")?;
        let status = resp.status();
        let work = Limited::new(resp.into_body(), max_size).collect();
        let resp = select!{
            _ =& mut conn => {
                return Err(loga::err("Connection failed while reading body"));
            }
            r = work => r,
        }.map_err(|e| loga::err_with("Error reading response", ea!(err = e)))?.to_bytes().to_vec();
        return Ok((resp, status));
    };
    let (resp, status) = select!{
        _ = sleep(max_time.to_std().unwrap()) => {
            return Err(loga::err("Timeout waiting for response from server"));
        }
        x = read => x ?,
    };
    if !status.is_success() {
        return Err(loga::err_with("Server returned error response", ea!(body = String::from_utf8_lossy(&resp))));
    }
    return Ok(resp);
}

/// Creates a new HTTPS/HTTP connection with default settings.  `base_uri` is just
/// used for schema, host, and port.
pub async fn new_conn(base_uri: &Uri) -> Result<Conn, loga::Error> {
    let (scheme, host, port) = uri_parts(base_uri).context("Incomplete url")?;
    let mut ipv4s = vec![];
    let mut ipv6s = vec![];
    for ip in hickory_resolver::TokioAsyncResolver::tokio(hickory_resolver::config::ResolverConfig::default(), {
        let mut opts = hickory_resolver::config::ResolverOpts::default();
        opts.ip_strategy = hickory_resolver::config::LookupIpStrategy::Ipv4AndIpv6;
        opts
    }).lookup_ip(&format!("{}.", host)).await.context("Failed to look up lookup host ip addresses")? {
        match ip {
            std::net::IpAddr::V4(_) => {
                ipv4s.push(ip);
            },
            std::net::IpAddr::V6(_) => {
                ipv6s.push(ip);
            },
        }
    }
    let mut try_ips = vec![];
    {
        let mut r = thread_rng();
        try_ips.extend(ipv4s.choose(&mut r));
        try_ips.extend(ipv6s.choose(&mut r));
    }

    async fn inner(ip: IpAddr, scheme: &str, host: &str, port: u16) -> Result<Conn, loga::Error> {
        return Ok(
            HttpsConnectorBuilder::new()
                .with_webpki_roots()
                .https_or_http()
                .with_server_name(host.to_string())
                .enable_http1()
                .build()
                .call(Uri::from_str(&format!("{}://{}:{}", scheme, host, port)).unwrap())
                .await
                .map_err(
                    |e| loga::err_with(
                        "Error connecting to host",
                        ea!(err = e.to_string(), source_addr = ip, host = host, port = port),
                    ),
                )?,
        );
    }

    let first_ip = try_ips.pop().context("No dns records found for host")?;
    match try_ips.pop() {
        Some(second_ip) => {
            select!{
                first = inner(first_ip, &scheme, &host, port) => return Ok(first?),
                second = inner(second_ip, &scheme, &host, port) => return Ok(second?),
            }
        },
        None => {
            return Ok(inner(first_ip, &scheme, &host, port).await?);
        },
    }
}

pub async fn post(
    uri: impl AsRef<str>,
    headers: &HashMap<String, String>,
    body: Vec<u8>,
    max_size: usize,
) -> Result<Vec<u8>, loga::Error> {
    let uri = uri.as_ref();
    let uri = Uri::from_str(uri).context_with("URI couldn't be parsed", ea!(uri = uri))?;
    let req = Request::builder();
    let mut req = req.method("POST").uri(uri.clone());
    for (k, v) in headers.iter() {
        req = req.header(k, v);
    }
    return Ok(
        send(
            new_conn(&uri).await?,
            max_size,
            Duration::seconds(10),
            req.body(Full::new(Bytes::from(body))).unwrap(),
        )
            .await
            .context_with("Error sending POST", ea!(uri = uri))?,
    );
}

pub async fn get(
    uri: impl AsRef<str>,
    headers: &HashMap<String, String>,
    max_size: usize,
) -> Result<Vec<u8>, loga::Error> {
    let uri = uri.as_ref();
    let uri = Uri::from_str(uri).context_with("URI couldn't be parsed", ea!(uri = uri))?;
    let req = Request::builder();
    let mut req = req.method("GET").uri(uri.clone());
    for (k, v) in headers.iter() {
        req = req.header(k, v);
    }
    return Ok(
        send(new_conn(&uri).await?, max_size, Duration::seconds(10), req.body(Empty::<Bytes>::new()).unwrap())
            .await
            .context_with("Error sending GET", ea!(uri = uri))?,
    );
}

pub async fn get_text(
    uri: impl AsRef<str>,
    headers: &HashMap<String, String>,
    max_size: usize,
) -> Result<String, loga::Error> {
    let body = get(uri, headers, max_size).await?;
    return Ok(
        String::from_utf8(
            body,
        ).map_err(
            |e| loga::err_with(
                "Received data isn't valid utf-8",
                ea!(err = e.to_string(), body = String::from_utf8_lossy(e.as_bytes())),
            ),
        )?,
    );
}
