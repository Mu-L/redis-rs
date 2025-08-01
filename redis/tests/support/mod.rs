#![allow(dead_code)]

#[cfg(feature = "aio")]
use futures::Future;
#[cfg(feature = "aio")]
use redis::{aio, cmd};
use redis::{Commands, ConnectionAddr, InfoDict, Pipeline, ProtocolVersion, RedisResult, Value};
use redis_test::server::{use_protocol, Module, RedisServer};
use redis_test::utils::{get_random_available_port, TlsFilePaths};
#[cfg(feature = "tls-rustls")]
use std::{
    fs::File,
    io::{BufReader, Read},
};
use std::{io, thread::sleep, time::Duration};

#[cfg(feature = "cache-aio")]
use redis::caching::CacheConfig;
#[cfg(feature = "tls-rustls")]
use redis::{ClientTlsConfig, TlsCertificates};

pub fn current_thread_runtime() -> tokio::runtime::Runtime {
    let mut builder = tokio::runtime::Builder::new_current_thread();

    #[cfg(feature = "aio")]
    builder.enable_io();

    builder.enable_time();

    builder.build().unwrap()
}

#[cfg(feature = "aio")]
pub enum RuntimeType {
    #[cfg(feature = "tokio-comp")]
    Tokio,
    #[cfg(feature = "async-std-comp")]
    AsyncStd,
    #[cfg(feature = "smol-comp")]
    Smol,
}

#[cfg(feature = "aio")]
pub fn block_on_all<F, V>(f: F, runtime: RuntimeType) -> F::Output
where
    F: Future<Output = RedisResult<V>>,
{
    use std::panic;
    use std::sync::atomic::{AtomicBool, Ordering};

    static CHECK: AtomicBool = AtomicBool::new(false);

    // TODO - this solution is purely single threaded, and won't work on multiple threads at the same time.
    // This is needed because Tokio's Runtime silently ignores panics - https://users.rust-lang.org/t/tokio-runtime-what-happens-when-a-thread-panics/95819
    // Once Tokio stabilizes the `unhandled_panic` field on the runtime builder, it should be used instead.
    panic::set_hook(Box::new(|panic| {
        println!("Panic: {panic}");
        CHECK.store(true, Ordering::Relaxed);
    }));

    // This continuously query the flag, in order to abort ASAP after a panic.
    let check_future = futures_util::FutureExt::fuse(async {
        loop {
            if CHECK.load(Ordering::Relaxed) {
                return Err((redis::ErrorKind::IoError, "panic was caught").into());
            }
            futures_time::task::sleep(futures_time::time::Duration::from_millis(1)).await;
        }
    });
    let f = futures_util::FutureExt::fuse(f);
    futures::pin_mut!(f, check_future);

    let f = async move {
        futures::select! {res = f => res, err = check_future => err}
    };

    let res = match runtime {
        #[cfg(feature = "tokio-comp")]
        RuntimeType::Tokio => block_on_all_using_tokio(f),
        #[cfg(feature = "async-std-comp")]
        RuntimeType::AsyncStd => block_on_all_using_async_std(f),
        #[cfg(feature = "smol-comp")]
        RuntimeType::Smol => block_on_all_using_smol(f),
    };

    let _ = panic::take_hook();
    if CHECK.swap(false, Ordering::Relaxed) {
        panic!("Internal thread panicked");
    }

    res
}

#[cfg(feature = "aio")]
#[rstest::rstest]
#[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
#[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
#[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
#[should_panic(expected = "Internal thread panicked")]
fn test_block_on_all_panics_from_spawns(#[case] runtime: RuntimeType) {
    use std::sync::{atomic::AtomicBool, Arc};

    let slept = Arc::new(AtomicBool::new(false));
    let slept_clone = slept.clone();
    let _ = block_on_all(
        async {
            spawn(async move {
                futures_time::task::sleep(futures_time::time::Duration::from_millis(1)).await;
                slept_clone.store(true, std::sync::atomic::Ordering::Relaxed);
                panic!("As it should");
            });

            loop {
                futures_time::task::sleep(futures_time::time::Duration::from_millis(2)).await;
                if slept.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
            }

            Ok(())
        },
        runtime,
    );
}

#[cfg(feature = "tokio-comp")]
fn block_on_all_using_tokio<F>(f: F) -> F::Output
where
    F: Future,
{
    #[cfg(any(feature = "async-std-comp", feature = "smol-comp"))]
    redis::aio::prefer_tokio().unwrap();
    current_thread_runtime().block_on(f)
}

#[cfg(feature = "async-std-comp")]
fn block_on_all_using_async_std<F>(f: F) -> F::Output
where
    F: Future,
{
    #[cfg(any(feature = "tokio-comp", feature = "smol-comp"))]
    redis::aio::prefer_async_std().unwrap();
    async_std::task::block_on(f)
}

#[cfg(feature = "smol-comp")]
fn block_on_all_using_smol<F>(f: F) -> F::Output
where
    F: Future,
{
    #[cfg(any(feature = "tokio-comp", feature = "async-std-comp"))]
    redis::aio::prefer_smol().unwrap();
    smol::block_on(f)
}

#[cfg(any(feature = "cluster", feature = "cluster-async"))]
mod cluster;

#[cfg(any(feature = "cluster", feature = "cluster-async"))]
mod mock_cluster;

mod util;

#[cfg(any(feature = "cluster", feature = "cluster-async"))]
#[allow(unused_imports)]
pub use self::cluster::*;

#[cfg(any(feature = "cluster", feature = "cluster-async"))]
#[allow(unused_imports)]
pub use self::mock_cluster::*;

#[cfg(feature = "sentinel")]
mod sentinel;

#[cfg(feature = "sentinel")]
#[allow(unused_imports)]
pub use self::sentinel::*;

pub struct TestContext {
    pub server: RedisServer,
    pub client: redis::Client,
    pub protocol: ProtocolVersion,
}

pub(crate) fn start_tls_crypto_provider() {
    #[cfg(feature = "tls-rustls")]
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // we don't care about success, because failure means that the provider was set from another thread.
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

impl TestContext {
    pub fn new() -> TestContext {
        TestContext::with_modules(&[], false)
    }

    #[cfg(feature = "tls-rustls")]
    pub fn new_with_mtls() -> TestContext {
        Self::with_modules(&[], true)
    }

    pub fn with_tls(tls_files: TlsFilePaths, mtls_enabled: bool) -> TestContext {
        Self::with_modules_and_tls(&[], mtls_enabled, Some(tls_files))
    }

    pub fn with_modules(modules: &[Module], mtls_enabled: bool) -> TestContext {
        Self::with_modules_and_tls(modules, mtls_enabled, None)
    }

    pub fn new_with_addr(addr: ConnectionAddr) -> Self {
        Self::with_modules_addr_and_tls(&[], false, addr, None)
    }

    fn with_modules_and_tls(
        modules: &[Module],
        mtls_enabled: bool,
        tls_files: Option<TlsFilePaths>,
    ) -> Self {
        start_tls_crypto_provider();
        let redis_port = get_random_available_port();
        let addr = RedisServer::get_addr(redis_port);
        Self::with_modules_addr_and_tls(modules, mtls_enabled, addr, tls_files)
    }

    fn with_modules_addr_and_tls(
        modules: &[Module],
        mtls_enabled: bool,
        addr: ConnectionAddr,
        tls_files: Option<TlsFilePaths>,
    ) -> Self {
        let server = RedisServer::new_with_addr_tls_modules_and_spawner(
            addr,
            None,
            tls_files,
            mtls_enabled,
            modules,
            |cmd| {
                cmd.spawn()
                    .unwrap_or_else(|err| panic!("Failed to run {cmd:?}: {err}"))
            },
        );

        let client =
            build_single_client(server.connection_info(), &server.tls_paths, mtls_enabled).unwrap();

        let mut con;

        let millisecond = Duration::from_millis(1);
        let mut retries = 0;
        loop {
            match client.get_connection() {
                Err(err) => {
                    if err.is_connection_refusal() {
                        sleep(millisecond);
                        retries += 1;
                        if retries > 100000 {
                            panic!(
                                "Tried to connect too many times, last error: {err}, logfile: {:?}",
                                server.log_file_contents()
                            );
                        }
                    } else {
                        panic!(
                            "Could not connect: {err}, logfile: {:?}",
                            server.log_file_contents()
                        );
                    }
                }
                Ok(x) => {
                    con = x;
                    break;
                }
            }
        }
        con.flushdb::<()>().unwrap();

        TestContext {
            server,
            client,
            protocol: use_protocol(),
        }
    }

    pub fn connection(&self) -> redis::Connection {
        self.client.get_connection().unwrap()
    }

    #[cfg(feature = "aio")]
    pub async fn async_connection(&self) -> RedisResult<redis::aio::MultiplexedConnection> {
        self.client.get_multiplexed_async_connection().await
    }

    #[cfg(feature = "aio")]
    pub async fn async_pubsub(&self) -> RedisResult<redis::aio::PubSub> {
        self.client.get_async_pubsub().await
    }

    pub fn stop_server(&mut self) {
        self.server.stop();
    }

    #[cfg(feature = "tokio-comp")]
    pub async fn multiplexed_async_connection_tokio(
        &self,
    ) -> RedisResult<redis::aio::MultiplexedConnection> {
        self.client.get_multiplexed_async_connection().await
    }

    #[cfg(all(feature = "aio", feature = "cache-aio"))]
    pub fn async_connection_with_cache(
        &self,
    ) -> impl Future<Output = redis::RedisResult<redis::aio::MultiplexedConnection>> {
        self.async_connection_with_cache_config(CacheConfig::default())
    }

    #[cfg(all(feature = "aio", feature = "cache-aio"))]
    pub fn async_connection_with_cache_config(
        &self,
        cache_config: CacheConfig,
    ) -> impl Future<Output = redis::RedisResult<redis::aio::MultiplexedConnection>> {
        use redis::AsyncConnectionConfig;

        let client = self.client.clone();
        async move {
            client
                .get_multiplexed_async_connection_with_config(
                    &AsyncConnectionConfig::new().set_cache_config(cache_config),
                )
                .await
        }
    }

    pub fn get_version(&self) -> Version {
        let mut conn = self.connection();
        get_version(&mut conn)
    }
}

fn encode_iter<W>(values: &[Value], writer: &mut W, prefix: &str) -> io::Result<()>
where
    W: io::Write,
{
    write!(writer, "{}{}\r\n", prefix, values.len())?;
    for val in values.iter() {
        encode_value(val, writer)?;
    }
    Ok(())
}
fn encode_map<W>(values: &[(Value, Value)], writer: &mut W, prefix: &str) -> io::Result<()>
where
    W: io::Write,
{
    write!(writer, "{}{}\r\n", prefix, values.len())?;
    for (k, v) in values.iter() {
        encode_value(k, writer)?;
        encode_value(v, writer)?;
    }
    Ok(())
}
pub fn encode_value<W>(value: &Value, writer: &mut W) -> io::Result<()>
where
    W: io::Write,
{
    #![allow(clippy::write_with_newline)]
    match *value {
        Value::Nil => write!(writer, "$-1\r\n"),
        Value::Int(val) => write!(writer, ":{val}\r\n"),
        Value::BulkString(ref val) => {
            write!(writer, "${}\r\n", val.len())?;
            writer.write_all(val)?;
            writer.write_all(b"\r\n")
        }
        Value::Array(ref values) => encode_iter(values, writer, "*"),
        Value::Okay => write!(writer, "+OK\r\n"),
        Value::SimpleString(ref s) => write!(writer, "+{s}\r\n"),
        Value::Map(ref values) => encode_map(values, writer, "%"),
        Value::Attribute {
            ref data,
            ref attributes,
        } => {
            encode_map(attributes, writer, "|")?;
            encode_value(data, writer)?;
            Ok(())
        }
        Value::Set(ref values) => encode_iter(values, writer, "~"),
        Value::Double(val) => write!(writer, ",{val}\r\n"),
        Value::Boolean(v) => {
            if v {
                write!(writer, "#t\r\n")
            } else {
                write!(writer, "#f\r\n")
            }
        }
        Value::VerbatimString {
            ref format,
            ref text,
        } => {
            // format is always 3 bytes
            write!(writer, "={}\r\n{}:{}\r\n", 3 + text.len(), format, text)
        }
        Value::BigNumber(ref val) => write!(writer, "({val}\r\n"),
        Value::Push { ref kind, ref data } => {
            write!(writer, ">{}\r\n+{kind}\r\n", data.len() + 1)?;
            for val in data.iter() {
                encode_value(val, writer)?;
            }
            Ok(())
        }
        Value::ServerError(ref err) => match err.details() {
            Some(details) => write!(writer, "-{} {details}\r\n", err.code()),
            None => write!(writer, "-{}\r\n", err.code()),
        },
    }
}

pub type Version = (u16, u16, u16);

pub fn parse_version(info: InfoDict) -> Version {
    let version: String = info.get("redis_version").unwrap();
    let versions: Vec<u16> = version
        .split('.')
        .map(|version| version.parse::<u16>().unwrap())
        .collect();
    assert_eq!(versions.len(), 3);
    (versions[0], versions[1], versions[2])
}

fn get_version(conn: &mut impl redis::ConnectionLike) -> Version {
    let info: InfoDict = redis::Cmd::new().arg("INFO").query(conn).unwrap();
    parse_version(info)
}

pub fn is_major_version(expected_version: u16, version: Version) -> bool {
    expected_version <= version.0
}

pub fn is_version(expected_major_minor: (u16, u16), version: Version) -> bool {
    expected_major_minor.0 < version.0
        || (expected_major_minor.0 == version.0 && expected_major_minor.1 <= version.1)
}

#[cfg(feature = "tls-rustls")]
fn load_certs_from_file(tls_file_paths: &TlsFilePaths) -> TlsCertificates {
    let ca_file = File::open(&tls_file_paths.ca_crt).expect("Cannot open CA cert file");
    let mut root_cert_vec = Vec::new();
    BufReader::new(ca_file)
        .read_to_end(&mut root_cert_vec)
        .expect("Unable to read CA cert file");

    let cert_file = File::open(&tls_file_paths.redis_crt).expect("cannot open private cert file");
    let mut client_cert_vec = Vec::new();
    BufReader::new(cert_file)
        .read_to_end(&mut client_cert_vec)
        .expect("Unable to read client cert file");

    let key_file = File::open(&tls_file_paths.redis_key).expect("Cannot open private key file");
    let mut client_key_vec = Vec::new();
    BufReader::new(key_file)
        .read_to_end(&mut client_key_vec)
        .expect("Unable to read client key file");

    TlsCertificates {
        client_tls: Some(ClientTlsConfig {
            client_cert: client_cert_vec,
            client_key: client_key_vec,
        }),
        root_cert: Some(root_cert_vec),
    }
}

#[cfg(feature = "tls-rustls")]
pub(crate) fn build_single_client<T: redis::IntoConnectionInfo>(
    connection_info: T,
    tls_file_params: &Option<TlsFilePaths>,
    mtls_enabled: bool,
) -> RedisResult<redis::Client> {
    if mtls_enabled && tls_file_params.is_some() {
        redis::Client::build_with_tls(
            connection_info,
            load_certs_from_file(
                tls_file_params
                    .as_ref()
                    .expect("Expected certificates when `tls-rustls` feature is enabled"),
            ),
        )
    } else {
        redis::Client::open(connection_info)
    }
}

#[cfg(not(feature = "tls-rustls"))]
pub(crate) fn build_single_client<T: redis::IntoConnectionInfo>(
    connection_info: T,
    _tls_file_params: &Option<TlsFilePaths>,
    _mtls_enabled: bool,
) -> RedisResult<redis::Client> {
    redis::Client::open(connection_info)
}

#[cfg(feature = "tls-rustls")]
pub(crate) mod mtls_test {
    use super::*;
    use redis::{cluster::ClusterClient, ConnectionInfo, RedisError};

    fn clean_node_info(nodes: &[ConnectionInfo]) -> Vec<ConnectionInfo> {
        let nodes = nodes
            .iter()
            .map(|node| match node {
                ConnectionInfo {
                    addr: redis::ConnectionAddr::TcpTls { host, port, .. },
                    redis,
                } => ConnectionInfo {
                    addr: redis::ConnectionAddr::TcpTls {
                        host: host.to_owned(),
                        port: *port,
                        insecure: false,
                        tls_params: None,
                    },
                    redis: redis.clone(),
                },
                _ => node.clone(),
            })
            .collect();
        nodes
    }

    pub(crate) fn create_cluster_client_from_cluster(
        cluster: &TestClusterContext,
        mtls_enabled: bool,
    ) -> Result<ClusterClient, RedisError> {
        let server = cluster
            .cluster
            .servers
            .first()
            .expect("Expected at least 1 server");
        let tls_paths = server.tls_paths.as_ref();
        let nodes = clean_node_info(&cluster.nodes);
        let builder = redis::cluster::ClusterClientBuilder::new(nodes);
        if let Some(tls_paths) = tls_paths {
            // server-side TLS available
            if mtls_enabled {
                builder.certs(load_certs_from_file(tls_paths))
            } else {
                builder
            }
        } else {
            // server-side TLS NOT available
            builder
        }
        .build()
    }
}

pub fn build_simple_pipeline_for_invalidation() -> Pipeline {
    let mut pipe = redis::pipe();
    pipe.cmd("GET")
        .arg("key_1")
        .ignore()
        .cmd("SET")
        .arg("key_1")
        .arg(42)
        .ignore();
    pipe
}

#[cfg(feature = "aio")]
pub async fn kill_client_async(
    conn_to_kill: &mut impl aio::ConnectionLike,
    client: &redis::Client,
) -> RedisResult<()> {
    let info: String = cmd("CLIENT").arg("INFO").query_async(conn_to_kill).await?;
    let id = info.split_once(' ').unwrap().0;
    assert!(id.contains("id="));
    let client_to_kill_id = id.split_once("id=").unwrap().1;

    let mut killer_conn = client.get_multiplexed_async_connection().await.unwrap();
    let () = cmd("CLIENT")
        .arg("KILL")
        .arg("ID")
        .arg(client_to_kill_id)
        .query_async(&mut killer_conn)
        .await
        .unwrap();

    Ok(())
}

fn spawn<T>(fut: impl std::future::Future<Output = T> + Send + Sync + 'static)
where
    T: Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(tokio_runtime) => {
            tokio_runtime.spawn(fut);
        }
        #[cfg(feature = "async-std-comp")]
        Err(_) => {
            async_std::task::spawn(fut);
        }
        #[cfg(not(feature = "async-std-comp"))]
        Err(_) => unreachable!(),
    }
}
