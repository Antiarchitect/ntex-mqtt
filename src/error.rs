use derive_more::{Display, From};
use ntex::util::Either;
use std::{error, io};

/// Errors which can occur when attempting to handle mqtt connection.
#[derive(Debug)]
pub enum MqttError<E> {
    /// Publish handler service error
    Service(E),
    /// Protocol error
    Protocol(ProtocolError),
    /// Handshake timeout
    HandshakeTimeout,
    /// Peer disconnect
    Disconnected(Option<io::Error>),
    /// Server error
    ServerError(&'static str),
}

/// Protocol level errors
#[derive(Debug, Display, From)]
pub enum ProtocolError {
    /// Mqtt parse error
    #[display(fmt = "Decode error: {:?}", _0)]
    Decode(DecodeError),
    /// Mqtt encode error
    #[display(fmt = "Encode error: {:?}", _0)]
    Encode(EncodeError),
    /// Unexpected packet
    #[display(fmt = "Unexpected packet {:?}, {}", _0, _1)]
    Unexpected(u8, &'static str),
    /// Packet id of publish ack packet does not match of send publish packet
    #[display(fmt = "Packet id of publish ack packet does not match of send publish packet")]
    PacketIdMismatch,
    /// Topic alias is greater than max topic alias
    #[display(fmt = "Topic alias is greater than max topic alias")]
    MaxTopicAlias,
    /// Number of in-flight messages exceeded
    #[display(fmt = "Number of in-flight messages exceeded")]
    ReceiveMaximumExceeded,
    /// Unknown topic alias
    #[display(fmt = "Unknown topic alias")]
    UnknownTopicAlias,
    /// Keep alive timeout
    #[display(fmt = "Keep alive timeout")]
    KeepAliveTimeout,
}

impl error::Error for ProtocolError {}

impl<E> From<ProtocolError> for MqttError<E> {
    fn from(err: ProtocolError) -> Self {
        MqttError::Protocol(err)
    }
}

impl<E> From<Either<DecodeError, io::Error>> for MqttError<E> {
    fn from(err: Either<DecodeError, io::Error>) -> Self {
        match err {
            Either::Left(err) => MqttError::Protocol(ProtocolError::Decode(err)),
            Either::Right(err) => MqttError::Disconnected(Some(err)),
        }
    }
}

impl<E> From<Either<EncodeError, io::Error>> for MqttError<E> {
    fn from(err: Either<EncodeError, io::Error>) -> Self {
        match err {
            Either::Left(err) => MqttError::Protocol(ProtocolError::Encode(err)),
            Either::Right(err) => MqttError::Disconnected(Some(err)),
        }
    }
}

#[derive(Debug, Display, From)]
pub enum DecodeError {
    InvalidProtocol,
    InvalidLength,
    MalformedPacket,
    UnsupportedProtocolLevel,
    ConnectReservedFlagSet,
    ConnAckReservedFlagSet,
    InvalidClientId,
    UnsupportedPacketType,
    // MQTT v3 only
    PacketIdRequired,
    MaxSizeExceeded,
    Utf8Error(std::str::Utf8Error),
}

impl error::Error for DecodeError {}

#[derive(Copy, Clone, Debug, Display, PartialEq, Eq, Hash)]
pub enum EncodeError {
    InvalidLength,
    MalformedPacket,
    PacketIdRequired,
    UnsupportedVersion,
}

impl error::Error for EncodeError {}

impl PartialEq for DecodeError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (DecodeError::InvalidProtocol, DecodeError::InvalidProtocol) => true,
            (DecodeError::InvalidLength, DecodeError::InvalidLength) => true,
            (DecodeError::UnsupportedProtocolLevel, DecodeError::UnsupportedProtocolLevel) => {
                true
            }
            (DecodeError::ConnectReservedFlagSet, DecodeError::ConnectReservedFlagSet) => true,
            (DecodeError::ConnAckReservedFlagSet, DecodeError::ConnAckReservedFlagSet) => true,
            (DecodeError::InvalidClientId, DecodeError::InvalidClientId) => true,
            (DecodeError::UnsupportedPacketType, DecodeError::UnsupportedPacketType) => true,
            (DecodeError::PacketIdRequired, DecodeError::PacketIdRequired) => true,
            (DecodeError::MaxSizeExceeded, DecodeError::MaxSizeExceeded) => true,
            (DecodeError::MalformedPacket, DecodeError::MalformedPacket) => true,
            (DecodeError::Utf8Error(_), _) => false,
            _ => false,
        }
    }
}

#[derive(Debug, Display, PartialEq)]
pub enum SendPacketError {
    /// Encoder error
    Encode(EncodeError),
    /// Provided packet id is in use
    #[display(fmt = "Provided packet id is in use")]
    PacketIdInUse(u16),
    /// Peer disconnected
    #[display(fmt = "Peer disconnected")]
    Disconnected,
}

impl error::Error for SendPacketError {}
