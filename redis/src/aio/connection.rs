#![allow(deprecated)]

#[cfg(feature = "async-std-comp")]
use super::async_std;
#[cfg(any(feature = "tokio-comp", feature = "async-std-comp"))]
use super::DefaultAsyncDNSResolver;
use super::{setup_connection, AsyncStream, RedisRuntime};
use super::{AsyncDNSResolver, ConnectionLike};
use crate::cmd::{cmd, Cmd};
use crate::connection::{
    resp2_is_pub_sub_state_cleared, resp3_is_pub_sub_state_cleared, ConnectionAddr, ConnectionInfo,
    Msg, RedisConnectionInfo,
};
use crate::io::tcp::TcpSettings;
#[cfg(feature = "aio")]
use crate::parser::ValueCodec;
use crate::types::{FromRedisValue, RedisFuture, RedisResult, Value};
use crate::{from_owned_redis_value, ProtocolVersion, ToRedisArgs};
use ::tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use combine::{parser::combinator::AnySendSyncPartialState, stream::PointerOffset};
use futures_util::future::select_ok;
use futures_util::{
    future::FutureExt,
    stream::{Stream, StreamExt},
};
use std::pin::Pin;
#[cfg(feature = "aio")]
use tokio_util::codec::Decoder;

/// Represents a stateful redis TCP connection.
#[deprecated(note = "aio::Connection is deprecated. Use aio::MultiplexedConnection instead.")]
pub struct Connection<C = Pin<Box<dyn AsyncStream + Send + Sync>>> {
    con: C,
    buf: Vec<u8>,
    decoder: combine::stream::Decoder<AnySendSyncPartialState, PointerOffset<[u8]>>,
    db: i64,

    // Flag indicating whether the connection was left in the PubSub state after dropping `PubSub`.
    //
    // This flag is checked when attempting to send a command, and if it's raised, we attempt to
    // exit the pubsub state before executing the new request.
    pubsub: bool,

    // Field indicating which protocol to use for server communications.
    protocol: ProtocolVersion,
}

fn assert_sync<T: Sync>() {}

#[allow(unused)]
fn test() {
    assert_sync::<Connection>();
}

#[cfg(any(feature = "tokio-comp", feature = "async-std-comp"))]
impl<C> Connection<C> {
    pub(crate) fn map<D>(self, f: impl FnOnce(C) -> D) -> Connection<D> {
        let Self {
            con,
            buf,
            decoder,
            db,
            pubsub,
            protocol,
        } = self;
        Connection {
            con: f(con),
            buf,
            decoder,
            db,
            pubsub,
            protocol,
        }
    }
}

impl<C> Connection<C>
where
    C: Unpin + AsyncRead + AsyncWrite + Send,
{
    /// Constructs a new `Connection` out of a `AsyncRead + AsyncWrite` object
    /// and a `RedisConnectionInfo`
    pub async fn new(connection_info: &RedisConnectionInfo, con: C) -> RedisResult<Self> {
        let mut rv = Connection {
            con,
            buf: Vec::new(),
            decoder: combine::stream::Decoder::new(),
            db: connection_info.db,
            pubsub: false,
            protocol: connection_info.protocol,
        };
        setup_connection(
            connection_info,
            &mut rv,
            #[cfg(feature = "cache-aio")]
            None,
        )
        .await?;
        Ok(rv)
    }

    /// Converts this [`Connection`] into [`PubSub`].
    #[allow(rustdoc::private_intra_doc_links)]
    #[deprecated(note = "aio::Connection is deprecated. Use [Client::get_async_pubsub] instead")]
    pub fn into_pubsub(self) -> PubSub<C> {
        PubSub::new(self)
    }

    /// Converts this [`Connection`] into [`Monitor`]
    #[deprecated(note = "aio::Connection is deprecated. Use [Client::get_async_pubsub] instead")]
    pub fn into_monitor(self) -> Monitor<C> {
        Monitor::new(self)
    }

    /// Fetches a single response from the connection.
    async fn read_response(&mut self) -> RedisResult<Value> {
        crate::parser::parse_redis_value_async(&mut self.decoder, &mut self.con).await
    }

    /// Brings [`Connection`] out of `PubSub` mode.
    ///
    /// This will unsubscribe this [`Connection`] from all subscriptions.
    ///
    /// If this function returns error then on all command send tries will be performed attempt
    /// to exit from `PubSub` mode until it will be successful.
    async fn exit_pubsub(&mut self) -> RedisResult<()> {
        let res = self.clear_active_subscriptions().await;
        if res.is_ok() {
            self.pubsub = false;
        } else {
            // Raise the pubsub flag to indicate the connection is "stuck" in that state.
            self.pubsub = true;
        }

        res
    }

    /// Get the inner connection out of a PubSub
    ///
    /// Any active subscriptions are unsubscribed. In the event of an error, the connection is
    /// dropped.
    async fn clear_active_subscriptions(&mut self) -> RedisResult<()> {
        // Responses to unsubscribe commands return in a 3-tuple with values
        // ("unsubscribe" or "punsubscribe", name of subscription removed, count of remaining subs).
        // The "count of remaining subs" includes both pattern subscriptions and non pattern
        // subscriptions. Thus, to accurately drain all unsubscribe messages received from the
        // server, both commands need to be executed at once.
        {
            // Prepare both unsubscribe commands
            let unsubscribe = crate::Pipeline::new()
                .add_command(cmd("UNSUBSCRIBE"))
                .add_command(cmd("PUNSUBSCRIBE"))
                .get_packed_pipeline();

            // Execute commands
            self.con.write_all(&unsubscribe).await?;
        }

        // Receive responses
        //
        // There will be at minimum two responses - 1 for each of punsubscribe and unsubscribe
        // commands. There may be more responses if there are active subscriptions. In this case,
        // messages are received until the _subscription count_ in the responses reach zero.
        let mut received_unsub = false;
        let mut received_punsub = false;
        if self.protocol != ProtocolVersion::RESP2 {
            while let Value::Push { kind, data } =
                from_owned_redis_value(self.read_response().await?)?
            {
                if data.len() >= 2 {
                    if let Value::Int(num) = data[1] {
                        if resp3_is_pub_sub_state_cleared(
                            &mut received_unsub,
                            &mut received_punsub,
                            &kind,
                            num as isize,
                        ) {
                            break;
                        }
                    }
                }
            }
        } else {
            loop {
                let res: (Vec<u8>, (), isize) =
                    from_owned_redis_value(self.read_response().await?)?;
                if resp2_is_pub_sub_state_cleared(
                    &mut received_unsub,
                    &mut received_punsub,
                    &res.0,
                    res.2,
                ) {
                    break;
                }
            }
        }

        // Finally, the connection is back in its normal state since all subscriptions were
        // cancelled *and* all unsubscribe messages were received.
        Ok(())
    }
}

#[cfg(feature = "async-std-comp")]
#[cfg_attr(docsrs, doc(cfg(feature = "async-std-comp")))]
impl<C> Connection<async_std::AsyncStdWrapped<C>>
where
    C: Unpin + ::async_std::io::Read + ::async_std::io::Write + Send,
{
    /// Constructs a new `Connection` out of a `async_std::io::AsyncRead + async_std::io::AsyncWrite` object
    /// and a `RedisConnectionInfo`
    pub async fn new_async_std(connection_info: &RedisConnectionInfo, con: C) -> RedisResult<Self> {
        Connection::new(connection_info, async_std::AsyncStdWrapped::new(con)).await
    }
}

#[cfg(any(feature = "tokio-comp", feature = "async-std-comp"))]
pub(crate) async fn connect<C>(connection_info: &ConnectionInfo) -> RedisResult<Connection<C>>
where
    C: Unpin + RedisRuntime + AsyncRead + AsyncWrite + Send,
{
    let con = connect_simple::<C>(
        connection_info,
        &DefaultAsyncDNSResolver,
        &TcpSettings::default(),
    )
    .await?;
    Connection::new(&connection_info.redis, con).await
}

impl<C> ConnectionLike for Connection<C>
where
    C: Unpin + AsyncRead + AsyncWrite + Send,
{
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        (async move {
            if self.pubsub {
                self.exit_pubsub().await?;
            }
            self.buf.clear();
            cmd.write_packed_command(&mut self.buf);
            self.con.write_all(&self.buf).await?;
            if cmd.is_no_response() {
                return Ok(Value::Nil);
            }
            loop {
                match self.read_response().await? {
                    Value::Push { .. } => continue,
                    val => return Ok(val),
                }
            }
        })
        .boxed()
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a crate::Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        (async move {
            if self.pubsub {
                self.exit_pubsub().await?;
            }

            self.buf.clear();
            cmd.write_packed_pipeline(&mut self.buf);
            self.con.write_all(&self.buf).await?;

            let mut first_err = None;

            for _ in 0..offset {
                let response = self.read_response().await;
                match response {
                    Ok(Value::ServerError(err)) => {
                        if first_err.is_none() {
                            first_err = Some(err.into());
                        }
                    }
                    Err(err) => {
                        if first_err.is_none() {
                            first_err = Some(err);
                        }
                    }
                    _ => {}
                }
            }

            let mut rv = Vec::with_capacity(count);
            let mut count = count;
            let mut idx = 0;
            while idx < count {
                let response = self.read_response().await;
                match response {
                    Ok(item) => {
                        // RESP3 can insert push data between command replies
                        if let Value::Push { .. } = item {
                            // if that is the case we have to extend the loop and handle push data
                            count += 1;
                        } else {
                            rv.push(item);
                        }
                    }
                    Err(err) => {
                        if first_err.is_none() {
                            first_err = Some(err);
                        }
                    }
                }
                idx += 1;
            }

            if let Some(err) = first_err {
                Err(err)
            } else {
                Ok(rv)
            }
        })
        .boxed()
    }

    fn get_db(&self) -> i64 {
        self.db
    }
}

/// Represents a `PubSub` connection.
pub struct PubSub<C = Pin<Box<dyn AsyncStream + Send + Sync>>>(Connection<C>);

/// Represents a `Monitor` connection.
pub struct Monitor<C = Pin<Box<dyn AsyncStream + Send + Sync>>>(Connection<C>);

impl<C> PubSub<C>
where
    C: Unpin + AsyncRead + AsyncWrite + Send,
{
    fn new(con: Connection<C>) -> Self {
        Self(con)
    }

    /// Subscribes to a new channel(s).
    pub async fn subscribe<T: ToRedisArgs>(&mut self, channel: T) -> RedisResult<()> {
        let mut cmd = cmd("SUBSCRIBE");
        cmd.arg(channel);
        if self.0.protocol != ProtocolVersion::RESP2 {
            cmd.set_no_response(true);
        }
        cmd.query_async(&mut self.0).await
    }

    /// Subscribes to new channel(s) with pattern(s).
    pub async fn psubscribe<T: ToRedisArgs>(&mut self, pchannel: T) -> RedisResult<()> {
        let mut cmd = cmd("PSUBSCRIBE");
        cmd.arg(pchannel);
        if self.0.protocol != ProtocolVersion::RESP2 {
            cmd.set_no_response(true);
        }
        cmd.query_async(&mut self.0).await
    }

    /// Unsubscribes from a channel.
    pub async fn unsubscribe<T: ToRedisArgs>(&mut self, channel: T) -> RedisResult<()> {
        let mut cmd = cmd("UNSUBSCRIBE");
        cmd.arg(channel);
        if self.0.protocol != ProtocolVersion::RESP2 {
            cmd.set_no_response(true);
        }
        cmd.query_async(&mut self.0).await
    }

    /// Unsubscribes from channel pattern(s).
    pub async fn punsubscribe<T: ToRedisArgs>(&mut self, pchannel: T) -> RedisResult<()> {
        let mut cmd = cmd("PUNSUBSCRIBE");
        cmd.arg(pchannel);
        if self.0.protocol != ProtocolVersion::RESP2 {
            cmd.set_no_response(true);
        }
        cmd.query_async(&mut self.0).await
    }

    /// Returns [`Stream`] of [`Msg`]s from this [`PubSub`]s subscriptions.
    ///
    /// The message itself is still generic and can be converted into an appropriate type through
    /// the helper methods on it.
    pub fn on_message(&mut self) -> impl Stream<Item = Msg> + '_ {
        ValueCodec::default()
            .framed(&mut self.0.con)
            .filter_map(|msg| Box::pin(async move { Msg::from_owned_value(msg.ok()?) }))
    }

    /// Returns [`Stream`] of [`Msg`]s from this [`PubSub`]s subscriptions consuming it.
    ///
    /// The message itself is still generic and can be converted into an appropriate type through
    /// the helper methods on it.
    /// This can be useful in cases where the stream needs to be returned or held by something other
    /// than the [`PubSub`].
    pub fn into_on_message(self) -> impl Stream<Item = Msg> {
        ValueCodec::default()
            .framed(self.0.con)
            .filter_map(|msg| Box::pin(async move { Msg::from_owned_value(msg.ok()?) }))
    }

    /// Exits from `PubSub` mode and converts [`PubSub`] into [`Connection`].
    #[deprecated(note = "aio::Connection is deprecated")]
    pub async fn into_connection(mut self) -> Connection<C> {
        self.0.exit_pubsub().await.ok();

        self.0
    }
}

impl<C> Monitor<C>
where
    C: Unpin + AsyncRead + AsyncWrite + Send,
{
    /// Create a [`Monitor`] from a [`Connection`]
    pub fn new(con: Connection<C>) -> Self {
        Self(con)
    }

    /// Deliver the MONITOR command to this [`Monitor`]ing wrapper.
    pub async fn monitor(&mut self) -> RedisResult<()> {
        cmd("MONITOR").query_async(&mut self.0).await
    }

    /// Returns [`Stream`] of [`FromRedisValue`] values from this [`Monitor`]ing connection
    pub fn on_message<T: FromRedisValue>(&mut self) -> impl Stream<Item = T> + '_ {
        ValueCodec::default()
            .framed(&mut self.0.con)
            .filter_map(|value| {
                Box::pin(async move { T::from_owned_redis_value(value.ok()?).ok() })
            })
    }

    /// Returns [`Stream`] of [`FromRedisValue`] values from this [`Monitor`]ing connection
    pub fn into_on_message<T: FromRedisValue>(self) -> impl Stream<Item = T> {
        ValueCodec::default()
            .framed(self.0.con)
            .filter_map(|value| {
                Box::pin(async move { T::from_owned_redis_value(value.ok()?).ok() })
            })
    }
}

pub(crate) async fn connect_simple<T: RedisRuntime>(
    connection_info: &ConnectionInfo,
    dns_resolver: &dyn AsyncDNSResolver,
    tcp_settings: &TcpSettings,
) -> RedisResult<T> {
    Ok(match connection_info.addr {
        ConnectionAddr::Tcp(ref host, port) => {
            let socket_addrs = dns_resolver.resolve(host, port).await?;
            select_ok(socket_addrs.map(|addr| Box::pin(<T>::connect_tcp(addr, tcp_settings))))
                .await?
                .0
        }

        #[cfg(any(feature = "tls-native-tls", feature = "tls-rustls"))]
        ConnectionAddr::TcpTls {
            ref host,
            port,
            insecure,
            ref tls_params,
        } => {
            let socket_addrs = dns_resolver.resolve(host, port).await?;
            select_ok(socket_addrs.map(|socket_addr| {
                Box::pin(<T>::connect_tcp_tls(
                    host,
                    socket_addr,
                    insecure,
                    tls_params,
                    tcp_settings,
                ))
            }))
            .await?
            .0
        }

        #[cfg(not(any(feature = "tls-native-tls", feature = "tls-rustls")))]
        ConnectionAddr::TcpTls { .. } => {
            fail!((
                crate::types::ErrorKind::InvalidClientConfig,
                "Cannot connect to TCP with TLS without the tls feature"
            ));
        }

        #[cfg(unix)]
        ConnectionAddr::Unix(ref path) => <T>::connect_unix(path).await?,

        #[cfg(not(unix))]
        ConnectionAddr::Unix(_) => {
            fail!((
                crate::types::ErrorKind::InvalidClientConfig,
                "Cannot connect to unix sockets \
                 on this platform",
            ))
        }
    })
}
