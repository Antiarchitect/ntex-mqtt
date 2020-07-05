use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::future::{ok, Ready};
use futures::{SinkExt, StreamExt, TryFutureExt};
use ntex::channel::mpsc;
use ntex::codec::{AsyncRead, AsyncWrite, Framed};
use ntex::service::{apply, apply_fn, fn_factory, unit_config};
use ntex::service::{IntoServiceFactory, Service, ServiceFactory};
use ntex::util::framed::DispatcherError;
use ntex::util::timeout::{Timeout, TimeoutError};

use crate::error::MqttError;
use crate::handshake::{Handshake, HandshakeResult};
use crate::service::{FactoryBuilder, FactoryBuilder2};

use super::codec as mqtt;
use super::connect::{Connect, ConnectAck};
use super::control::{ControlPacket, ControlResult};
use super::default::DefaultControlService;
use super::dispatcher::factory;
use super::publish::Publish;
use super::sink::MqttSink;
use super::Session;

/// Mqtt Server
pub struct MqttServer<Io, St, C: ServiceFactory, Cn: ServiceFactory, P: ServiceFactory> {
    connect: C,
    control: Cn,
    publish: P,
    max_size: u32,
    inflight: usize,
    handshake_timeout: usize,
    disconnect_timeout: usize,
    _t: PhantomData<(Io, St)>,
}

impl<Io, St, C>
    MqttServer<
        Io,
        St,
        C,
        DefaultControlService<St, C::Error>,
        DefaultPublishService<St, C::Error>,
    >
where
    St: 'static,
    C: ServiceFactory<Config = (), Request = Connect<Io>, Response = ConnectAck<Io, St>>
        + 'static,
    C::Error: fmt::Debug,
{
    /// Create server factory and provide connect service
    pub fn new<F>(connect: F) -> Self
    where
        F: IntoServiceFactory<C>,
    {
        MqttServer {
            connect: connect.into_factory(),
            control: DefaultControlService::default(),
            publish: DefaultPublishService::default(),
            max_size: 0,
            inflight: 15,
            handshake_timeout: 0,
            disconnect_timeout: 3000,
            _t: PhantomData,
        }
    }
}

impl<Io, St, C, Cn, P> MqttServer<Io, St, C, Cn, P>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
    St: 'static,
    C: ServiceFactory<Config = (), Request = Connect<Io>, Response = ConnectAck<Io, St>>
        + 'static,
    Cn: ServiceFactory<Config = Session<St>, Request = ControlPacket, Response = ControlResult>
        + 'static,
    P: ServiceFactory<Config = Session<St>, Request = Publish, Response = ()> + 'static,
    C::Error: From<Cn::Error>
        + From<Cn::InitError>
        + From<P::Error>
        + From<P::InitError>
        + fmt::Debug,
{
    /// Set handshake timeout in millis.
    ///
    /// Handshake includes `connect` packet and response `connect-ack`.
    /// By default handshake timeuot is disabled.
    pub fn handshake_timeout(mut self, timeout: usize) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Set server connection disconnect timeout in milliseconds.
    ///
    /// Defines a timeout for disconnect connection. If a disconnect procedure does not complete
    /// within this time, the connection get dropped.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default disconnect timeout is set to 3 seconds.
    pub fn disconnect_timeout(mut self, val: usize) -> Self {
        self.disconnect_timeout = val;
        self
    }

    /// Set max inbound frame size.
    ///
    /// If max size is set to `0`, size is unlimited.
    /// By default max size is set to `0`
    pub fn max_size(mut self, size: u32) -> Self {
        self.max_size = size;
        self
    }

    /// Number of in-flight concurrent messages.
    ///
    /// By default in-flight is set to 15 messages
    pub fn inflight(mut self, val: usize) -> Self {
        self.inflight = val;
        self
    }

    /// Service to handle control packets
    pub fn control<F, Srv>(self, service: F) -> MqttServer<Io, St, C, Srv, P>
    where
        F: IntoServiceFactory<Srv>,
        Srv: ServiceFactory<
                Config = Session<St>,
                Request = ControlPacket,
                Response = ControlResult,
            > + 'static,
        C::Error: From<Srv::Error> + From<Srv::InitError>,
    {
        MqttServer {
            connect: self.connect,
            publish: self.publish,
            control: service.into_factory(),
            max_size: self.max_size,
            inflight: self.inflight,
            handshake_timeout: self.handshake_timeout,
            disconnect_timeout: self.disconnect_timeout,
            _t: PhantomData,
        }
    }

    /// Set service to handle publish packets and create mqtt server factory
    pub fn publish<F, Srv>(self, publish: F) -> MqttServer<Io, St, C, Cn, Srv>
    where
        F: IntoServiceFactory<Srv> + 'static,
        Srv: ServiceFactory<Config = Session<St>, Request = Publish, Response = ()> + 'static,
        C::Error: From<Srv::Error> + From<Srv::InitError> + fmt::Debug,
    {
        MqttServer {
            connect: self.connect,
            publish: publish.into_factory(),
            control: self.control,
            max_size: self.max_size,
            inflight: self.inflight,
            handshake_timeout: self.handshake_timeout,
            disconnect_timeout: self.disconnect_timeout,
            _t: PhantomData,
        }
    }

    /// Set service to handle publish packets and create mqtt server factory
    pub fn finish(
        self,
    ) -> impl ServiceFactory<Config = (), Request = Io, Response = (), Error = MqttError<C::Error>>
    {
        let connect = self.connect;
        let max_size = self.max_size;
        let handshake_timeout = self.handshake_timeout;
        let disconnect_timeout = self.disconnect_timeout;
        let publish = self
            .publish
            .into_factory()
            .map_err(|e| MqttError::Service(e.into()))
            .map_init_err(|e| MqttError::Service(e.into()));
        let control = self
            .control
            .map_err(|e| MqttError::Service(e.into()))
            .map_init_err(|e| MqttError::Service(e.into()));

        unit_config(
            FactoryBuilder::new(handshake_service_factory(
                connect,
                max_size,
                self.inflight,
                handshake_timeout,
            ))
            .disconnect_timeout(disconnect_timeout)
            .build(factory(publish, control))
            .map_err(|e| match e {
                DispatcherError::Service(e) => e,
                DispatcherError::Encoder(e) => MqttError::Encode(e),
                DispatcherError::Decoder(e) => MqttError::Decode(e),
            }),
        )
    }

    /// Set service to handle publish packets and create mqtt server factory
    pub(crate) fn inner_finish(
        self,
    ) -> impl ServiceFactory<
        Config = (),
        Request = Framed<Io, mqtt::Codec>,
        Response = (),
        Error = MqttError<C::Error>,
        InitError = C::InitError,
    > {
        let connect = self.connect;
        let max_size = self.max_size;
        let handshake_timeout = self.handshake_timeout;
        let disconnect_timeout = self.disconnect_timeout;
        let publish = self
            .publish
            .into_factory()
            .map_err(|e| MqttError::Service(e.into()))
            .map_init_err(|e| MqttError::Service(e.into()));
        let control = self
            .control
            .map_err(|e| MqttError::Service(e.into()))
            .map_init_err(|e| MqttError::Service(e.into()));

        unit_config(
            FactoryBuilder2::new(handshake_service_factory2(
                connect,
                max_size,
                self.inflight,
                handshake_timeout,
            ))
            .disconnect_timeout(disconnect_timeout)
            .build(factory(publish, control))
            .map_err(|e| match e {
                DispatcherError::Service(e) => e,
                DispatcherError::Encoder(e) => MqttError::Encode(e),
                DispatcherError::Decoder(e) => MqttError::Decode(e),
            }),
        )
    }
}

fn handshake_service_factory<Io, St, C>(
    factory: C,
    max_size: u32,
    inflight: usize,
    handshake_timeout: usize,
) -> impl ServiceFactory<
    Config = (),
    Request = Handshake<Io, mqtt::Codec>,
    Response = HandshakeResult<Io, Session<St>, mqtt::Codec, mpsc::Receiver<mqtt::Packet>>,
    Error = MqttError<C::Error>,
>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    C: ServiceFactory<Config = (), Request = Connect<Io>, Response = ConnectAck<Io, St>>,
    C::Error: fmt::Debug,
{
    apply(
        Timeout::new(Duration::from_millis(handshake_timeout as u64)),
        fn_factory(move || {
            factory.new_service(()).map_ok(move |service| {
                let service = Rc::new(service.map_err(MqttError::Service));
                apply_fn(service, move |conn: Handshake<Io, mqtt::Codec>, service| {
                    handshake(
                        conn.codec(mqtt::Codec::new()),
                        service.clone(),
                        max_size,
                        inflight,
                    )
                })
            })
        }),
    )
    .map_err(|e| match e {
        TimeoutError::Service(e) => e,
        TimeoutError::Timeout => MqttError::HandshakeTimeout,
    })
}

fn handshake_service_factory2<Io, St, C>(
    factory: C,
    max_size: u32,
    inflight: usize,
    handshake_timeout: usize,
) -> impl ServiceFactory<
    Config = (),
    Request = HandshakeResult<Io, (), mqtt::Codec, mpsc::Receiver<mqtt::Packet>>,
    Response = HandshakeResult<Io, Session<St>, mqtt::Codec, mpsc::Receiver<mqtt::Packet>>,
    Error = MqttError<C::Error>,
    InitError = C::InitError,
>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    C: ServiceFactory<Config = (), Request = Connect<Io>, Response = ConnectAck<Io, St>>,
    C::Error: fmt::Debug,
{
    apply(
        Timeout::new(Duration::from_millis(handshake_timeout as u64)),
        fn_factory(move || {
            factory.new_service(()).map_ok(move |service| {
                let service = Rc::new(service.map_err(MqttError::Service));
                apply_fn(service, move |conn, service| {
                    handshake(conn, service.clone(), max_size, inflight)
                })
            })
        }),
    )
    .map_err(|e| match e {
        TimeoutError::Service(e) => e,
        TimeoutError::Timeout => MqttError::HandshakeTimeout,
    })
}

async fn handshake<Io, S, St, E>(
    mut framed: HandshakeResult<Io, (), mqtt::Codec, mpsc::Receiver<mqtt::Packet>>,
    service: S,
    max_size: u32,
    inflight: usize,
) -> Result<HandshakeResult<Io, Session<St>, mqtt::Codec, mpsc::Receiver<mqtt::Packet>>, S::Error>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    S: Service<Request = Connect<Io>, Response = ConnectAck<Io, St>, Error = MqttError<E>>,
{
    log::trace!("Starting mqtt handshake");

    framed.get_codec_mut().set_max_size(max_size);

    // read first packet
    let packet = framed
        .next()
        .await
        .ok_or_else(|| {
            log::trace!("Server mqtt is disconnected during handshake");
            MqttError::Disconnected
        })
        .and_then(|res| {
            res.map_err(|e| {
                log::trace!("Error is received during mqtt handshake: {:?}", e);
                MqttError::Decode(e)
            })
        })?;

    match packet {
        mqtt::Packet::Connect(connect) => {
            let (tx, rx) = mpsc::channel();
            let sink = MqttSink::new(tx);

            // authenticate mqtt connection
            let mut ack = service
                .call(Connect::new(connect, framed, sink, inflight))
                .await?;

            match ack.session {
                Some(session) => {
                    log::trace!(
                        "Sending: {:#?}",
                        mqtt::Packet::ConnectAck {
                            session_present: ack.session_present,
                            return_code: mqtt::ConnectAckReason::ConnectionAccepted,
                        }
                    );
                    let sink = ack.sink;
                    ack.io
                        .send(mqtt::Packet::ConnectAck {
                            session_present: ack.session_present,
                            return_code: mqtt::ConnectAckReason::ConnectionAccepted,
                        })
                        .await?;

                    Ok(ack.io.out(rx).state(Session::new(
                        session,
                        sink,
                        ack.keep_alive,
                        ack.inflight,
                    )))
                }
                None => {
                    log::trace!(
                        "Sending: {:#?}",
                        mqtt::Packet::ConnectAck {
                            session_present: false,
                            return_code: ack.return_code,
                        }
                    );

                    ack.io
                        .send(mqtt::Packet::ConnectAck {
                            session_present: false,
                            return_code: ack.return_code,
                        })
                        .await?;
                    Err(MqttError::Disconnected)
                }
            }
        }
        packet => {
            log::info!("MQTT-3.1.0-1: Expected CONNECT packet, received {}", 1);
            Err(MqttError::Unexpected(
                packet.packet_type(),
                "MQTT-3.1.0-1: Expected CONNECT packet",
            ))
        }
    }
}

pub struct DefaultPublishService<St, Err> {
    _t: PhantomData<(St, Err)>,
}

impl<St, Err> Default for DefaultPublishService<St, Err> {
    fn default() -> Self {
        Self { _t: PhantomData }
    }
}

impl<St, Err> ServiceFactory for DefaultPublishService<St, Err> {
    type Config = Session<St>;
    type Request = Publish;
    type Response = ();
    type Error = Err;
    type Service = DefaultPublishService<St, Err>;
    type InitError = Err;
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: Session<St>) -> Self::Future {
        ok(DefaultPublishService { _t: PhantomData })
    }
}

impl<St, Err> Service for DefaultPublishService<St, Err> {
    type Request = Publish;
    type Response = ();
    type Error = Err;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&self, _: Publish) -> Self::Future {
        log::warn!("Publish service is disabled");
        ok(())
    }
}