//! Ethernet: link state, addressing (DHCP with the Art-Net 2.x fallback or static), which network is active, and the mDNS responder.

use super::*;

/// Keeps the Ethernet link state on the status page current. Polls instead
/// of waiting: the stack's wait functions share one waker, and a second task
/// waiting on it would make both wake each other endlessly.
#[embassy_executor::task]
pub(crate) async fn eth_link_task(eth: Stack<'static>) -> ! {
    loop {
        let up = eth.is_link_up();
        status::update(|st| st.eth_link = up);
        Timer::after(Duration::from_millis(250)).await;
    }
}

/// Applies the Ethernet address settings, again whenever they change.
pub(crate) async fn eth_addressing(eth: Stack<'static>, mac: [u8; 6]) -> ! {
    // Art-Net's default: 2.x.y.z/8 from the MAC.
    let artnet = Ipv4Addr::new(2, mac[3], mac[4], mac[5].clamp(1, 254));
    loop {
        settings::ETH_CHANGED.reset();
        let s = settings::current();
        let fallback = match s.fallback_ip {
            Some(ip) => (ip, s.fallback_prefix),
            None => (artnet, 8),
        };
        // Never hand the stack an address it panics on (e.g. a damaged
        // record); the settings page already rejects those.
        let mode = if s.eth_mode == EthMode::Static && !settings::usable_ip(s.ip) {
            error!("Static IP {} is not usable; using DHCP", s.ip);
            EthMode::Dhcp
        } else {
            s.eth_mode
        };
        match mode {
            EthMode::Static => {
                eth.set_config_v4(ConfigV4::Static(StaticConfigV4 {
                    address: Ipv4Cidr::new(s.ip, s.prefix),
                    gateway: s.gateway,
                    dns_servers: Default::default(),
                }));
                status::update(|st| {
                    st.eth_addr = Some((s.ip, s.prefix));
                    st.eth_source = "static";
                });
                info!("Ethernet IP {}/{} (static)", s.ip, s.prefix);
                settings::ETH_CHANGED.wait().await;
            }
            EthMode::Dhcp => {
                let _ = select(
                    dhcp_with_fallback(eth, fallback, s.artnet_fallback),
                    settings::ETH_CHANGED.wait(),
                )
                .await;
            }
        }
        info!("Ethernet settings changed; applying");
    }
}

pub(crate) fn set_eth_status(addr: Option<(Ipv4Addr, u8)>, source: &'static str) {
    status::update(|st| {
        st.eth_addr = addr;
        st.eth_source = source;
    });
}

/// DHCP, and (if enabled) the Art-Net default 2.x.y.z/8 when no DHCP server
/// answers or the lease is lost. On the fallback address, DHCP is tried
/// again after every link loss. Meanwhile a probe looks for a server every
/// minute without dropping the address (see [`dhcp_server_answers`]); only
/// when one answers does the real client take over. That also covers
/// switches that block a new port for ~30 s.
pub(crate) async fn dhcp_with_fallback(eth: Stack<'static>, (fallback, prefix): (Ipv4Addr, u8), use_fallback: bool) -> ! {
    // Note: `set_config_v4` does not wake the network runner by itself; the
    // Art-Net task polls its socket every 250 ms, which does.
    eth.set_config_v4(ConfigV4::Dhcp(Default::default()));
    set_eth_status(None, "DHCP");
    let mut on_fallback = false;
    loop {
        eth.wait_link_up().await;
        info!("Ethernet link up");
        loop {
            if on_fallback {
                eth.set_config_v4(ConfigV4::Dhcp(Default::default()));
                set_eth_status(None, "DHCP");
                on_fallback = false;
            }
            let dhcp = async {
                if use_fallback {
                    with_timeout(Duration::from_millis(config::DHCP_TIMEOUT_MS), eth.wait_config_up()).await
                } else {
                    eth.wait_config_up().await;
                    Ok(())
                }
            };
            match select(dhcp, eth.wait_link_down()).await {
                Either::Second(()) => break,
                Either::First(Ok(())) if eth.config_v4().is_some() => {
                    if let Some(cfg) = eth.config_v4() {
                        info!("Ethernet IP {} (DHCP)", cfg.address);
                        set_eth_status(Some((cfg.address.address(), cfg.address.prefix_len())), "DHCP");
                    }
                    match select(eth.wait_link_down(), eth.wait_config_down()).await {
                        Either::First(()) => break,
                        Either::Second(()) => {
                            warn!("DHCP lease lost");
                            set_eth_status(None, "DHCP");
                        }
                    }
                }
                Either::First(_) => {
                    eth.set_config_v4(ConfigV4::Static(StaticConfigV4 {
                        address: Ipv4Cidr::new(fallback, prefix),
                        gateway: None,
                        dns_servers: Default::default(),
                    }));
                    on_fallback = true;
                    set_eth_status(Some((fallback, prefix)), "fallback");
                    warn!("No DHCP answer; Ethernet IP {}/{} (fallback)", fallback, prefix);
                    // Stay until the link drops or a DHCP server answers. The
                    // real client drops the address for up to DHCP_TIMEOUT_MS,
                    // which closes every connection; the probe keeps it.
                    loop {
                        match select(eth.wait_link_down(), Timer::after(DHCP_RETRY)).await {
                            Either::First(()) => break,
                            Either::Second(()) if dhcp_server_answers(eth).await => {
                                info!("A DHCP server answered; switching to DHCP");
                                break;
                            }
                            Either::Second(()) => {}
                        }
                    }
                    if !eth.is_link_up() {
                        break;
                    }
                }
            }
        }
        warn!("Ethernet link down");
        if !on_fallback {
            set_eth_status(None, "DHCP");
        }
    }
}

/// How often the fallback address looks for a DHCP server.
const DHCP_RETRY: Duration = Duration::from_secs(60);

/// Sends one DHCPDISCOVER from the current (fallback) address and waits a
/// few seconds for an offer. Keeps the address, so connections stay up.
async fn dhcp_server_answers(eth: Stack<'static>) -> bool {
    use pixelcast_core::dhcp;
    let embassy_net::HardwareAddress::Ethernet(hw) = eth.hardware_address();
    let mac = hw.0;
    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx_buf = [0u8; 600];
    let mut tx_meta = [PacketMetadata::EMPTY; 1];
    let mut tx_buf = [0u8; dhcp::DISCOVER_LEN];
    let mut socket = UdpSocket::new(eth, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    if socket.bind(dhcp::CLIENT_PORT).is_err() {
        return false;
    }
    let xid = Rng::new().random();
    let to = embassy_net::IpEndpoint::new(embassy_net::Ipv4Address::BROADCAST.into(), dhcp::SERVER_PORT);
    if socket.send_to(&dhcp::discover(xid, mac), to).await.is_err() {
        return false;
    }
    let mut buf = [0u8; 600];
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match embassy_time::with_deadline(deadline, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) if dhcp::is_offer(&buf[..n], xid, mac) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}

/// The network Art-Net is received on: Ethernet while its link is up,
/// otherwise the setup Wi-Fi if it runs. Only one at a time, so two senders
/// can never write into the same frame.
pub(crate) fn active(eth: Option<Stack<'static>>) -> Option<(Stack<'static>, Net)> {
    if !link_up(eth)
        && AP_UP.load(Ordering::Relaxed)
        && let Some(ap) = AP_STACK.try_get()
    {
        return Some((ap.0, Net::Wifi));
    }
    eth.map(|e| (e, Net::Eth))
}

/// The network the LED data comes in on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Net {
    Eth,
    /// The setup Wi-Fi.
    Wifi,
}

impl Net {
    /// For the status page and the log.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Net::Eth => "eth",
            Net::Wifi => "wifi",
        }
    }
}

impl core::fmt::Display for Net {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.key())
    }
}

pub(crate) fn link_up(eth: Option<Stack<'static>>) -> bool {
    eth.is_some_and(|e| e.is_link_up())
}

/// mDNS for `<name>.local` on the active network: answers A queries
/// (multicast) and announces every 60 s (TTL 120 s). An earlier listening
/// version seemed to cost Art-Net packets; that was core 0 at its limit,
/// and since Art-Net is handled in the W5500 driver it costs nothing
/// measurable (72 fps at 0 % loss under 100 queries/s).
#[embassy_executor::task]
pub(crate) async fn mdns_task(eth: Option<Stack<'static>>) -> ! {
    use esp32_pixelcast::mdns;
    // Buffers from the heap (normal RAM has no room left for them).
    let rx_meta = alloc::vec![PacketMetadata::EMPTY; 2].leak();
    let rx_buf = alloc::vec![0u8; 512].leak();
    let tx_meta = alloc::vec![PacketMetadata::EMPTY; 2].leak();
    let tx_buf = alloc::vec![0u8; 256].leak();
    let mut query = [0u8; 512];
    let mut out = [0u8; 128];
    let group = embassy_net::Ipv4Address::from(mdns::GROUP);
    let to_group = embassy_net::IpEndpoint::new(group.into(), mdns::PORT);
    loop {
        Timer::after(Duration::from_secs(1)).await;
        let Some((stack, net)) = active(eth) else { continue };
        let Some(cfg) = stack.config_v4() else { continue };
        let ip = cfg.address.address().octets();
        let mut socket = UdpSocket::new(stack, &mut *rx_meta, &mut *rx_buf, &mut *tx_meta, &mut *tx_buf);
        if socket.bind(mdns::PORT).is_err() {
            continue;
        }
        if let Err(e) = stack.join_multicast_group(group) {
            warn!("mdns: joining 224.0.0.251 failed: {:?}", e);
        }
        let base_mac = board::base_mac();
        let name = settings::with(|s| s.name(base_mac));
        let host = mdns::host_label(&name);
        info!("mdns: answering for {}.local on {}", core::str::from_utf8(host.as_bytes()).unwrap_or("?"), net);
        // Announce twice on start, as mDNS asks for, then every 60 s.
        let mut announce = 2u8;
        let mut next = Instant::now();
        loop {
            if Instant::now() >= next {
                let n = mdns::record(host.as_bytes(), ip, &mut out);
                let _ = socket.send_to(&out[..n], to_group).await;
                announce = announce.saturating_sub(1);
                next = Instant::now() + Duration::from_secs(if announce > 0 { 1 } else { 60 });
            }
            match select(socket.recv_from(&mut query), Timer::at(next.min(Instant::now() + Duration::from_secs(1)))).await {
                Either::First(Ok((len, meta))) => {
                    if let Some(n) = mdns::answer(&query[..len], host.as_bytes(), ip, &mut out) {
                        // Multicast answer; a query not from port 5353 (a plain
                        // DNS client asking the group) also gets it directly.
                        let _ = socket.send_to(&out[..n], to_group).await;
                        if meta.endpoint.port != mdns::PORT {
                            let _ = socket.send_to(&out[..n], meta.endpoint).await;
                        }
                    }
                }
                Either::First(Err(_)) => {}
                Either::Second(()) => {}
            }
            // Start over when the network, the address or the name changes.
            let now_ip = stack.config_v4().map(|c| c.address.address().octets());
            if active(eth).map(|a| a.1) != Some(net)
                || now_ip != Some(ip)
                || mdns::host_label(&settings::with(|s| s.name(base_mac))).as_bytes() != host.as_bytes()
            {
                break;
            }
        }
        let _ = stack.leave_multicast_group(group);
    }
}
