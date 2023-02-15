use std::{
    future::{self, Future},
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Weak},
    time::Duration,
};

use futures::{
    stream::{self, BoxStream},
    Sink, SinkExt, Stream, StreamExt,
};
use mavlink::{
    common::MavMessage,
    error::{MessageReadError, MessageWriteError},
    MavHeader, MavFrame, MavlinkVersion, Message,
};
use tokio::{
    net::{TcpStream, UdpSocket},
    sync::Mutex,
    task, time,
};
use tokio_util::{
    udp::UdpFramed,
    codec::Framed
};

use crate::{
    MaybeRawMsg,
    mailbox::MavMailbox,
    codec::MavMessageSerde,
};

type BoxSink<Item, Error> = Pin<Box<dyn Sink<Item, Error = Error> + Send + 'static>>;

// This struct represents a connection with the MAV. It is cheaply clonable and thread safe.
#[derive(Clone)]
pub struct MavConn<M: Message> {
    // actor_handle: task::JoinHandle<()>,

    // I need two Arc's here since `shared` is behind a `Mutex` and `MavMailbox` is not.
    shared:  Arc<Mutex<MavConnInner<M>>>,
    mailbox: Arc<MavMailbox<M>>,
}

struct MavConnInner<M: Message> {
    header: MavHeader,
    sink:   BoxSink<MavFrame<MaybeRawMsg<M>>, MessageWriteError>,
}

impl MavConn<MavMessage> {
    #[tracing::instrument(level = "trace", err)]
    pub async fn new_tcp(addr: SocketAddr, system_id: u8, component_id: u8) -> io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        Self::new_with_rw(stream, system_id, component_id).await
    }

    #[tracing::instrument(level = "trace", err)]
    pub async fn new_udp(addr: SocketAddr, system_id: u8, component_id: u8) -> io::Result<Self> {
        // let bind_addr = "127.0.0.1:8080".parse::<SocketAddr>().unwrap();
        let sock = UdpSocket::bind(addr).await?;

        // Have a peek before connecting
        let mut buf = [0u8; 1024];
        let (_, incoming_addr) = sock.peek_from(&mut buf).await?;
        sock.connect(incoming_addr).await?;

        let (sink, stream) = UdpFramed::new(sock, MavMessageSerde::new()).split();
        Self::new_with_stream_sink(
            stream.filter_map({
                let addr = incoming_addr.clone();
                move |recvd| async move {
                    match recvd {
                        Ok((msg, recv_addr)) => (recv_addr == addr).then_some(Ok(msg)),
                        Err(e) => Some(Err(e)),
                    }
                }
            }),
            sink.with(move |msg| async move {
                Ok((msg, incoming_addr.clone()))
            }),
            system_id,
            component_id,
        ).await
    }

    #[tracing::instrument(level = "trace", err)]
    pub async fn new_serial(path: &str, baud_rate: u32, system_id: u8, component_id: u8) -> io::Result<Self> {
        use tokio_serial::SerialPortBuilderExt;
        let stream = tokio_serial::new(path, baud_rate).open_native_async()?;
        Self::new_with_rw(stream, system_id, component_id).await
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn new_with_rw(
        rw: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
        system_id: u8,
        component_id: u8
    ) -> io::Result<Self> {
        let (sink, stream) = Framed::new(rw, MavMessageSerde::new()).split();
        Self::new_with_stream_sink(stream, sink, system_id, component_id).await
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn new_with_stream_sink(
        stream: impl Stream<Item = Result<MavFrame<MavMessage>, MessageReadError>> + Send + 'static,
        sink: impl Sink<MavFrame<MaybeRawMsg<MavMessage>>, Error = MessageWriteError> + Send + 'static,
        system_id: u8,
        component_id: u8,
    ) -> io::Result<Self> {
        let mailbox = Arc::new(MavMailbox::new());
        let shared = Arc::new(Mutex::new(MavConnInner {
            header: MavHeader {
                system_id,
                component_id,
                sequence: 0,
            },
            sink:   Box::pin(sink),
        }));

        let _actor_handle = MavConnActor::spawn(
            Arc::downgrade(&shared),
            Arc::downgrade(&mailbox),
            stream.boxed(),
        );

        Ok(MavConn { shared, mailbox })
    }
}

impl<M: Message + Send + 'static> MavConn<M> {
    #[tracing::instrument(level = "trace", skip_all, err)]
    pub async fn recv_with_header(&self) -> io::Result<MavFrame<M>> {
        self.mailbox
            .next()
            .await
            .ok_or(io::ErrorKind::ConnectionAborted.into())
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn recv_with_header_id(&self, id: u32) -> io::Result<MavFrame<M>> {
        self.mailbox
            .next_with_id(id)
            .await
            .ok_or(io::ErrorKind::ConnectionAborted.into())
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn recv(&self) -> io::Result<M> {
        self.recv_with_header().await.map(|msg| msg.msg)
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn recv_id(&self, id: u32) -> io::Result<M> {
        self.recv_with_header_id(id).await.map(|msg| msg.msg)
    }

    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn send<T: Into<MaybeRawMsg<M>>>(&self, msg: T) -> io::Result<()> {
        let mut locked = self.shared.lock().await;
        let MavConnInner { sink, header, .. } = &mut *locked;
        let msg_with_header = MavFrame {
            protocol_version: MavlinkVersion::V2,
            header:           header.clone(),
            msg:              msg.into(),
        };
        sink.send(msg_with_header)
            .await
            .map_err(|MessageWriteError::Io(e)| e)?;
        header.sequence = header.sequence.wrapping_add(1);
        Ok(())
    }

    // Sends a messag and processes incoming messages until `match_fn` returns `Some(T)`. If
    // `timeout` is exceeded, the message is resent. Will retry a maximum of 5 times.
    #[tracing::instrument(level = "trace", skip(self, get_msg, match_fn))]
    pub async fn send_with_timeout_until<T, S: Into<MaybeRawMsg<M>>>(
        &self,
        timeout: Duration,
        mut get_msg: impl FnMut() -> S,
        mut match_fn: impl FnMut(MavFrame<M>) -> Option<T>,
    ) -> io::Result<T> {
        self.send(get_msg()).await?;

        let stream = self.stream().filter_map(|msg| future::ready(match_fn(msg)));
        futures::pin_mut!(stream);
        for _ in 0..5 {
            match time::timeout(timeout, stream.next()).await {
                Ok(None) => return Err(io::ErrorKind::ConnectionAborted.into()),
                Ok(Some(res)) => return Ok(res),
                Err(_) => {
                    // Retransmit
                    self.send(get_msg()).await?;
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::Other,
            "maximum retries attempted",
        ))
    }

    #[tracing::instrument(level = "trace", skip(self, get_msg, match_fn))]
    pub async fn send_with_timeout_until_id<T, S: Into<MaybeRawMsg<M>>>(
        &self,
        timeout: Duration,
        id: u32,
        mut get_msg: impl FnMut() -> S,
        mut match_fn: impl FnMut(MavFrame<M>) -> Option<T>,
    ) -> io::Result<T> {
        self.send(get_msg()).await?;

        let stream = self
            .stream_id(id)
            .filter_map(|msg| future::ready(match_fn(msg)));
        futures::pin_mut!(stream);
        for _ in 0..5 {
            match time::timeout(timeout, stream.next()).await {
                Ok(None) => return Err(io::ErrorKind::ConnectionAborted.into()),
                Ok(Some(res)) => return Ok(res),
                Err(_) => {
                    // Retransmit
                    self.send(get_msg()).await?;
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::Other,
            "maximum retries attempted",
        ))
    }

    pub fn stream(&self) -> impl Stream<Item = MavFrame<M>> + Send + '_ {
        stream_from_fn(move || self.mailbox.next())
    }

    pub fn stream_id(&self, id: u32) -> impl Stream<Item = MavFrame<M>> + Send + '_ {
        stream_from_fn(move || self.mailbox.next_with_id(id))
    }
}

impl MavConn<MavMessage> {
    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn clear_mission(&self) -> io::Result<()> {
        use mavlink::common::{MavMissionResult, MISSION_CLEAR_ALL_DATA};

        let msg = MavMessage::MISSION_CLEAR_ALL(MISSION_CLEAR_ALL_DATA {
            target_system:    1,
            target_component: 1,
        });
        let ack = self
            .send_with_timeout_until(
                Duration::from_millis(500),
                || msg.clone(),
                |msg| match msg {
                    MavFrame {
                        msg: MavMessage::MISSION_ACK(ack),
                        ..
                    } => Some(ack),
                    _ => None,
                },
            )
            .await?;
        if ack.mavtype != MavMissionResult::MAV_MISSION_ACCEPTED {
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("failed to clear mission, with reasult {:?}", ack.mavtype),
            ))
        } else {
            Ok(())
        }
    }

    // Caller should make sure that the first waypoint is close enough to the current drone
    // position.
    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn send_mission(&self, cmds: &[mavlink::common::COMMAND_INT_DATA]) -> io::Result<()> {
        use mavlink::common::{MavMissionResult, MISSION_COUNT_DATA, MISSION_ITEM_INT_DATA};

        let count = MavMessage::MISSION_COUNT(MISSION_COUNT_DATA {
            count:            cmds.len() as u16,
            // TODO: UNHARDCODE
            target_system:    0x1,
            target_component: 0x1,
        });

        if cmds.len() == 0 {
            self.send_with_timeout_until_id(
                Duration::from_secs(1),
                MavMessage::message_id_from_name("MISSION_ACK").unwrap(),
                || count.clone(),
                |msg| {
                    matches!(msg, MavFrame {
                        msg: MavMessage::MISSION_ACK(_),
                        ..
                    })
                    .then_some(())
                },
            )
            .await?;
            return Ok(());
        }

        self.send_with_timeout_until(
            Duration::from_secs(1),
            || count.clone(),
            |msg| match msg {
                MavFrame {
                    msg: MavMessage::MISSION_REQUEST_INT(req),
                    ..
                } if req.seq == 0 => Some(()),
                MavFrame {
                    msg: MavMessage::MISSION_REQUEST(req),
                    ..
                } if req.seq == 0 => Some(()),
                _ => None,
            },
        )
        .await?;

        for (seq, cmd) in cmds.iter().enumerate() {
            let msg = MavMessage::MISSION_ITEM_INT(MISSION_ITEM_INT_DATA {
                target_system:    0x1,
                target_component: 0x1,
                seq:              seq as u16,
                current:          if seq == 0 { 1 } else { 0 },
                autocontinue:     0x1,
                command:          cmd.command,
                frame:            cmd.frame,
                param1:           cmd.param1,
                param2:           cmd.param2,
                param3:           cmd.param3,
                param4:           cmd.param4,
                x:                cmd.x,
                y:                cmd.y,
                z:                cmd.z,
            });

            if seq < cmds.len() - 1 {
                self.send_with_timeout_until(
                    Duration::from_millis(500),
                    || msg.clone(),
                    |msg| match msg {
                        MavFrame {
                            msg: MavMessage::MISSION_REQUEST_INT(req),
                            ..
                        } if usize::from(req.seq) == seq + 1 => Some(()),
                        MavFrame {
                            msg: MavMessage::MISSION_REQUEST(req),
                            ..
                        } if usize::from(req.seq) == seq + 1 => Some(()),
                        _ => None,
                    },
                )
                .await?;
            } else {
                let ack = self
                    .send_with_timeout_until_id(
                        Duration::from_secs(1),
                        MavMessage::message_id_from_name("MISSION_ACK").unwrap(),
                        || msg.clone(),
                        |msg| match msg {
                            MavFrame {
                                msg: MavMessage::MISSION_ACK(ack),
                                ..
                            } => Some(ack),
                            _ => None,
                        },
                    )
                    .await?;

                if ack.mavtype != MavMissionResult::MAV_MISSION_ACCEPTED {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("failed to clear mission, with reasult {:?}", ack.mavtype),
                    ));
                } else {
                    return Ok(());
                }
            }
        }

        unreachable!(
            "We have checked that cmds is not empty and in the last iteration it will always return"
        );
    }

    // NOTE: the `confirmation` field of the `cmd` will be substituted by the appropriate value by
    // this function.
    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn send_cmd_long(&self, cmd: mavlink::common::COMMAND_LONG_DATA) -> io::Result<bool> {
        use mavlink::common::{MavResult, COMMAND_LONG_DATA};

        let mut confirmation = 0;
        self.send_with_timeout_until_id(
            Duration::from_millis(500),
            MavMessage::message_id_from_name("COMMAND_ACK").unwrap(),
            || {
                // TODO: Test this
                let msg = MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
                    confirmation,
                    ..cmd.clone()
                });
                confirmation += 1;
                msg
            },
            |msg| match msg {
                MavFrame {
                    msg: MavMessage::COMMAND_ACK(ack),
                    ..
                } if ack.command == cmd.command
                    && ack.result != MavResult::MAV_RESULT_IN_PROGRESS =>
                {
                    Some(ack.result == MavResult::MAV_RESULT_ACCEPTED)
                }
                _ => None,
            },
        )
        .await
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn send_cmd(&self, cmd: mavlink::common::COMMAND_INT_DATA) -> io::Result<bool> {
        use mavlink::common::MavResult;
        self.send_with_timeout_until_id(
            Duration::from_millis(500),
            MavMessage::message_id_from_name("COMMAND_ACK").unwrap(),
            || MavMessage::COMMAND_INT(cmd.clone()),
            |msg| match msg {
                MavFrame {
                    msg: MavMessage::COMMAND_ACK(ack),
                    ..
                } if ack.command == cmd.command => {
                    Some(ack.result == MavResult::MAV_RESULT_ACCEPTED)
                }
                _ => None,
            },
        )
        .await
    }
}

impl<M: Message> MavConnInner<M> {
    fn advance_seq(&mut self) {
        self.header.sequence = self.header.sequence.wrapping_add(1);
    }
}

struct MavConnActor<M: Message> {
    hb_timer: tokio::time::Interval,
    stream:   BoxStream<'static, Result<MavFrame<M>, MessageReadError>>,
    // Weak pointers since it's not the actor that maintains the connection alive, but
    // instead the connections that maintain the actor alive.
    mailbox:  Weak<MavMailbox<M>>,
    shared:   Weak<Mutex<MavConnInner<M>>>,
}

impl MavConnActor<MavMessage> {
    fn spawn(
        shared: Weak<Mutex<MavConnInner<MavMessage>>>,
        mailbox: Weak<MavMailbox<MavMessage>>,
        stream: BoxStream<'static, Result<MavFrame<MavMessage>, MessageReadError>>,
    ) -> task::JoinHandle<()> {
        let mut me = MavConnActor {
            hb_timer: tokio::time::interval(Duration::from_secs(1)),
            shared,
            mailbox,
            stream,
        };
        task::spawn(async move { me.run().await })
    }

    async fn run(&mut self) {
        let mut next_fut = self.stream.next();
        loop {
            tokio::select! {
                _ = self.hb_timer.tick() => {
                    tracing::trace!("sending heartbeat");

                    use mavlink::common::*;

                    let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
                        mavtype: MavType::MAV_TYPE_ONBOARD_CONTROLLER,
                        autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
                        mavlink_version: 0x3,
                        system_status: MavState::MAV_STATE_ACTIVE,
                        ..HEARTBEAT_DATA::default()
                    }).into();
                    let Some(arc)= self.shared.upgrade() else {
                        // The connection was dropped.
                        return;
                    };
                    let mut locked = arc.lock().await;
                    let pair = mavlink::MavFrame {
                        protocol_version: MavlinkVersion::V2,
                        header: locked.header.clone(),
                        msg,
                    };
                    // TODO: Handle this better.
                    if let Err(e) = locked.sink.send(pair).await {
                        tracing::warn!(error = ?e, "failed to send MAVLink heatbeat");
                    } else {
                        locked.advance_seq();
                    }
                }

                result_pair = &mut next_fut => {
                    let Some(arc) = self.mailbox.upgrade() else {
                        // The mailbox was already dropped.
                        return;
                    };
                    let pair = match result_pair {
                        None => {
                            // This should be unreachable I think. But anyway...
                            tracing::error!("the impossible happened! but it's no big deal");
                            arc.close();
                            return;
                        },
                        Some(Err(e)) => {
                            tracing::warn!(error = ?e, "failed to read MAVLink message");
                            continue;
                        }
                        Some(Ok(msg)) => msg,
                    };
                    tracing::trace!(msg = ?pair, "received mavlink message");
                    arc.insert_now(pair).await;
                    next_fut = self.stream.next();
                }
            }
        }
    }
}

fn stream_from_fn<'a, F, Fut, Item>(mut f: F) -> impl Stream<Item = Item> + Send + 'a
where
    F: FnMut() -> Fut + Send + 'a,
    Fut: Future<Output = Option<Item>> + Send + 'a,
{
    stream::unfold((), move |_| {
        let fut = f();
        async move { Some((fut.await?, ())) }
    })
}
