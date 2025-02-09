//! Rlpx protocol multiplexer and satellite stream
//!
//! A Satellite is a Stream that primarily drives a single RLPx subprotocol but can also handle
//! additional subprotocols.
//!
//! Most of other subprotocols are "dependent satellite" protocols of "eth" and not a fully standalone protocol, for example "snap", See also [snap protocol](https://github.com/ethereum/devp2p/blob/298d7a77c3bf833641579ecbbb5b13f0311eeeea/caps/snap.md?plain=1#L71)
//! Hence it is expected that the primary protocol is "eth" and the additional protocols are
//! "dependent satellite" protocols.

use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    io,
    pin::Pin,
    task::{ready, Context, Poll},
};

use bytes::{Bytes, BytesMut};
use futures::{pin_mut, Sink, SinkExt, Stream, StreamExt, TryStream, TryStreamExt};
use tokio::sync::{mpsc, mpsc::UnboundedSender};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::{
    capability::{Capability, SharedCapabilities, SharedCapability, UnsupportedCapabilityError},
    errors::P2PStreamError,
    CanDisconnect, DisconnectReason, P2PStream,
};

/// A Stream and Sink type that wraps a raw rlpx stream [P2PStream] and handles message ID
/// multiplexing.
#[derive(Debug)]
pub struct RlpxProtocolMultiplexer<St> {
    /// The raw p2p stream
    conn: P2PStream<St>,
    /// All the subprotocols that are multiplexed on top of the raw p2p stream
    protocols: Vec<ProtocolStream>,
}

impl<St> RlpxProtocolMultiplexer<St> {
    /// Wraps the raw p2p stream
    pub fn new(conn: P2PStream<St>) -> Self {
        Self { conn, protocols: Default::default() }
    }

    /// Installs a new protocol on top of the raw p2p stream
    pub fn install_protocol<S>(
        &mut self,
        _cap: Capability,
        _st: S,
    ) -> Result<(), UnsupportedCapabilityError> {
        todo!()
    }

    /// Returns the [SharedCapabilities] of the underlying raw p2p stream
    pub fn shared_capabilities(&self) -> &SharedCapabilities {
        self.conn.shared_capabilities()
    }

    /// Converts this multiplexer into a [RlpxSatelliteStream] with the given primary protocol.
    ///
    /// Returns an error if the primary protocol is not supported by the remote or the handshake
    /// failed.
    pub async fn into_satellite_stream_with_handshake<F, Fut, Err, Primary>(
        mut self,
        cap: &Capability,
        handshake: F,
    ) -> Result<RlpxSatelliteStream<St, Primary>, Self>
    where
        F: FnOnce(ProtocolProxy) -> Fut,
        Fut: Future<Output = Result<Primary, Err>>,
        St: Stream<Item = io::Result<BytesMut>> + Sink<Bytes, Error = io::Error> + Unpin,
    {
        let Ok(shared_cap) = self.shared_capabilities().ensure_matching_capability(cap).cloned()
        else {
            return Err(self)
        };

        let (to_primary, from_wire) = mpsc::unbounded_channel();
        let (to_wire, mut from_primary) = mpsc::unbounded_channel();
        let proxy = ProtocolProxy {
            cap: shared_cap.clone(),
            from_wire: UnboundedReceiverStream::new(from_wire),
            to_wire,
        };

        let f = handshake(proxy);
        pin_mut!(f);

        // handle messages until the handshake is complete
        loop {
            // TODO error handling
            tokio::select! {
                Some(Ok(msg)) = self.conn.next() => {
                    // TODO handle multiplex
                    let _ = to_primary.send(msg);
                }
                Some(msg) = from_primary.recv() => {
                    // TODO error handling
                    self.conn.send(msg).await.unwrap();
                }
                res = &mut f => {
                    let Ok(primary) = res else { return Err(self) };
                    return Ok(RlpxSatelliteStream {
                            conn: self.conn,
                            to_primary,
                            from_primary: UnboundedReceiverStream::new(from_primary),
                            primary,
                            primary_capability: shared_cap,
                            satellites: self.protocols,
                            out_buffer: Default::default(),
                    })
                }
            }
        }
    }
}

/// A Stream and Sink type that acts as a wrapper around a primary RLPx subprotocol (e.g. "eth")
#[derive(Debug)]
pub struct ProtocolProxy {
    cap: SharedCapability,
    from_wire: UnboundedReceiverStream<BytesMut>,
    to_wire: UnboundedSender<Bytes>,
}

impl ProtocolProxy {
    fn mask_msg_id(&self, msg: Bytes) -> Bytes {
        // TODO handle empty messages
        let mut masked_bytes = BytesMut::zeroed(msg.len());
        masked_bytes[0] = msg[0] + self.cap.relative_message_id_offset();
        masked_bytes[1..].copy_from_slice(&msg[1..]);
        masked_bytes.freeze()
    }

    fn unmask_id(&self, mut msg: BytesMut) -> BytesMut {
        // TODO handle empty messages
        msg[0] -= self.cap.relative_message_id_offset();
        msg
    }
}

impl Stream for ProtocolProxy {
    type Item = Result<BytesMut, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let msg = ready!(self.from_wire.poll_next_unpin(cx));
        Poll::Ready(msg.map(|msg| Ok(self.get_mut().unmask_id(msg))))
    }
}

impl Sink<Bytes> for ProtocolProxy {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        let msg = self.mask_msg_id(item);
        self.to_wire.send(msg).map_err(|_| io::ErrorKind::BrokenPipe.into())
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

#[async_trait::async_trait]
impl CanDisconnect<Bytes> for ProtocolProxy {
    async fn disconnect(
        &mut self,
        _reason: DisconnectReason,
    ) -> Result<(), <Self as Sink<Bytes>>::Error> {
        // TODO handle disconnects
        Ok(())
    }
}

/// A connection channel to receive messages for the negotiated protocol.
///
/// This is a [Stream] that returns raw bytes of the received messages for this protocol.
#[derive(Debug)]
pub struct ProtocolConnection {
    from_wire: UnboundedReceiverStream<BytesMut>,
}

impl Stream for ProtocolConnection {
    type Item = BytesMut;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.from_wire.poll_next_unpin(cx)
    }
}

/// A Stream and Sink type that acts as a wrapper around a primary RLPx subprotocol (e.g. "eth")
/// [EthStream](crate::EthStream) and can also handle additional subprotocols.
#[derive(Debug)]
pub struct RlpxSatelliteStream<St, Primary> {
    /// The raw p2p stream
    conn: P2PStream<St>,
    to_primary: UnboundedSender<BytesMut>,
    from_primary: UnboundedReceiverStream<Bytes>,
    primary: Primary,
    primary_capability: SharedCapability,
    satellites: Vec<ProtocolStream>,
    out_buffer: VecDeque<Bytes>,
}

impl<St, Primary> RlpxSatelliteStream<St, Primary> {}

impl<St, Primary, PrimaryErr> Stream for RlpxSatelliteStream<St, Primary>
where
    St: Stream<Item = io::Result<BytesMut>> + Sink<Bytes, Error = io::Error> + Unpin,
    Primary: TryStream<Error = PrimaryErr> + Unpin,
    P2PStreamError: Into<PrimaryErr>,
{
    type Item = Result<Primary::Ok, Primary::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            // first drain the primary stream
            if let Poll::Ready(Some(msg)) = this.primary.try_poll_next_unpin(cx) {
                return Poll::Ready(Some(msg))
            }

            let mut out_ready = true;
            loop {
                match this.conn.poll_ready_unpin(cx) {
                    Poll::Ready(_) => {
                        if let Some(msg) = this.out_buffer.pop_front() {
                            if let Err(err) = this.conn.start_send_unpin(msg) {
                                return Poll::Ready(Some(Err(err.into())))
                            }
                        } else {
                            break;
                        }
                    }
                    Poll::Pending => {
                        out_ready = false;
                        break
                    }
                }
            }

            // advance primary out
            loop {
                match this.from_primary.poll_next_unpin(cx) {
                    Poll::Ready(Some(msg)) => {
                        this.out_buffer.push_back(msg);
                    }
                    Poll::Ready(None) => {
                        // primary closed
                        return Poll::Ready(None)
                    }
                    Poll::Pending => break,
                }
            }

            // advance all satellites
            for idx in (0..this.satellites.len()).rev() {
                let mut proto = this.satellites.swap_remove(idx);
                loop {
                    match proto.poll_next_unpin(cx) {
                        Poll::Ready(Some(msg)) => {
                            this.out_buffer.push_back(msg);
                        }
                        Poll::Ready(None) => return Poll::Ready(None),
                        Poll::Pending => {
                            this.satellites.push(proto);
                            break
                        }
                    }
                }
            }

            let mut delegated = false;
            loop {
                // pull messages from connection
                match this.conn.poll_next_unpin(cx) {
                    Poll::Ready(Some(Ok(msg))) => {
                        delegated = true;
                        let offset = msg[0];
                        // find the protocol that matches the offset
                        // TODO optimize this by keeping a better index
                        let mut lowest_satellite = None;
                        // find the protocol with the lowest offset that is greater than the message
                        // offset
                        for (i, proto) in this.satellites.iter().enumerate() {
                            let proto_offset = proto.cap.relative_message_id_offset();
                            if proto_offset >= offset {
                                if let Some((_, lowest_offset)) = lowest_satellite {
                                    if proto_offset < lowest_offset {
                                        lowest_satellite = Some((i, proto_offset));
                                    }
                                } else {
                                    lowest_satellite = Some((i, proto_offset));
                                }
                            }
                        }

                        if let Some((idx, lowest_offset)) = lowest_satellite {
                            if lowest_offset < this.primary_capability.relative_message_id_offset()
                            {
                                // delegate to satellite
                                this.satellites[idx].send_raw(msg);
                                continue
                            }
                        }
                        // delegate to primary
                        let _ = this.to_primary.send(msg);
                    }
                    Poll::Ready(Some(Err(err))) => return Poll::Ready(Some(Err(err.into()))),
                    Poll::Ready(None) => {
                        // connection closed
                        return Poll::Ready(None)
                    }
                    Poll::Pending => break,
                }
            }

            if !delegated || !out_ready || this.out_buffer.is_empty() {
                return Poll::Pending
            }
        }
    }
}

impl<St, Primary, T> Sink<T> for RlpxSatelliteStream<St, Primary>
where
    St: Stream<Item = io::Result<BytesMut>> + Sink<Bytes, Error = io::Error> + Unpin,
    Primary: Sink<T, Error = io::Error> + Unpin,
    P2PStreamError: Into<<Primary as Sink<T>>::Error>,
{
    type Error = <Primary as Sink<T>>::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        if let Err(err) = ready!(this.conn.poll_ready_unpin(cx)) {
            return Poll::Ready(Err(err.into()))
        }
        if let Err(err) = ready!(this.primary.poll_ready_unpin(cx)) {
            return Poll::Ready(Err(err))
        }
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        self.get_mut().primary.start_send_unpin(item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().conn.poll_flush_unpin(cx).map_err(Into::into)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().conn.poll_close_unpin(cx).map_err(Into::into)
    }
}

/// Wraps a RLPx subprotocol and handles message ID multiplexing.
struct ProtocolStream {
    cap: SharedCapability,
    /// the channel shared with the satellite stream
    to_satellite: UnboundedSender<BytesMut>,
    satellite_st: Pin<Box<dyn Stream<Item = BytesMut>>>,
}

impl ProtocolStream {
    fn mask_msg_id(&self, mut msg: BytesMut) -> Bytes {
        // TODO handle empty messages
        msg[0] += self.cap.relative_message_id_offset();
        msg.freeze()
    }

    fn unmask_id(&self, mut msg: BytesMut) -> BytesMut {
        // TODO handle empty messages
        msg[0] -= self.cap.relative_message_id_offset();
        msg
    }

    fn send_raw(&self, msg: BytesMut) {
        let _ = self.to_satellite.send(self.unmask_id(msg));
    }
}

impl Stream for ProtocolStream {
    type Item = Bytes;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let msg = ready!(this.satellite_st.as_mut().poll_next(cx));
        Poll::Ready(msg.map(|msg| this.mask_msg_id(msg)))
    }
}

impl fmt::Debug for ProtocolStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProtocolStream").field("cap", &self.cap).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;
    use tokio_util::codec::Decoder;

    use crate::{
        test_utils::{connect_passthrough, eth_handshake, eth_hello},
        UnauthedEthStream, UnauthedP2PStream,
    };

    use super::*;

    #[tokio::test]
    async fn eth_satellite() {
        reth_tracing::init_test_tracing();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let (status, fork_filter) = eth_handshake();
        let other_status = status;
        let other_fork_filter = fork_filter.clone();
        let _handle = tokio::spawn(async move {
            let (incoming, _) = listener.accept().await.unwrap();
            let stream = crate::PassthroughCodec::default().framed(incoming);
            let (server_hello, _) = eth_hello();
            let (p2p_stream, _) =
                UnauthedP2PStream::new(stream).handshake(server_hello).await.unwrap();

            let (_eth_stream, _) = UnauthedEthStream::new(p2p_stream)
                .handshake(other_status, other_fork_filter)
                .await
                .unwrap();

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });

        let conn = connect_passthrough(local_addr, eth_hello().0).await;
        let eth = conn.shared_capabilities().eth().unwrap().clone();

        let multiplexer = RlpxProtocolMultiplexer::new(conn);

        let _satellite = multiplexer
            .into_satellite_stream_with_handshake(
                eth.capability().as_ref(),
                move |proxy| async move {
                    UnauthedEthStream::new(proxy).handshake(status, fork_filter).await
                },
            )
            .await
            .unwrap();
    }
}
