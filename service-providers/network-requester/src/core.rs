// Copyright 2020 - Nym Technologies SA <contact@nymtech.net>
// SPDX-License-Identifier: Apache-2.0

use crate::allowed_hosts::{HostsStore, OutboundRequestFilter};
use crate::connection::Connection;
use crate::error::NetworkRequesterError;
use crate::statistics::ServiceStatisticsCollector;
use crate::websocket;
use crate::websocket::TSWebsocketStream;
use client_connections::{
    ConnectionCommand, ConnectionCommandReceiver, LaneQueueLengths, TransmissionLane,
};
use futures::channel::mpsc;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use log::*;
use nymsphinx::addressing::clients::Recipient;
use nymsphinx::anonymous_replies::requests::AnonymousSenderTag;
use nymsphinx::receiver::ReconstructedMessage;
use proxy_helpers::connection_controller::{
    BroadcastActiveConnections, Controller, ControllerCommand, ControllerSender,
};
use proxy_helpers::proxy_runner::{MixProxyReader, MixProxySender};
use socks5_requests::{
    ConnectRequest, ConnectionId, Message as Socks5Message, NetworkRequesterResponse, Request,
    Response,
};
use statistics_common::collector::StatisticsSender;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use task::ShutdownListener;
use tokio_tungstenite::tungstenite::protocol::Message;
use websocket_requests::{requests::ClientRequest, responses::ServerResponse};

// Since it's an atomic, it's safe to be kept static and shared across threads
static ACTIVE_PROXIES: AtomicUsize = AtomicUsize::new(0);

pub struct ServiceProvider {
    listening_address: String,
    outbound_request_filter: OutboundRequestFilter,
    open_proxy: bool,
    enable_statistics: bool,
    stats_provider_addr: Option<Recipient>,
}

// TODO: move elsewhere after things settle
#[derive(Debug, Clone)]
pub enum ReturnAddress {
    Known(Box<Recipient>),
    Anonymous(AnonymousSenderTag),
}

impl From<Recipient> for ReturnAddress {
    fn from(recipient: Recipient) -> Self {
        ReturnAddress::Known(Box::new(recipient))
    }
}

impl From<AnonymousSenderTag> for ReturnAddress {
    fn from(sender_tag: AnonymousSenderTag) -> Self {
        ReturnAddress::Anonymous(sender_tag)
    }
}

impl ReturnAddress {
    fn new(
        explicit_return_address: Option<Recipient>,
        implicit_tag: Option<AnonymousSenderTag>,
    ) -> Option<Self> {
        // if somehow we received both, always prefer the explicit address since it's way easier to use
        if let Some(recipient) = explicit_return_address {
            return Some(ReturnAddress::Known(Box::new(recipient)));
        }
        if let Some(sender_tag) = implicit_tag {
            return Some(ReturnAddress::Anonymous(sender_tag));
        }
        None
    }

    fn send_back_to(self, message: Vec<u8>, connection_id: u64) -> ClientRequest {
        match self {
            ReturnAddress::Known(recipient) => ClientRequest::Send {
                recipient: *recipient,
                message,
                connection_id: Some(connection_id),
            },
            ReturnAddress::Anonymous(sender_tag) => ClientRequest::Reply {
                message,
                sender_tag,
                connection_id: Some(connection_id),
            },
        }
    }
}

impl ServiceProvider {
    pub fn new(
        listening_address: String,
        open_proxy: bool,
        enable_statistics: bool,
        stats_provider_addr: Option<Recipient>,
    ) -> ServiceProvider {
        let allowed_hosts = HostsStore::new(
            HostsStore::default_base_dir(),
            PathBuf::from("allowed.list"),
        );

        let unknown_hosts = HostsStore::new(
            HostsStore::default_base_dir(),
            PathBuf::from("unknown.list"),
        );

        let outbound_request_filter = OutboundRequestFilter::new(allowed_hosts, unknown_hosts);
        ServiceProvider {
            listening_address,
            outbound_request_filter,
            open_proxy,
            enable_statistics,
            stats_provider_addr,
        }
    }

    /// Listens for any messages from `mix_reader` that should be written back to the mix network
    /// via the `websocket_writer`.
    async fn mixnet_response_listener(
        mut websocket_writer: SplitSink<TSWebsocketStream, Message>,
        mut mix_reader: MixProxyReader<(Socks5Message, ReturnAddress)>,
        stats_collector: Option<ServiceStatisticsCollector>,
        mut client_connection_rx: ConnectionCommandReceiver,
    ) {
        loop {
            tokio::select! {
                socks5_msg = mix_reader.recv() => {
                    if let Some((msg, return_address)) = socks5_msg {
                        if let Some(stats_collector) = stats_collector.as_ref() {
                            if let Some(remote_addr) = stats_collector
                                .connected_services
                                .read()
                                .await
                                .get(&msg.conn_id())
                            {
                                stats_collector
                                    .response_stats_data
                                    .write()
                                    .await
                                    .processed(remote_addr, msg.size() as u32);
                            }
                        }

                        // make 'request' to native-websocket client
                        let conn_id = msg.conn_id();
                        let response_message = return_address.send_back_to(msg.into_bytes(), conn_id);

                        let message = Message::Binary(response_message.serialize());
                        websocket_writer.send(message).await.unwrap();
                    } else {
                        log::error!("Exiting: channel closed!");
                        break;
                    }
                },
                Some(command) = client_connection_rx.next() => {
                    match command {
                        ConnectionCommand::Close(id) => {
                            let msg = ClientRequest::ClosedConnection(id);
                            let ws_msg = Message::Binary(msg.serialize());
                            websocket_writer.send(ws_msg).await.unwrap();
                        }
                        ConnectionCommand::ActiveConnections(ids) => {
                            // We can optimize this by sending a single request, but this is
                            // usually in the low single digits, max a few tens, so we leave that
                            // for a rainy day.
                            // Also that means fiddling with the currently manual
                            // serialize/deserialize we do with ClientRequests ... bleh
                            for id in ids {
                                log::trace!("Requesting lane queue length for: {}", id);
                                let msg = ClientRequest::GetLaneQueueLength(id);
                                let ws_msg = Message::Binary(msg.serialize());
                                websocket_writer.send(ws_msg).await.unwrap();
                            }
                        }
                    }
                },
            }
        }
    }

    fn handle_lane_queue_length_response(
        lane_queue_lengths: &LaneQueueLengths,
        lane: u64,
        queue_length: usize,
    ) {
        log::trace!("Received LaneQueueLength lane: {lane}, queue_length: {queue_length}");
        if let Ok(mut lane_queue_lengths) = lane_queue_lengths.lock() {
            let lane = TransmissionLane::ConnectionId(lane);
            lane_queue_lengths.map.insert(lane, queue_length);
        } else {
            log::warn!("Unable to lock lane queue lengths, skipping updating received lane length")
        }
    }

    async fn read_websocket_message(
        websocket_reader: &mut SplitStream<TSWebsocketStream>,
        lane_queue_lengths: LaneQueueLengths,
    ) -> Option<ReconstructedMessage> {
        while let Some(msg) = websocket_reader.next().await {
            let data = msg
                .expect("we failed to read from the websocket!")
                .into_data();

            // try to recover the actual message from the mix network...
            let deserialized_message = match ServerResponse::deserialize(&data) {
                Ok(deserialized) => deserialized,
                Err(err) => {
                    error!(
                        "Failed to deserialize received websocket message! - {}",
                        err
                    );
                    continue;
                }
            };

            let received = match deserialized_message {
                ServerResponse::Received(received) => received,
                ServerResponse::LaneQueueLength { lane, queue_length } => {
                    Self::handle_lane_queue_length_response(
                        &lane_queue_lengths,
                        lane,
                        queue_length,
                    );
                    continue;
                }
                ServerResponse::Error(err) => {
                    panic!("received error from native client! - {}", err)
                }
                _ => unimplemented!("probably should never be reached?"),
            };
            return Some(received);
        }
        None
    }

    async fn start_proxy(
        conn_id: ConnectionId,
        remote_addr: String,
        return_address: ReturnAddress,
        controller_sender: ControllerSender,
        mix_input_sender: MixProxySender<(Socks5Message, ReturnAddress)>,
        lane_queue_lengths: LaneQueueLengths,
        shutdown: ShutdownListener,
    ) {
        let mut conn =
            match Connection::new(conn_id, remote_addr.clone(), return_address.clone()).await {
                Ok(conn) => conn,
                Err(err) => {
                    error!(
                        "error while connecting to {:?} ! - {:?}",
                        remote_addr.clone(),
                        err
                    );

                    // inform the remote that the connection is closed before it even was established
                    mix_input_sender
                        .send((
                            Socks5Message::Response(Response::new(conn_id, Vec::new(), true)),
                            return_address,
                        ))
                        .await
                        .expect("InputMessageReceiver has stopped receiving!");

                    return;
                }
            };

        // Connect implies it's a fresh connection - register it with our controller
        let (mix_sender, mix_receiver) = mpsc::unbounded();
        controller_sender
            .unbounded_send(ControllerCommand::Insert(conn_id, mix_sender))
            .unwrap();

        let old_count = ACTIVE_PROXIES.fetch_add(1, Ordering::SeqCst);
        info!(
            "Starting proxy for {} (currently there are {} proxies being handled)",
            remote_addr,
            old_count + 1
        );

        // run the proxy on the connection
        conn.run_proxy(mix_receiver, mix_input_sender, lane_queue_lengths, shutdown)
            .await;

        // proxy is done - remove the access channel from the controller
        controller_sender
            .unbounded_send(ControllerCommand::Remove(conn_id))
            .unwrap();

        let old_count = ACTIVE_PROXIES.fetch_sub(1, Ordering::SeqCst);
        info!(
            "Proxy for {} is finished  (currently there are {} proxies being handled)",
            remote_addr,
            old_count - 1
        );
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_proxy_connect(
        &mut self,
        controller_sender: &mut ControllerSender,
        mix_input_sender: &MixProxySender<(Socks5Message, ReturnAddress)>,
        lane_queue_lengths: LaneQueueLengths,
        sender_tag: Option<AnonymousSenderTag>,
        connect_req: Box<ConnectRequest>,
        shutdown: ShutdownListener,
    ) {
        let return_address = match ReturnAddress::new(connect_req.return_address, sender_tag) {
            Some(address) => address,
            None => {
                log::warn!(
                "attempted to start connection with no way of returning data back to the sender"
            );
                return;
            }
        };

        let remote_addr = connect_req.remote_addr;
        let conn_id = connect_req.conn_id;

        if !self.open_proxy && !self.outbound_request_filter.check(&remote_addr) {
            let log_msg = format!("Domain {:?} failed filter check", remote_addr);
            log::info!("{}", log_msg);
            mix_input_sender
                .send((
                    Socks5Message::NetworkRequesterResponse(NetworkRequesterResponse::new(
                        conn_id, log_msg,
                    )),
                    return_address,
                ))
                .await
                .expect("InputMessageReceiver has stopped receiving!");
            return;
        }

        let controller_sender_clone = controller_sender.clone();
        let mix_input_sender_clone = mix_input_sender.clone();

        // and start the proxy for this connection
        tokio::spawn(async move {
            Self::start_proxy(
                conn_id,
                remote_addr,
                return_address,
                controller_sender_clone,
                mix_input_sender_clone,
                lane_queue_lengths,
                shutdown,
            )
            .await
        });
    }

    fn handle_proxy_send(
        controller_sender: &mut ControllerSender,
        conn_id: ConnectionId,
        data: Vec<u8>,
        closed: bool,
    ) {
        controller_sender
            .unbounded_send(ControllerCommand::Send(conn_id, data, closed))
            .unwrap()
    }

    async fn handle_proxy_message(
        &mut self,
        message: ReconstructedMessage,
        controller_sender: &mut ControllerSender,
        mix_input_sender: &MixProxySender<(Socks5Message, ReturnAddress)>,
        lane_queue_lengths: LaneQueueLengths,
        stats_collector: Option<ServiceStatisticsCollector>,
        shutdown: ShutdownListener,
    ) {
        let deserialized_msg = match Socks5Message::try_from_bytes(&message.message) {
            Ok(msg) => msg,
            Err(err) => {
                error!("Failed to deserialized received message! - {err}");
                return;
            }
        };
        match deserialized_msg {
            Socks5Message::Request(deserialized_request) => match deserialized_request {
                Request::Connect(req) => {
                    // TODO: stats might be invalid if connection fails to start
                    if let Some(stats_collector) = stats_collector {
                        stats_collector
                            .connected_services
                            .write()
                            .await
                            .insert(req.conn_id, req.remote_addr.clone());
                    }
                    self.handle_proxy_connect(
                        controller_sender,
                        mix_input_sender,
                        lane_queue_lengths,
                        message.sender_tag,
                        req,
                        shutdown,
                    )
                    .await
                }

                Request::Send(conn_id, data, closed) => {
                    if let Some(stats_collector) = stats_collector {
                        if let Some(remote_addr) = stats_collector
                            .connected_services
                            .read()
                            .await
                            .get(&conn_id)
                        {
                            stats_collector
                                .request_stats_data
                                .write()
                                .await
                                .processed(remote_addr, data.len() as u32);
                        }
                    }
                    Self::handle_proxy_send(controller_sender, conn_id, data, closed)
                }
            },
            Socks5Message::Response(_) | Socks5Message::NetworkRequesterResponse(_) => {}
        }
    }

    /// Start all subsystems
    pub async fn run(&mut self) -> Result<(), NetworkRequesterError> {
        let websocket_stream = self.connect_websocket(&self.listening_address).await?;

        // split the websocket so that we could read and write from separate threads
        let (websocket_writer, mut websocket_reader) = websocket_stream.split();

        // channels responsible for managing messages that are to be sent to the mix network. The receiver is
        // going to be used by `mixnet_response_listener`
        let (mix_input_sender, mix_input_receiver) =
            tokio::sync::mpsc::channel::<(Socks5Message, ReturnAddress)>(1);

        // Used to notify tasks to shutdown. Not all tasks fully supports this (yet).
        let shutdown = task::ShutdownNotifier::default();

        // Channel for announcing client connection state by the controller.
        // The `mixnet_response_listener` will use this to either report closed connection to the
        // client or request lane queue lengths.
        let (client_connection_tx, client_connection_rx) = mpsc::unbounded();

        // Shared queue length data. Published by the `OutQueueController` in the client, and used
        // primarily to throttle incoming connections
        let shared_lane_queue_lengths = LaneQueueLengths::new();

        // Controller for managing all active connections.
        // We provide it with a ShutdownListener since it requires it, even though for the network
        // requester shutdown signalling is not yet fully implemented.
        let (mut active_connections_controller, mut controller_sender) = Controller::new(
            client_connection_tx,
            BroadcastActiveConnections::On,
            shutdown.subscribe(),
        );

        tokio::spawn(async move {
            active_connections_controller.run().await;
        });

        let stats_collector = if self.enable_statistics {
            let stats_collector =
                ServiceStatisticsCollector::new(self.stats_provider_addr, mix_input_sender.clone())
                    .await
                    .expect("Service statistics collector could not be bootstrapped");
            let mut stats_sender = StatisticsSender::new(stats_collector.clone());

            tokio::spawn(async move {
                stats_sender.run().await;
            });
            Some(stats_collector)
        } else {
            None
        };

        let stats_collector_clone = stats_collector.clone();
        // start the listener for mix messages
        tokio::spawn(async move {
            Self::mixnet_response_listener(
                websocket_writer,
                mix_input_receiver,
                stats_collector_clone,
                client_connection_rx,
            )
            .await;
        });

        println!("\nAll systems go. Press CTRL-C to stop the server.");
        // for each incoming message from the websocket... (which in 99.99% cases is going to be a mix message)
        loop {
            let Some(received) = Self::read_websocket_message(
                    &mut websocket_reader,
                    shared_lane_queue_lengths.clone()
                )
                .await
            else {
                log::error!("The websocket stream has finished!");
                return Ok(());
            };

            self.handle_proxy_message(
                received,
                &mut controller_sender,
                &mix_input_sender,
                shared_lane_queue_lengths.clone(),
                stats_collector.clone(),
                shutdown.subscribe(),
            )
            .await;
        }
    }

    // Make the websocket connection so we can receive incoming Mixnet messages.
    async fn connect_websocket(
        &self,
        uri: &str,
    ) -> Result<TSWebsocketStream, NetworkRequesterError> {
        match websocket::Connection::new(uri).connect().await {
            Ok(ws_stream) => {
                info!("* connected to local websocket server at {}", uri);
                Ok(ws_stream)
            }
            Err(err) => {
                log::error!(
                    "Error: websocket connection attempt failed, is the Nym client running?"
                );
                Err(err.into())
            }
        }
    }
}
