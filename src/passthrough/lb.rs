extern crate lru_time_cache;
extern crate pnet;
extern crate pnet_macros_support;
use crate::passthrough;

use self::passthrough::backend::{Backend, Node};
use self::passthrough::utils::{EPHEMERAL_PORT_LOWER, EPHEMERAL_PORT_UPPER, IPV4_HEADER_LEN};
use crate::config::Config;
use crate::stats::StatsMssg;
use lru_time_cache::LruCache;
use may::sync::{Mutex, RwLock};
use pnet::packet::ip::IpNextHeaderProtocols;
use pnet::packet::ipv4::{checksum, Ipv4Packet, MutableIpv4Packet};
use pnet::packet::tcp::MutableTcpPacket;
use pnet::packet::{tcp, Packet};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub struct Client {
    pub ip: IpAddr,
    pub port: u16,
}

#[derive(Clone, Debug)]
pub struct Connection {
    // Client tcp address
    pub client: SocketAddr,

    // backend server the client was scheduled to.  Tracked for future packets
    backend_srv: Node,

    // Unique port assigned to this connection.  Used for mapping responses from
    // backend servers to this client address
    pub ephem_port: u16,
}

// LB represents a single loadbalancer function, listening for an Address
// and scheduling packets on a pool of backend servers
#[derive(Clone)]
pub struct LB {
    // Loadbalancer name.  Maps to frontend name in the config
    pub name: String,

    // Ipv4 Address this loadbalancer listens for
    pub listen_ip: Ipv4Addr,

    // Port this loadbalancer listens for
    pub listen_port: u16,

    // The backend server logic
    pub backend: Arc<Backend>,

    // Connection tracker for bookeeping of client connections.
    // very basic right now, just used for mapping backend servers to clients
    // using may coroutine specific Mutex here since it will only be used in context of coroutine
    pub conn_tracker: Arc<RwLock<LruCache<Client, Connection>>>,

    // Port mapper for quickly looking up the client address based on
    // the port a backend server sent a response to.
    // Only used in Passthrough mode without DSR (so bidirectional)
    // Since DSR bypasses coming back through the loadbalancer this data structure
    // isn't needed in Passthrough DSR mode
    // using may coroutine specific Mutex here since it will only be used in context of coroutine
    pub port_mapper: Arc<RwLock<HashMap<u16, Client>>>,

    // Keeping track of the next port to assign for client -> backend server mappings
    // using may coroutine specific Mutex here since it will only be used in context of coroutine
    pub next_port: Arc<Mutex<u16>>,

    // Flag indicating whether we are operating in Passthrough DSR mode (server response bypasses the loadbalancer)
    // or in plain Passthrough mode (server repsonse returns through the loadbalancer and the loadbalancer
    // sends back to client).
    // False by default (so plain Passthrough/bidirectional)
    pub dsr: bool,

    // How often to update the stats/counters.  5 seconds by default
    pub stats_update_frequency: u64,
}

impl LB {
    pub fn new(frontend_name: String, conf: Config, dsr: bool) -> Option<LB> {
        let front = conf.base.frontends.get(&frontend_name).unwrap();

        let mut backend_servers = HashMap::new();

        // Set defaults
        let mut health_check_interval = 5;
        let mut connection_tracker_capacity = 1000 as usize;
        let mut stats_update_frequency = 5;

        match conf.base.passthrough {
            Some(setting) => {
                connection_tracker_capacity = setting.connection_tracker_capacity;
                if let Some(_num) = setting.workers {
                    may::config().set_workers(4);
                }
                if let Some(freq) = setting.stats_update_frequency {
                    stats_update_frequency = freq;
                }
            }
            None => {}
        }

        match conf.base.backends.get(&front.backend) {
            Some(back) => {
                for (_, addr) in &back.servers {
                    let listen_addr: SocketAddr = FromStr::from_str(&addr.addr)
                        .ok()
                        .expect("Failed to parse listen host:port string");
                    backend_servers.insert(listen_addr, addr.weight);
                }
                if back.health_check_interval > 0 {
                    health_check_interval = back.health_check_interval;
                }
            }
            None => error!(
                "Error finding backend server pool in config: {} not found on backend config",
                front.backend
            ),
        };

        if backend_servers.len() > 0 {
            let listen_addr: SocketAddr = FromStr::from_str(&front.listen_addr)
                .ok()
                .expect("Failed to parse listen host:port string");

            let backend = Arc::new(Backend::new(
                front.backend.clone(),
                backend_servers,
                health_check_interval,
            ));
            match listen_addr.ip() {
                IpAddr::V4(ip4) => {
                    let new_lb = LB {
                        name: frontend_name.clone(),
                        listen_ip: ip4,
                        listen_port: listen_addr.port(),
                        backend: backend.clone(),
                        conn_tracker: Arc::new(RwLock::new(
                            LruCache::<Client, Connection>::with_capacity(
                                connection_tracker_capacity,
                            ),
                        )),
                        port_mapper: Arc::new(RwLock::new(HashMap::new())),
                        next_port: Arc::new(Mutex::new(EPHEMERAL_PORT_LOWER)),
                        dsr: dsr,
                        stats_update_frequency: stats_update_frequency,
                    };
                    return Some(new_lb);
                }
                _ => {
                    error!(
                        "Unable to configure load balancer server {:?}.  Only Ipv4 is supported",
                        front
                    );
                    return None;
                }
            }
        } else {
            error!("Unable to configure load balancer server {:?}", front);
            return None;
        }
    }

    pub fn next_avail_port(&mut self) -> u16 {
        let mut port = self.next_port.lock().unwrap();
        if *port < EPHEMERAL_PORT_UPPER {
            *port += 1;
        } else {
            *port = EPHEMERAL_PORT_LOWER;
        }
        *port
    }

    // handle repsonse packets from a backend server passing back through the loadbalancer
    pub fn server_response_handler(
        &mut self,
        ip_header: &Ipv4Packet,
        tcp_header: &mut MutableTcpPacket,
        client_addr: &SocketAddr,
        tx: may::sync::mpsc::Sender<MutableIpv4Packet>,
    ) -> Option<StatsMssg> {
        match client_addr.ip() {
            IpAddr::V4(client_ipv4) => {
                let mut mssg = StatsMssg {
                    frontend: None,
                    backend: self.backend.name.clone(),
                    connections: 0,
                    bytes_tx: 0,
                    bytes_rx: 0,
                    servers: None,
                };

                let mut new_ipv4 = MutableIpv4Packet::owned(ip_header.packet().to_vec()).unwrap();
                tcp_header.set_destination(client_addr.port());
                tcp_header.set_source(self.listen_port);
                tcp_header.set_checksum(tcp::ipv4_checksum(
                    &tcp_header.to_immutable(),
                    &self.listen_ip,
                    &client_ipv4,
                ));

                new_ipv4
                    .set_total_length(tcp_header.packet().len() as u16 + IPV4_HEADER_LEN as u16);
                new_ipv4.set_version(4);
                new_ipv4.set_ttl(225);
                new_ipv4.set_next_level_protocol(IpNextHeaderProtocols::Tcp);
                new_ipv4.set_payload(&tcp_header.packet());
                new_ipv4.set_destination(client_ipv4);
                new_ipv4.set_source(self.listen_ip);
                new_ipv4.set_header_length(5);
                new_ipv4.set_checksum(checksum(&new_ipv4.to_immutable()));
                mssg.bytes_rx = tcp_header.payload().len() as u64;

                match tx.send(new_ipv4) {
                    Ok(n) => {
                        debug!(
                            "Client handler sent {:?} packet to outgoing interface handler thread",
                            n
                        );
                        match tcp_header.get_flags() {
                            0b000010010 => mssg.connections = 1, // add a connection to count on SYN,ACK
                            0b000010001 => mssg.connections = -1, // sub a connection to count on FIN,ACK
                            _ => {}
                        }
                        return Some(mssg);
                    }
                    Err(e) => error!("failed to send packet to {:?}: Error: {}", client_addr, e),
                }
            }
            _ => {} // ipv6 not supported (yet)
        }
        return None;
    }

    // handle requests packets from a client
    pub fn client_handler(
        &mut self,
        ip_header: &Ipv4Packet,
        tcp_header: &mut MutableTcpPacket,
        tx: may::sync::mpsc::Sender<MutableIpv4Packet>,
    ) -> Option<StatsMssg> {
        let client_port = tcp_header.get_source();

        // setup stats update return
        let mut mssg = StatsMssg {
            frontend: None,
            backend: self.backend.name.clone(),
            connections: 0,
            bytes_tx: 0,
            bytes_rx: 0,
            servers: None,
        };

        let mut new_ipv4 = MutableIpv4Packet::owned(ip_header.packet().to_vec()).unwrap();
        new_ipv4.set_total_length(tcp_header.packet().len() as u16 + IPV4_HEADER_LEN as u16);
        new_ipv4.set_version(4);
        new_ipv4.set_ttl(225);
        new_ipv4.set_next_level_protocol(IpNextHeaderProtocols::Tcp);
        new_ipv4.set_header_length(5);

        // leave original ip source if dsr
        if !self.dsr {
            new_ipv4.set_source(self.listen_ip);
        } else {
            new_ipv4.set_source(ip_header.get_source());
        }

        //check if we are already tracking this connection
        let cli = Client {
            ip: IpAddr::V4(ip_header.get_source()),
            port: client_port,
        };

        if let Some(conn) = self.cli_connection(&cli) {
            debug!("Found existing connection {:?}", conn);
            match conn.backend_srv.host {
                IpAddr::V4(fwd_ipv4) => {
                    if self.backend.get_server_health(conn.backend_srv.clone()) {
                        tcp_header.set_destination(conn.backend_srv.port);

                        // leave original tcp source if dsr
                        if !self.dsr {
                            tcp_header.set_source(conn.ephem_port);
                            tcp_header.set_checksum(tcp::ipv4_checksum(
                                &tcp_header.to_immutable(),
                                &self.listen_ip,
                                &fwd_ipv4,
                            ));
                        } else {
                            tcp_header.set_checksum(tcp::ipv4_checksum(
                                &tcp_header.to_immutable(),
                                &ip_header.get_source(),
                                &fwd_ipv4,
                            ));
                        }

                        new_ipv4.set_payload(&tcp_header.packet());
                        new_ipv4.set_destination(fwd_ipv4);
                        new_ipv4.set_checksum(checksum(&new_ipv4.to_immutable()));

                        match tx.send(new_ipv4) {
                            Ok(n) => {
                                debug!("Client handler sent {:?} packet to outgoing interface handler thread", n);
                            }
                            Err(e) => error!("failed to send packet: {}", e),
                        }
                        mssg.bytes_tx = tcp_header.payload().len() as u64;
                        return Some(mssg);
                    } else {
                        debug!(
                            "Backend sever {:?} is no longer healthy.  Rescheduling",
                            conn.backend_srv
                        );
                        // backend server is unhealthy, remove connection from map
                        // leave in port_mapper in case there are still packets from server in flight
                        self.conn_tracker.write().unwrap().remove(&cli);
                    }
                }
                _ => return None,
            }
        }

        // Either not tracking connection yet or backend server not healthy
        // if backend server previously scheduled is not healthy this is just a best effort.  if RST is neccessary let new backend send it
        if let Some(node) = self.backend.get_server(
            IpAddr::V4(self.listen_ip),
            self.listen_port,
            IpAddr::V4(ip_header.get_source()),
            tcp_header.get_source(),
        ) {
            match node.host {
                IpAddr::V4(fwd_ipv4) => {
                    tcp_header.set_destination(node.port);

                    // leave original tcp source if dsr
                    let mut ephem_port = 0 as u16;
                    if !self.dsr {
                        // set ephemeral port for tracking connections and in case of mutiple clients using same port
                        ephem_port = self.clone().next_avail_port();
                        debug!(
                            "Using Ephemeral port {} for client connection {:?}",
                            ephem_port,
                            SocketAddr::new(IpAddr::V4(ip_header.get_source()), client_port)
                        );
                        tcp_header.set_source(ephem_port);
                        tcp_header.set_checksum(tcp::ipv4_checksum(
                            &tcp_header.to_immutable(),
                            &self.listen_ip,
                            &fwd_ipv4,
                        ));
                    } else {
                        tcp_header.set_checksum(tcp::ipv4_checksum(
                            &tcp_header.to_immutable(),
                            &ip_header.get_source(),
                            &fwd_ipv4,
                        ));
                    }

                    new_ipv4.set_payload(&tcp_header.packet());
                    new_ipv4.set_destination(fwd_ipv4);
                    new_ipv4.set_checksum(checksum(&new_ipv4.to_immutable()));

                    match tx.send(new_ipv4) {
                        Ok(n) => {
                            debug!("Client handler sent {:?} packet to outgoing interface handler thread", n);
                        }
                        Err(e) => error!("failed to send packet: {}", e),
                    }

                    mssg.bytes_tx = tcp_header.payload().len() as u64;

                    // not already tracking the connection so insert into our maps
                    let conn = Connection {
                        client: SocketAddr::new(IpAddr::V4(ip_header.get_source()), client_port),
                        backend_srv: node,
                        ephem_port: ephem_port,
                    };
                    {
                        self.conn_tracker.write().unwrap().insert(cli, conn);
                    }
                    {
                        self.port_mapper.write().unwrap().insert(
                            ephem_port,
                            Client {
                                ip: IpAddr::V4(ip_header.get_source()),
                                port: client_port,
                            },
                        );
                    }
                    return Some(mssg);
                }
                _ => return None,
            }
        } else {
            error!("Unable to find backend");
            // Send RST to client
            tcp_header.set_source(self.listen_port);
            tcp_header.set_destination(tcp_header.get_source());
            if tcp_header.get_flags() == tcp::TcpFlags::SYN {
                // reply ACK, RST
                tcp_header.set_flags(0b000010100);
            } else {
                tcp_header.set_flags(tcp::TcpFlags::RST);
            }
            tcp_header.set_acknowledgement(tcp_header.get_sequence().clone() + 1);
            tcp_header.set_sequence(0);
            tcp_header.set_window(0);
            tcp_header.set_checksum(tcp::ipv4_checksum(
                &tcp_header.to_immutable(),
                &self.listen_ip,
                &ip_header.get_source(),
            ));

            new_ipv4.set_payload(&tcp_header.packet());
            new_ipv4.set_total_length(tcp_header.packet().len() as u16 + IPV4_HEADER_LEN as u16);
            new_ipv4.set_destination(ip_header.get_source());
            new_ipv4.set_checksum(checksum(&new_ipv4.to_immutable()));

            match tx.send(new_ipv4) {
                Ok(n) => debug!(
                    "Client handler sent {:?} packet to outgoing interface handler thread",
                    n
                ),
                Err(e) => error!("failed to send packet: {}", e),
            }
            let mut connections = 0;
            if !self.dsr {
                connections = -1;
            }
            mssg.connections = connections;
            return Some(mssg);
        }
    }

    fn cli_connection(&mut self, cli: &Client) -> Option<Connection> {
        // by using a peek instead of get we can get away with a read lock
        if let Some(conn) = self.conn_tracker.read().unwrap().peek(&cli) {
            return Some(conn.clone());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    extern crate hyper;
    use self::passthrough::backend::Node;
    use self::passthrough::utils::{build_dummy_ip, EPHEMERAL_PORT_LOWER, EPHEMERAL_PORT_UPPER};
    use crate::config::Config;
    use crate::passthrough;
    use pnet::packet::ipv4::MutableIpv4Packet;
    use pnet::packet::tcp::{MutableTcpPacket, TcpPacket};
    use pnet::packet::Packet;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::mpsc::channel;

    #[test]
    fn test_new_lb() {
        let conf = Config::new("testdata/passthrough_test.toml").unwrap();
        let test_lb = passthrough::LB::new("tcp_3000".to_string(), conf, false).unwrap();

        let ip: Ipv4Addr = "127.0.0.1".parse().unwrap();
        assert_eq!(test_lb.listen_ip, ip);
        assert_eq!(test_lb.listen_port, 3000);
        assert_eq!(test_lb.name, "tcp_3000");
    }

    #[test]
    fn test_next_port() {
        let conf = Config::new("testdata/passthrough_test.toml").unwrap();
        let mut test_lb = passthrough::LB::new("tcp_3000".to_string(), conf, false).unwrap();

        let first_port = test_lb.next_avail_port();
        assert_eq!(*test_lb.next_port.lock().unwrap(), first_port);
        assert_eq!(test_lb.next_avail_port(), first_port + 1);
        *test_lb.next_port.lock().unwrap() = EPHEMERAL_PORT_UPPER + 1;
        assert_eq!(test_lb.next_avail_port(), EPHEMERAL_PORT_LOWER);
    }

    #[test]
    fn test_next_client_handler() {
        let conf = Config::new("testdata/passthrough_test.toml").unwrap();
        let mut test_lb = passthrough::LB::new("tcp_3000".to_string(), conf, false).unwrap();

        let (tx, rx) = may::sync::mpsc::channel();
        let lb_ip = "127.0.0.1".parse().unwrap();
        let client_ip: Ipv4Addr = "9.9.9.9".parse().unwrap();
        let backend_srv_ip: Ipv4Addr = "127.0.0.1".parse().unwrap();

        {
            // set a backend server to healthy
            let mut srvs_map = test_lb.backend.servers_map.write().unwrap();
            let mut srvs_ring = test_lb.backend.ring.lock().unwrap();
            let health = srvs_map
                .get_mut(&SocketAddr::new(IpAddr::V4(backend_srv_ip), 3080))
                .unwrap();
            *health = true;
            srvs_ring.add_node(&Node {
                host: IpAddr::V4(backend_srv_ip),
                port: 3080,
            })
        }

        // gen test ip/tcp packet with simulated client
        let req_header = build_dummy_ip(client_ip, lb_ip, 43000, 3000);
        let mut tcp_header = MutableTcpPacket::owned(req_header.payload().to_owned()).unwrap();

        // call client_handler and verify packet being sent out to healthy backend server
        test_lb.client_handler(&mut req_header.to_immutable(), &mut tcp_header, tx.clone());
        let fwd_pkt: MutableIpv4Packet = rx.recv().unwrap();
        assert_eq!(fwd_pkt.get_destination(), backend_srv_ip);
        assert_eq!(fwd_pkt.get_source(), lb_ip);

        let tcp_resp = TcpPacket::new(fwd_pkt.payload()).unwrap();
        assert_eq!(tcp_resp.get_destination(), 3080);
        assert_eq!(tcp_resp.get_source(), EPHEMERAL_PORT_LOWER + 1);

        {
            // check connection is being tracked
            let port_mp = test_lb.port_mapper.read().unwrap();
            let cli = port_mp.get(&(EPHEMERAL_PORT_LOWER + 1)).unwrap();

            let mut test_lb = test_lb.conn_tracker.write().unwrap();
            let conn = test_lb.get(&cli).unwrap();
            assert_eq!(conn.ephem_port, EPHEMERAL_PORT_LOWER + 1);
            assert_eq!(conn.client, SocketAddr::new(IpAddr::V4(client_ip), 43000));
        }

        {
            assert_eq!(test_lb.conn_tracker.read().unwrap().len(), 1);
        }

        {
            // check same client again to verify connection tracker is used
            let mut tcp_header = MutableTcpPacket::owned(req_header.payload().to_owned()).unwrap();
            test_lb.client_handler(&mut req_header.to_immutable(), &mut tcp_header, tx.clone());
            // next port should not have incremented
            assert_eq!(*test_lb.next_port.lock().unwrap(), EPHEMERAL_PORT_LOWER + 1);

            let fwd_pkt: MutableIpv4Packet = rx.recv().unwrap();
            assert_eq!(fwd_pkt.get_destination(), backend_srv_ip);
            assert_eq!(fwd_pkt.get_source(), lb_ip);

            let tcp_resp = TcpPacket::new(fwd_pkt.payload()).unwrap();
            assert_eq!(tcp_resp.get_destination(), 3080);
            assert_eq!(tcp_resp.get_source(), EPHEMERAL_PORT_LOWER + 1);
            assert_eq!(test_lb.conn_tracker.read().unwrap().len(), 1);
        }

        {
            // set backend server to unhealthy
            let mut srvs_map = test_lb.backend.servers_map.write().unwrap();
            let mut srvs_ring = test_lb.backend.ring.lock().unwrap();
            let health = srvs_map
                .get_mut(&SocketAddr::new(IpAddr::V4(backend_srv_ip), 3080))
                .unwrap();
            *health = false;
            srvs_ring.remove_node(&Node {
                host: IpAddr::V4(backend_srv_ip),
                port: 3080,
            })
        }

        // check same client again to verify connection is failed
        let mut tcp_header = MutableTcpPacket::owned(req_header.payload().to_owned()).unwrap();
        test_lb.client_handler(&mut req_header.to_immutable(), &mut tcp_header, tx.clone());
        // since there are not healthy backend servers there should be no connections added to map
        assert_eq!(test_lb.conn_tracker.read().unwrap().len(), 0);
    }

    #[test]
    fn test_passthrough_server_response() {
        let conf = Config::new("testdata/passthrough_test.toml").unwrap();
        let srv = passthrough::Server::new(conf, false);
        let mut lb = srv.lbs[0].clone();

        let (tx, rx) = may::sync::mpsc::channel();
        let lb_ip = "127.0.0.1".parse().unwrap();
        let client_ip: Ipv4Addr = "9.9.9.9".parse().unwrap();
        let backend_srv_ip: Ipv4Addr = "8.8.8.8".parse().unwrap();

        // simulate response from server at port 80 to local (ephemeral) port 35000
        let resp_header = build_dummy_ip(backend_srv_ip, lb_ip, 80, 35000);
        let mut tcp_header = MutableTcpPacket::owned(resp_header.payload().to_owned()).unwrap();
        // server should respond to client ip at client's port
        lb.server_response_handler(
            &resp_header.to_immutable(),
            &mut tcp_header,
            &SocketAddr::new(IpAddr::V4(client_ip), 55000),
            tx,
        );
        let srv_resp: MutableIpv4Packet = rx.recv().unwrap();
        assert_eq!(srv_resp.get_destination(), client_ip);
        assert_eq!(srv_resp.get_source(), lb_ip);

        let tcp_resp = TcpPacket::new(srv_resp.payload()).unwrap();
        assert_eq!(tcp_resp.get_destination(), 55000);
        assert_eq!(tcp_resp.get_source(), 3000);
    }
}
