//! Contains the code that listens to control and data connections on a single TCP port (proxy
//! protocol mode).

use crate::server::failed_logins::FailedLoginsCache;
use crate::server::shutdown;
use crate::{
    ServerError,
    auth::UserDetail,
    server::{
        ControlChanMsg, Reply, ReplyCode,
        chancomms::{ProxyLoopMsg, ProxyLoopReceiver, ProxyLoopSender},
        controlchan,
        datachan::spawn_processing,
        ftpserver::chosen::OptionsHolder,
        proxy_protocol::{ProxyConnection, ProxyProtocolSwitchboard, spawn_proxy_header_parsing},
        session::SharedSession,
    },
    storage::StorageBackend,
};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use tokio::{io::AsyncWriteExt, sync::mpsc::channel};

// ProxyProtocolListener binds to a single port and assumes connections multiplexed by the
// [proxy protocol](https://www.haproxy.com/blog/haproxy/proxy-protocol/)
pub(super) struct ProxyProtocolListener<Storage, User>
where
    Storage: StorageBackend<User>,
    User: UserDetail,
{
    pub bind_address: SocketAddr,
    pub logger: slog::Logger,
    pub external_control_port: u16,
    pub options: OptionsHolder<Storage, User>,
    pub proxy_protocol_switchboard: Option<ProxyProtocolSwitchboard<Storage, User>>,
    pub shutdown_topic: Arc<shutdown::Notifier>,
    pub failed_logins: Option<Arc<FailedLoginsCache>>,
}

impl<Storage, User> ProxyProtocolListener<Storage, User>
where
    Storage: StorageBackend<User> + 'static,
    User: UserDetail + 'static,
{
    // Starts listening, returning an error if the TCP address could not be bound to.
    pub async fn listen(mut self) -> std::result::Result<(), ServerError> {
        let listener = tokio::net::TcpListener::bind(self.bind_address).await?;

        // this callback is used by all sessions, basically only to
        // request for a passive listening port.
        let (proxyloop_msg_tx, mut proxyloop_msg_rx): (ProxyLoopSender<Storage, User>, ProxyLoopReceiver<Storage, User>) = channel(1);

        loop {
            // The 'proxy loop' handles two kinds of events:
            // - incoming tcp connections originating from the proxy
            // - channel messages originating from PASV, to handle the passive listening port

            tokio::select! {

                Ok((tcp_stream, _socket_addr)) = listener.accept() => {
                    let socket_addr = tcp_stream.peer_addr();

                    slog::info!(self.logger, "Incoming proxy connection from {:?}", socket_addr);
                    spawn_proxy_header_parsing(self.logger.clone(), tcp_stream, proxyloop_msg_tx.clone());
                },
                Some(msg) = proxyloop_msg_rx.recv() => {
                    match msg {
                        ProxyLoopMsg::ProxyHeaderReceived (connection, mut tcp_stream) => {
                            let socket_addr = tcp_stream.peer_addr();
                            // Based on the proxy protocol header, and the configured control port number,
                            // we differentiate between connections for the control channel,
                            // and connections for the data channel.
                            let destination_port = connection.destination.port();
                            if destination_port == self.external_control_port {
                                slog::info!(self.logger, "Incoming control connection: {:?} ({:?})(control port: {:?})", connection, socket_addr, self.external_control_port);
                                let params: controlchan::LoopConfig<Storage,User> = (&self.options).into();
                                let result = controlchan::spawn_loop::<Storage,User>(params, tcp_stream, Some(connection), Some(proxyloop_msg_tx.clone()), self.shutdown_topic.subscribe().await, self.failed_logins.clone()).await;
                                if let Err(e) = result {
                                    slog::warn!(self.logger, "Could not spawn control channel loop for connection: {:?}", e);
                                }
                            } else {
                                // handle incoming data connections
                                slog::info!(self.logger, "Incoming data connection: {:?} ({:?}) (range: {:?})", connection, socket_addr, self.options.passive_ports);
                                if !self.options.passive_ports.contains(&destination_port) {
                                    slog::warn!(self.logger, "Incoming proxy connection going to unconfigured port! This port is not configured as a passive listening port: port {} not in passive port range {:?}", destination_port, self.options.passive_ports);
                                    tcp_stream.shutdown().await?;
                                    continue;
                                }
                                self.dispatch_data_connection(tcp_stream, connection).await;
                            }
                        },
                        ProxyLoopMsg::AssignDataPortCommand (session_arc) => {
                            self.select_and_register_passive_port(session_arc).await;
                        },
                        // This is sent from the control loop when it exits, so that the port is freed
                        ProxyLoopMsg::CloseDataPortCommand (session_arc) => {
                            if let Some(switchboard) = &mut self.proxy_protocol_switchboard {
                                let session = session_arc.lock().await;
                                if let Some(active_datachan) = &session.proxy_active_datachan {
                                    slog::info!(self.logger, "Unregistering active data channel port because the control channel is closing {:?}", active_datachan);
                                    switchboard.unregister_hash(active_datachan);
                                }
                            }
                        },
                    }
                },
            };
        }
    }

    // this function finds (by hashing <srcip>.<dstport>) the session
    // that requested this data channel connection in the proxy
    // protocol switchboard hashmap, and then calls the
    // spawn_data_processing function with the tcp_stream
    async fn dispatch_data_connection(&mut self, mut tcp_stream: tokio::net::TcpStream, connection: ProxyConnection) {
        if let Some(switchboard) = &mut self.proxy_protocol_switchboard {
            match switchboard.get_session_by_incoming_data_connection(&connection).await {
                Some(session) => {
                    spawn_processing(self.logger.clone(), session, tcp_stream).await;
                    switchboard.unregister_this(&connection);
                }
                None => {
                    slog::warn!(self.logger, "Unexpected connection ({:?})", connection);
                    if let Err(e) = tcp_stream.shutdown().await {
                        slog::error!(self.logger, "Error during tcp_stream shutdown: {:?}", e);
                    }
                }
            }
        }
    }

    async fn select_and_register_passive_port(&mut self, session_arc: SharedSession<Storage, User>) {
        slog::info!(self.logger, "Received internal message to allocate data port");
        // 1. reserve a port
        // 2. put the session_arc and tx in the hashmap with srcip+dstport as key
        // 3. put expiry time in the LIFO list
        // 4. send reply to client: "Entering Passive Mode ({},{},{},{},{},{})"

        let port = match &mut self.proxy_protocol_switchboard {
            Some(switchboard) => switchboard.reserve_next_free_port(session_arc.clone()).await,
            None => panic!("Proxy protocol switchboard unavailable. This should not happen."),
        };

        let session = session_arc.lock().await;
        if let Some(proxy_connection) = session.proxy_control {
            let destination_ip = match proxy_connection.destination.ip() {
                IpAddr::V4(ip) => ip,
                IpAddr::V6(_) => panic!("Won't happen since PASV only does IP V4."),
            };

            let reply = match port {
                Ok(port) => super::controlchan::commands::make_pasv_reply(&self.logger, self.options.passive_host.clone(), &destination_ip, port).await,
                Err(_) => Reply::new_with_string(ReplyCode::CantOpenDataConnection, "Local error".to_string()),
            };

            let tx_some = session.control_msg_tx.clone();
            if let Some(tx) = tx_some {
                let tx = tx.clone();
                if let Err(err) = tx.send(ControlChanMsg::CommandChannelReply(reply)).await {
                    slog::warn!(
                        self.logger,
                        "PASV (listen_proxied): Could not send internal message to reply to the client: {}",
                        err
                    );
                }
            }
        }
    }
}
