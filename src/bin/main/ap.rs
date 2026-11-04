//! The setup Wi-Fi: access point, its DHCP and DNS (captive portal) servers.

use super::*;
use embassy_futures::select::{Either3, select3};

/// Free heap the Wi-Fi start needs (driver ~47 KB, web buffers, margin).
const WIFI_HEAP_NEEDED: usize = 52 * 1024;

/// Set when the BOOT button asks for the setup Wi-Fi.
pub(crate) static AP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Set once the setup Wi-Fi is running.
pub(crate) static AP_UP: AtomicBool = AtomicBool::new(false);

/// The network stack of the setup Wi-Fi, once it runs.
pub(crate) static AP_STACK: OnceLock<ApStack> = OnceLock::new();

/// `Stack` is not `Sync` because it is meant for a single executor. All tasks
/// here run on that one executor on core 0, so sharing it is sound.
pub(crate) struct ApStack(pub(crate) Stack<'static>);

unsafe impl Send for ApStack {}

unsafe impl Sync for ApStack {}

#[embassy_executor::task]
pub(crate) async fn ap_net_task(mut runner: ApRunner) -> ! {
    runner.run().await
}

/// Makes the stack ignore connection attempts it cannot take instead of
/// refusing them. smoltcp has no listen backlog: a SYN that finds no
/// listening socket is answered with RST, and the browser reports "load
/// failed" at once. smoltcp never sends that RST for a packet a raw socket
/// has seen, so a raw TCP socket (tiny buffer, never read, it just drops the
/// copies) turns the refusal into silence: the browser resends the SYN
/// after ~1 s, by when a web socket is usually free again.
///
/// Side effect: no RST at all on this network, so connections to ports
/// without a service, or left over from before a restart, time out instead
/// of failing at once. Every response here closes its connection, so this
/// does not affect the web interface.
#[embassy_executor::task(pool_size = 2)]
pub(crate) async fn no_rst_task(stack: Stack<'static>) -> ! {
    use embassy_net::raw::{IpProtocol, IpVersion, PacketMetadata as RawMeta, RawSocket};
    let mut rx_meta = [RawMeta::EMPTY; 1];
    let mut rx_buf = [0u8; 1];
    let mut tx_meta = [RawMeta::EMPTY; 1];
    let mut tx_buf = [0u8; 1];
    let _socket = RawSocket::new(
        stack,
        Some(IpVersion::Ipv4),
        Some(IpProtocol::Tcp),
        &mut rx_meta,
        &mut rx_buf,
        &mut tx_meta,
        &mut tx_buf,
    );
    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}

/// The board's address on the setup Wi-Fi as text, for the captive portal.
const AP_HOST: &str = "192.168.4.1";
/// The portal page (DHCP option 114).
const AP_URL: &str = "http://192.168.4.1/";

const _: () = assert!(
    config::AP_IP.octets()[0] == 192
        && config::AP_IP.octets()[1] == 168
        && config::AP_IP.octets()[2] == 4
        && config::AP_IP.octets()[3] == 1,
    "update AP_HOST and AP_URL together with AP_IP"
);

/// Answers every DNS query on the setup Wi-Fi with the board's address. Phones
/// check for internet access by fetching a known page; getting the board's
/// page instead makes them show "sign in to network" and open it.
#[embassy_executor::task]
pub(crate) async fn dns_task(stack: Stack<'static>) {
    static BUFFERS: ConstStaticCell<edge_nal_embassy::UdpBuffers<1, 512, 512, 4>> =
        ConstStaticCell::new(edge_nal_embassy::UdpBuffers::new());
    let udp = edge_nal_embassy::Udp::new(stack, BUFFERS.take());
    let mut tx = [0u8; 512];
    let mut rx = [0u8; 512];
    let local = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 53));
    loop {
        if let Err(e) = edge_captive::io::run(
            &udp,
            local,
            &mut tx,
            &mut rx,
            config::AP_IP,
            core::time::Duration::from_secs(60),
        )
        .await
        {
            warn!("dns: {:?}", e);
        }
        Timer::after(Duration::from_secs(1)).await;
    }
}

/// The access point configuration for the current settings.
pub(crate) fn ap_config(mac: [u8; 6]) -> Option<(AccessPointConfig, alloc::string::String)> {
    let s = settings::current();
    let ssid = s.name(mac);
    let (Ok(ssid_cfg), Ok(pw)) = (ssid.as_str().try_into(), s.password().try_into()) else {
        error!("wifi: invalid name or password for the access point");
        return None;
    };
    let cfg = AccessPointConfig::default()
        .with_ssid(ssid_cfg)
        .with_channel(s.ap_channel)
        .with_authentication(AuthenticationMethodConfig::Wpa2Personal(pw));
    Some((cfg, ssid))
}

/// Why the setup Wi-Fi starts (or None to keep waiting). `boot_window`:
/// the start-up window from the settings is still to be used.
fn ap_trigger(eth: Option<Stack<'static>>, link_down_ms: u64, boot_window: bool) -> Option<&'static str> {
    if eth.is_none() {
        return Some("no Ethernet module");
    }
    if boot_window {
        return Some("start-up window");
    }
    if link_down_ms >= u64::from(settings::ap_after_s()) * 1000 {
        return Some("no Ethernet link");
    }
    if AP_REQUESTED.swap(false, Ordering::Relaxed) {
        return Some("BOOT button held");
    }
    if status::AP_REQUEST.swap(false, Ordering::Relaxed) {
        return Some("requested from the web interface");
    }
    None
}

/// Starts the setup Wi-Fi when the Ethernet link has been down for a while,
/// when the BOOT button is held, when the web interface asks, or for the
/// start-up window (settings: minutes after each start). It serves the web
/// interface and also receives Art-Net. Changes to name, password or channel
/// are applied to the running access point.
///
/// Started for the start-up window, it switches off once the window is over
/// and nobody used the web interface over it for the idle time: the driver
/// goes to station mode, which sends nothing while not connected (it keeps
/// its memory, so starting again needs none). Any later trigger starts the
/// access point again. Started for any other reason, it runs until the next
/// restart. Either way the LED memory it took stays with it until the next
/// restart.
#[embassy_executor::task]
pub(crate) async fn ap_task(spawner: Spawner, wifi: WIFI<'static>, eth: Option<Stack<'static>>, ctx: &'static WebCtx) {
    const TICK_MS: u64 = 100;
    let mut wifi = Some(wifi);
    let mut controller: Option<WifiController<'static>> = None;
    let mut boot_window = settings::ap_boot_min() > 0;
    let mut first = true;
    loop {
        let mut link_down_ms = 0;
        let reason = loop {
            if let Some(r) = ap_trigger(eth, link_down_ms, boot_window) {
                break r;
            }
            Timer::after(Duration::from_millis(TICK_MS)).await;
            link_down_ms = if link_up(eth) { 0 } else { link_down_ms + TICK_MS };
        };
        let windowed = reason == "start-up window";
        boot_window = false;

        if first {
            // The Wi-Fi driver needs about 47 KB of heap: take it from the
            // LED memory (outputs are capped until the next restart).
            match ledout::shrink_for_wifi().await {
                Some((start, len)) => {
                    // SAFETY: the LED side no longer touches this part of the
                    // pool and never will again (the limit only goes down); it
                    // is static.
                    unsafe {
                        esp_alloc::HEAP.add_region(esp_alloc::HeapRegion::new(start, len, esp_alloc::MemoryCapability::Internal.into()))
                    };
                    info!("wifi: {} bytes of LED memory moved to the heap", len);
                }
                None => {
                    error!("wifi: the LED core did not release its memory; not starting the setup Wi-Fi");
                    return;
                }
            }
        }

        let Some((ap_cfg, ssid)) = ap_config(ctx.mac) else {
            return;
        };
        if controller.is_none() {
            // The driver allocates ~47 KB; starting it with less panics on
            // the allocation (and without Ethernet the setup Wi-Fi starts at
            // every boot, so that would repeat). Refuse instead.
            if esp_alloc::HEAP.free() < WIFI_HEAP_NEEDED {
                error!("wifi: only {} bytes of heap free; not starting the setup Wi-Fi", esp_alloc::HEAP.free());
                status::update(|st| st.ap_reason = "not enough memory");
                return;
            }
            // The defaults (10 static + 32 dynamic RX buffers, A-MPDU) suit a
            // busy station. A setup network with a phone or two needs far
            // less, and every buffer comes out of the shared heap.
            let wifi_config = ControllerConfig::default()
                .with_static_rx_buf_num(5)
                .with_dynamic_rx_buf_num(8)
                // 16 is the smallest value ESP-IDF's own configuration allows.
                .with_dynamic_tx_buf_num(16)
                .with_ampdu_rx_enable(false)
                .with_ampdu_tx_enable(false);
            let Some(peripheral) = wifi.take() else { return };
            match WifiController::new(peripheral, wifi_config) {
                Ok(c) => controller = Some(c),
                Err(e) => {
                    error!("wifi: init failed: {:?}", e);
                    return;
                }
            }
        }
        let Some(ctrl) = controller.as_mut() else { return };
        settings::AP_CHANGED.reset();
        if let Err(e) = ctrl.set_config(&WifiConfig::AccessPoint(ap_cfg)) {
            error!("wifi: starting the access point failed: {:?}", e);
            return;
        }

        if first {
            first = false;
            // Sockets: DHCP server, DNS, Art-Net, web servers, no-RST, mDNS.
            // Exactly full: smoltcp panics when one more socket is added.
            // (The resources are small; the socket buffers live elsewhere.)
            static AP_RESOURCES: StaticCell<StackResources<{ 5 + web::SOCKETS }>> = StaticCell::new();
            let rng = Rng::new();
            let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
            let (ap, ap_runner) = embassy_net::new(
                WifiInterface::access_point(),
                embassy_net::Config::ipv4_static(StaticConfigV4 {
                    address: Ipv4Cidr::new(config::AP_IP, 24),
                    gateway: None,
                    dns_servers: Default::default(),
                }),
                AP_RESOURCES.init(StackResources::new()),
                seed,
            );
            spawner.spawn(ap_net_task(ap_runner).unwrap());
            spawner.spawn(no_rst_task(ap).unwrap());
            spawner.spawn(dhcp_server_task(ap).unwrap());
            // The web server buffers come from the heap, only once the setup
            // network runs. Art-Net needs nothing extra: its receiver moves
            // over to this network while the Ethernet link is down.
            for _ in 0..web::SOCKETS {
                let rx = alloc::vec![0u8; web::RX_LEN].leak();
                let tx = alloc::vec![0u8; web::TX_LEN].leak();
                spawner.spawn(web_task(ap, rx, tx, ctx, Some(AP_HOST)).unwrap());
            }
            spawner.spawn(dns_task(ap).unwrap());
            let _ = AP_STACK.init(ApStack(ap));
        }
        info!("heap after Wi-Fi start: {}", esp_alloc::HEAP.stats());
        AP_UP.store(true, Ordering::Relaxed);
        status::update(|st| {
            st.ap_ssid = Some(ssid.clone());
            st.ap_reason = reason;
        });
        info!(
            "wifi: setup network \"{}\" up ({}), web interface at http://{}/",
            ssid, reason, config::AP_IP
        );
        let window_end = Instant::now() + Duration::from_secs(u64::from(settings::ap_boot_min()) * 60);

        // Keep the controller alive (dropping it would stop the radio), and
        // apply changed settings to the running access point.
        loop {
            match select3(
                ctrl.wait_for_access_point_connected_event_async(),
                settings::AP_CHANGED.wait(),
                Timer::after(Duration::from_secs(1)),
            )
            .await
            {
                Either3::First(Ok(ev)) => info!("wifi: {:?}", ev),
                Either3::First(Err(e)) => {
                    warn!("wifi: event error: {:?}", e);
                    Timer::after(Duration::from_secs(1)).await;
                }
                Either3::Second(()) => {
                    let Some((cfg, ssid)) = ap_config(ctx.mac) else {
                        continue;
                    };
                    match ctrl.set_config(&WifiConfig::AccessPoint(cfg)) {
                        Ok(()) => {
                            info!("wifi: setup network changed to \"{}\"", ssid);
                            status::update(|st| st.ap_ssid = Some(ssid));
                        }
                        Err(e) => {
                            // esp-radio stops the radio after a failed change.
                            // Mark the network as gone so Art-Net and the
                            // status page stop using it; a restart brings it
                            // back.
                            error!("wifi: applying the new settings failed ({:?}); setup Wi-Fi is off until restart", e);
                            AP_UP.store(false, Ordering::Relaxed);
                            status::update(|st| st.ap_ssid = None);
                            return;
                        }
                    }
                }
                Either3::Third(()) => {
                    let idle_ms = u64::from(settings::ap_idle_min()) * 60_000;
                    if windowed && Instant::now() >= window_end && u64::from(web::ap_web_idle_ms()) >= idle_ms {
                        break;
                    }
                }
            }
        }
        // End of the start-up window: station mode, not connected, sends
        // nothing; the driver keeps its memory for the next start.
        AP_UP.store(false, Ordering::Relaxed);
        if let Err(e) = ctrl.set_config(&WifiConfig::Station(esp_radio::wifi::sta::StationConfig::default())) {
            warn!("wifi: switching the access point off failed: {:?}", e);
        }
        status::update(|st| {
            st.ap_ssid = None;
            st.ap_reason = "off after the start-up window";
        });
        info!("wifi: start-up window over; setup Wi-Fi off");
    }
}

/// Hands out 192.168.4.50 and up to clients of the setup Wi-Fi.
#[embassy_executor::task]
pub(crate) async fn dhcp_server_task(stack: Stack<'static>) {
    use edge_dhcp::{
        io::server::run,
        server::{Server, ServerOptions},
    };
    use edge_nal::UdpBind;

    static BUFFERS: ConstStaticCell<edge_nal_embassy::UdpBuffers<1, 1024, 1024, 4>> =
        ConstStaticCell::new(edge_nal_embassy::UdpBuffers::new());
    let udp = edge_nal_embassy::Udp::new(stack, BUFFERS.take());
    let mut buf = [0u8; 1024];
    let mut gateway = [Ipv4Addr::UNSPECIFIED; 1];
    let mut options = ServerOptions::new(config::AP_IP, Some(&mut gateway));
    // The board is the DNS server (see `dns_task`), and option 114 (RFC 8910)
    // tells newer phones the portal address directly.
    let dns = [config::AP_IP];
    options.dns = &dns;
    options.captive_url = Some(AP_URL);
    let mut server = Server::<_, 8>::new(|| Instant::now().as_secs(), config::AP_IP);
    loop {
        let bind = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 67));
        match udp.bind(bind).await {
            Ok(mut socket) => {
                if let Err(e) = run(&mut server, &options, &mut socket, &mut buf).await {
                    warn!("dhcp server: {:?}", e);
                }
            }
            Err(e) => warn!("dhcp server: bind failed: {:?}", e),
        }
        Timer::after(Duration::from_secs(1)).await;
    }
}
