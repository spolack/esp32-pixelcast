//! Receiving Art-Net/sACN: the receive task (ArtPoll, ArtAddress, sACN priorities), the handler shared with the Ethernet fast path (merge, statistics, live view), and the fast path itself.

use super::*;
use embassy_futures::select::{Either3, select3};

/// Art-Net statistics window (the live page shows each window).
const REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// Every this many windows, the statistics also go to the log.
const LOG_EVERY: u32 = 5;

/// Uptime in seconds when the last LED data (DMX or Sync) arrived (0 = never).
pub(crate) static LAST_ARTNET_S: AtomicU32 = AtomicU32::new(0);

pub(crate) fn artnet_idle_for(d: Duration) -> bool {
    let last = LAST_ARTNET_S.load(Ordering::Relaxed);
    last == 0 || Instant::now().as_secs().saturating_sub(u64::from(last)) >= d.as_secs()
}

/// Answers an ArtPoll: one ArtPollReply per group of up to four output
/// universes, so lighting software lists the node with its name and ports.
pub(crate) async fn answer_poll(
    socket: &UdpSocket<'_>,
    stack: Stack<'static>,
    net: Net,
    to: embassy_net::IpEndpoint,
    polls: u16,
    merging: &[u16],
) {
    let Some(cfg) = stack.config_v4() else {
        return;
    };
    let s = settings::current();
    let mac = if net == Net::Eth { board::eth_mac() } else { board::ap_mac() };
    let name = s.name(board::base_mac());
    let long = alloc::format!("{} (esp32-pixelcast {})", name, env!("CARGO_PKG_VERSION"));
    let st = status::get();
    let (mut universes, n) = node_universes(&s);
    let longest = s.outputs.iter().map(|o| o.wire_bytes(s.led_type)).max().unwrap_or(0);
    let refresh = led::max_fps(longest, 1, s.timing.period_ns, s.latch_us).min(u32::from(u16::MAX));
    let version = {
        let mut v = env!("CARGO_PKG_VERSION").split('.').map(|p| p.parse::<u16>().unwrap_or(0));
        let (_major, minor, patch) = (v.next().unwrap_or(0), v.next().unwrap_or(0), v.next().unwrap_or(0));
        (minor << 8) | (patch & 0xff)
    };
    let node = artnet::NodeInfo {
        ip: cfg.address.address().octets(),
        mac,
        short_name: &name,
        long_name: &long,
        version,
        dhcp: net == Net::Eth && st.eth_source == "DHCP",
        refresh_hz: refresh as u16,
        polls,
        active: !artnet_idle_for(Duration::from_secs(3)),
        ltp: s.merge == artnet_merge::Mode::Ltp,
        merging,
    };
    let mut reply = [0u8; artnet::POLL_REPLY_LEN];
    let mut index = 0u8;
    let mut replies: [Option<artnet::Bind>; 4 * led::LANES] = Default::default();
    artnet::for_each_bind(&mut universes[..n], |b| {
        if let Some(slot) = replies.get_mut(usize::from(index)) {
            *slot = Some(b.clone());
        }
        index += 1;
    });
    for (i, b) in replies.iter().flatten().enumerate() {
        artnet::poll_reply(&mut reply, &node, b, i as u8 + 1);
        if let Err(e) = socket.send_to(&reply, to).await {
            warn!("artnet: ArtPollReply to {} failed: {:?}", to, e);
            return;
        }
    }
}

/// The universes the outputs listen to, in output order (at most 16).
pub(crate) fn node_universes(s: &Settings) -> ([u16; 48], usize) {
    let mut universes = [0u16; 48];
    let mut n = 0;
    for o in s.outputs.iter() {
        for k in 0..o.universes(s.led_type) {
            if n < universes.len() {
                universes[n] = o.universe.wrapping_add(k as u16) & 0x7fff;
                n += 1;
            }
        }
    }
    (universes, n)
}

/// Applies an ArtAddress: a new short name, and new port-addresses for the
/// ports of one reply group. Outputs use consecutive universes, so a port
/// that moves shifts its output's first universe. Returns whether anything
/// changed.
pub(crate) fn apply_art_address(a: &artnet::Address) -> bool {
    let mut s = settings::current();
    let old = s.clone();
    if let Some(name) = a.short_name()
        && name.len() <= 32
        && name.bytes().all(|b| (0x20..0x7f).contains(&b))
    {
        s.name = alloc::string::String::from(name);
    }
    // Merge mode: the spec sets it per port; here it applies to the node.
    if artnet::AC_MERGE_LTP.contains(&a.command) {
        s.merge = artnet_merge::Mode::Ltp;
    } else if artnet::AC_MERGE_HTP.contains(&a.command) {
        s.merge = artnet_merge::Mode::Htp;
    }
    let (mut universes, n) = node_universes(&s);
    let mut index = 0u8;
    let mut target = None;
    artnet::for_each_bind(&mut universes[..n], |b| {
        index += 1;
        if index == a.bind_index {
            target = Some(b.clone());
        }
    });
    if let Some(b) = target {
        let mut moved = [false; led::LANES];
        for (i, &port) in b.ports.as_slice().iter().enumerate() {
            let from = (b.net_sub << 4) | u16::from(port);
            let to = a.apply(i, from);
            if to == from {
                continue;
            }
            for (k, o) in s.outputs.iter_mut().enumerate() {
                let count = o.universes(old.led_type) as u16;
                if !moved[k] && from >= o.universe && from < o.universe.saturating_add(count) {
                    let j = from - o.universe;
                    if to >= j {
                        o.universe = to - j;
                        moved[k] = true;
                    }
                }
            }
        }
    }
    let changed = s != old;
    if changed {
        info!("artnet: ArtAddress applied (bind {})", a.bind_index);
        settings::set_current_keep_ap(s);
        settings::SAVE_SOON.signal(());
    }
    changed
}

/// Receives Art-Net, feeds the LED output, answers ArtPoll (discovery) and
/// keeps statistics. The socket is recreated on
/// the other network when the active one changes, reusing the same buffers.
#[embassy_executor::task]
pub(crate) async fn artnet_task(
    eth: Option<Stack<'static>>,
    rx_meta: &'static mut [PacketMetadata],
    rx_buf: &'static mut [u8],
) {
    const TICK: Duration = Duration::from_millis(250);
    // Room for the ArtPollReplies of 16 universes (4 × 239 bytes) and the
    // audio packets (sACN: 638 bytes). smoltcp keeps each packet in one
    // piece, so the ring must hold twice the largest one, or a packet that
    // does not fit before the wrap point never fits.
    let mut tx_meta = [PacketMetadata::EMPTY; 5];
    let mut tx_buf = [0u8; 2 * 640];
    let mut polls_answered = 0u16;
    // The largest packet through this socket is an sACN data packet (638
    // bytes; Art-Net DMX on Ethernet takes the fast path, 530 bytes on the
    // setup Wi-Fi). Anything longer is dropped.
    let mut buf = [0u8; 704];
    let mut window_start = Instant::now();
    let mut windows = 0u32;

    loop {
        let Some((stack, name)) = active(eth) else {
            // No Ethernet module and the setup Wi-Fi is not up yet.
            Timer::after(TICK).await;
            continue;
        };
        let proto = settings::with(|s| s.protocol);
        let port = match proto {
            settings::Protocol::ArtNet => artnet::PORT,
            settings::Protocol::Sacn => sacn::PORT,
        };
        let mut socket =
            UdpSocket::new(stack, &mut *rx_meta, &mut *rx_buf, &mut tx_meta, &mut tx_buf);
        if let Err(e) = socket.bind(port) {
            error!("receive: bind on {} failed: {:?}", name, e);
            Timer::after(Duration::from_secs(1)).await;
            continue;
        }
        // sACN is mostly sent to one multicast group per universe.
        let groups = if proto == settings::Protocol::Sacn { sacn_groups() } else { Default::default() };
        for g in groups.iter() {
            if let Err(e) = stack.join_multicast_group(embassy_net::Ipv4Address::from(*g)) {
                warn!("sacn: joining {:?} failed: {:?}", g, e);
            }
        }
        info!("receive: {} on {} (UDP {}, {} multicast groups)", proto.key(), name, port, groups.len());
        status::update(|st| st.artnet_net = name.key());
        let mut priorities = sacn::Priorities::new();
        sync_merge_mode();
        // Art-Net DMX and Sync on Ethernet are handled by the driver itself
        // (`eth_fast_path`); this task still gets everything else.
        FAST_PATH.store(proto == settings::Protocol::ArtNet && name == Net::Eth, Ordering::Relaxed);
        let mut merging = [0u16; 32];
        let mut merging_n = 0;

        let mut next_tick = Instant::now() + TICK;
        let mut last_poller: Option<embassy_net::IpAddress> = None;
        let mut repeat_poll: Option<(embassy_net::IpEndpoint, Instant)> = None;
        loop {
            // The audio universe first: under a burst the receive branch is
            // always ready and would starve it.
            let event = select3(esp32_pixelcast::audio::TX.wait(), socket.recv_from(&mut buf), Timer::at(next_tick)).await;
            match event {
                Either3::First(frame) => send_audio(&mut socket, stack, &frame).await,
                Either3::Second(Ok((len, meta))) => {
                    // sACN is translated into the Art-Net view (port-address
                    // from 0) so the LED output and the statistics see one
                    // kind of packet.
                    let packet = match proto {
                        settings::Protocol::ArtNet => artnet::parse(&buf[..len]),
                        settings::Protocol::Sacn => match sacn::parse(&buf[..len]) {
                            Some(sacn::Packet::Data { universe, priority, sequence, preview, terminated, data, .. }) => {
                                if terminated {
                                    priorities.release(universe, priority);
                                }
                                let now_ms = Instant::now().as_millis() as u32;
                                match sacn::port_address(universe) {
                                    Some(pa) if !preview && !terminated && priorities.accept(universe, priority, now_ms) => {
                                        Some(artnet::Packet::Dmx { universe: pa, sequence, data })
                                    }
                                    _ => Some(artnet::Packet::Other(0)),
                                }
                            }
                            Some(sacn::Packet::Sync { .. }) => Some(artnet::Packet::Sync),
                            None => None,
                        },
                    };
                    if let Some(artnet::Packet::Address(a)) = packet {
                        if a.command == artnet::AC_CANCEL_MERGE {
                            RX_SHARED.lock(|r| r.borrow_mut().merger.cancel());
                        }
                        apply_art_address(&a);
                        sync_merge_mode();
                        // The spec asks for an ArtPollReply in answer.
                        polls_answered = polls_answered.wrapping_add(1);
                        let to = embassy_net::IpEndpoint::new(meta.endpoint.addr, artnet::PORT);
                        answer_poll(&socket, stack, name, to, polls_answered, &merging[..merging_n]).await;
                    }
                    if let Some(artnet::Packet::Poll) = packet {
                        polls_answered = polls_answered.wrapping_add(1);
                        let to = embassy_net::IpEndpoint::new(meta.endpoint.addr, artnet::PORT);
                        answer_poll(&socket, stack, name, to, polls_answered, &merging[..merging_n]).await;
                        // A poller not seen before: the stack may have dropped
                        // the reply while it looked up its MAC. Answer again.
                        if last_poller != Some(meta.endpoint.addr) {
                            last_poller = Some(meta.endpoint.addr);
                            repeat_poll = Some((to, Instant::now() + Duration::from_millis(300)));
                        }
                    }
                    let src = match meta.endpoint.addr {
                        // By source address for both protocols: two sACN
                        // sources at the same priority merge like Art-Net.
                        embassy_net::IpAddress::Ipv4(ip) => Some(ip.octets()),
                    };
                    handle_packet(packet.as_ref(), src);
                    // Let the Ethernet driver read on: in a burst it must
                    // move frames out of the W5500 (16 KB) into the socket
                    // buffer faster than they are handled here.
                    embassy_futures::yield_now().await;
                }
                // Longer than the buffer: not a packet this node takes.
                Either3::Second(Err(embassy_net::udp::RecvError::Truncated)) => {}
                Either3::Third(()) => {}
            }
            if let Some((to, at)) = repeat_poll
                && Instant::now() >= at
            {
                repeat_poll = None;
                polls_answered = polls_answered.wrapping_add(1);
                answer_poll(&socket, stack, name, to, polls_answered, &merging[..merging_n]).await;
            }
            // Checked after every packet too: under a steady stream the timer
            // branch may never get its turn.
            let now = Instant::now();
            if now < next_tick {
                continue;
            }
            next_tick = now + TICK;
            if now >= window_start + REPORT_INTERVAL {
                let report = RX_SHARED.lock(|r| r.borrow_mut().stats.take_report((now - window_start).as_millis()));
                windows = windows.wrapping_add(1);
                if report.pps > 0 && windows % LOG_EVERY == 0 {
                    info!("[{}] {}", name, report);
                }
                status::update(|st| st.artnet = Some(report));
                window_start = now;
                merging_n = RX_SHARED.lock(|r| r.borrow().merger.merging_list(now.as_millis(), &mut merging));
                status::MERGING.store(merging_n as u32, Ordering::Relaxed);
            }
            // Rebind when the network, the protocol or (for sACN) the
            // universes to listen to change.
            sync_merge_mode();
            HEAP_LOW.store(esp_alloc::HEAP.free() < 6 * 1024, Ordering::Relaxed);
            if active(eth).map(|a| a.1) != Some(name)
                || settings::with(|s| s.protocol) != proto
                || (proto == settings::Protocol::Sacn && sacn_groups() != groups)
            {
                break;
            }
        }
        FAST_PATH.store(false, Ordering::Relaxed);
        for g in groups.iter() {
            let _ = stack.leave_multicast_group(embassy_net::Ipv4Address::from(*g));
        }
    }
}

/// Sends one frame of the audio universe from the receive socket (Art-Net
/// from 6454, as the protocol expects), with the settings in effect: unicast
/// to `dest`, else Art-Net broadcast or the sACN universe's multicast group.
async fn send_audio(socket: &mut UdpSocket<'_>, stack: Stack<'static>, frame: &esp32_pixelcast::audio::Frame) {
    let a = settings::with(|s| s.audio);
    if !a.on {
        return;
    }
    let sacn_universe = a.universe + 1;
    let (to, port) = match (a.protocol, a.dest) {
        (_, Some(d)) => (d, if a.protocol == settings::Protocol::Sacn { sacn::PORT } else { artnet::PORT }),
        (settings::Protocol::Sacn, None) => (sacn::multicast_group(sacn_universe).into(), sacn::PORT),
        (settings::Protocol::ArtNet, None) => match stack.config_v4() {
            Some(c) => (c.address.broadcast().unwrap_or(embassy_net::Ipv4Address::BROADCAST), artnet::PORT),
            None => return,
        },
    };
    let to = embassy_net::IpEndpoint::new(embassy_net::IpAddress::Ipv4(to), port);
    // Never hold up the receive task: a frame that cannot go out within
    // 50 ms is dropped (the next one follows 23 ms later).
    let send = async {
        match a.protocol {
        settings::Protocol::ArtNet => {
            socket
                .send_to_with(artnet::DMX_PACKET_LEN, to, |b| {
                    (artnet::write_dmx(b, a.universe, frame.sequence, &frame.channels), ())
                })
                .await
            }
            settings::Protocol::Sacn => {
            // Component id: a fixed prefix and the MAC; source name: the
            // device name and "audio".
            let mut cid = *b"pixelcast\0\0\0\0\0\0\0";
            cid[10..16].copy_from_slice(&board::base_mac());
            let mut name = heapless::String::<64>::new();
            settings::with(|s| {
                if s.name.is_empty() {
                    let m = board::base_mac();
                    let _ = core::fmt::write(&mut name, format_args!("ArtnetNode-{:02X}{:02X} audio", m[4], m[5]));
                } else {
                    let _ = core::fmt::write(&mut name, format_args!("{} audio", s.name));
                }
            });
            socket
                .send_to_with(sacn::DATA_PACKET_LEN, to, |b| {
                    (sacn::write_data(b, &cid, &name, a.priority, frame.sequence, sacn_universe, &frame.channels), ())
                })
                .await
            }
        }
    };
    match with_timeout(Duration::from_millis(50), send).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("audio: send to {} failed: {:?}", to, e),
        Err(_) => warn!("audio: send to {} timed out; frame dropped", to),
    }
}

/// Merge state and statistics, used by both paths.
pub(crate) struct RxShared {
    merger: artnet_merge::Merger,
    stats: artnet::Stats,
}

/// Used only by tasks of the network core's executor (the receive task and
/// the W5500 runner, which calls `eth_fast_path`): they never preempt each
/// other, so no critical section is needed (interrupts stay on).
pub(crate) struct CoreLocal(core::cell::RefCell<RxShared>);

// SAFETY: see above; never touched from interrupts or the other core.
unsafe impl Sync for CoreLocal {}

impl CoreLocal {
    fn lock<R>(&self, f: impl FnOnce(&core::cell::RefCell<RxShared>) -> R) -> R {
        f(&self.0)
    }
}

pub(crate) static RX_SHARED: CoreLocal = CoreLocal(core::cell::RefCell::new(RxShared {
    merger: artnet_merge::Merger::new(),
    stats: artnet::Stats::new(),
}));

/// The merge mode from the settings, for the receive path (which must not
/// take the settings lock per packet). See [`sync_merge_mode`].
pub(crate) static MERGE_LTP: AtomicBool = AtomicBool::new(false);

/// Copies the merge mode from the settings into [`MERGE_LTP`].
pub(crate) fn sync_merge_mode() {
    MERGE_LTP.store(settings::with(|s| s.merge == artnet_merge::Mode::Ltp), Ordering::Relaxed);
}

/// HTP keeps a copy per source: not when the heap runs low.
pub(crate) static HEAP_LOW: AtomicBool = AtomicBool::new(false);

/// Art-Net on Ethernet goes through `eth_fast_path`.
pub(crate) static FAST_PATH: AtomicBool = AtomicBool::new(false);

/// This board's Ethernet MAC (set once at start-up).
pub(crate) static ETH_MAC: [core::sync::atomic::AtomicU8; 6] = [const { core::sync::atomic::AtomicU8::new(0) }; 6];

/// One received packet (Art-Net view; sACN already translated): DMX is
/// merged (Art-Net, by source `src`) and shown, Sync shows the frame, all
/// are counted.
#[esp_hal::ram]
pub(crate) fn handle_packet(packet: Option<&artnet::Packet<'_>>, src: Option<[u8; 4]>) {
    let start = Instant::now();
    // The audio control channels: they feed nothing else.
    if let Some(artnet::Packet::Dmx { universe, data, .. }) = packet
        && esp32_pixelcast::audio::on_dmx(*universe, data)
    {
        return;
    }
    // Only LED data counts as activity (signal loss, status LED, DHCP retry,
    // ArtPollReply): not polls, ArtAddress or ignored sACN sources.
    if matches!(packet, Some(artnet::Packet::Dmx { .. } | artnet::Packet::Sync)) {
        LAST_ARTNET_S.store(start.as_secs().max(1) as u32, Ordering::Relaxed);
    }
    // A running test pattern owns the LED output.
    let feed = !testpattern::active();
    RX_SHARED.lock(|r| {
        let r = &mut *r.borrow_mut();
        match packet {
            Some(artnet::Packet::Dmx { universe, data, .. }) => {
                let shown = match src {
                    Some(ip) => {
                        let mode = if MERGE_LTP.load(Ordering::Relaxed) || HEAP_LOW.load(Ordering::Relaxed) {
                            artnet_merge::Mode::Ltp
                        } else {
                            artnet_merge::Mode::Htp
                        };
                        r.merger.feed(*universe, ip, start.as_millis(), data, mode)
                    }
                    None => Some(*data),
                };
                if let Some(d) = shown {
                    status::offer_dmx(*universe, d);
                    if feed {
                        let t0 = Instant::now();
                        ledout::on_dmx(*universe, d);
                        status::RX_COPY_US.fetch_add(t0.elapsed().as_micros() as u32, Ordering::Relaxed);
                    }
                }
            }
            Some(artnet::Packet::Sync) if feed => ledout::on_sync(),
            _ => {}
        }
        r.stats.record(packet);
    });
    status::RX_PROC_US.fetch_add(start.elapsed().as_micros() as u32, Ordering::Relaxed);
    status::RX_PROC_N.fetch_add(1, Ordering::Relaxed);
}

/// Called by the W5500 driver for every received Ethernet frame, straight
/// from its read buffer: Art-Net DMX and Sync for this board are handled
/// here (no copies, no network stack, no task switch: in a burst the W5500
/// must be emptied fast). Returns false for anything else, which goes to
/// the network stack as usual.
#[esp_hal::ram]
pub(crate) fn eth_fast_path(frame: &[u8]) -> bool {
    if !FAST_PATH.load(Ordering::Relaxed) {
        return false;
    }
    let mac: [u8; 6] = core::array::from_fn(|i| ETH_MAC[i].load(Ordering::Relaxed));
    let Some((src, payload)) = artnet::udp_payload(frame, &mac) else {
        return false;
    };
    let packet = artnet::parse(payload);
    if !matches!(packet, Some(artnet::Packet::Dmx { .. } | artnet::Packet::Sync)) {
        return false; // ArtPoll, ArtAddress, ...: the receive task answers those
    }
    handle_packet(packet.as_ref(), Some(src));
    true
}

/// The sACN universes the outputs use (port-address + 1), as multicast
/// groups, in output order without duplicates.
pub(crate) fn sacn_groups() -> Groups {
    let s = settings::current();
    let mut g = Groups::new();
    for o in s.outputs.iter() {
        for k in 0..o.universes(s.led_type) {
            let pa = o.universe.wrapping_add(k as u16) & 0x7fff;
            let addr = sacn::multicast_group(pa + 1);
            if !g.contains(&addr) {
                // Past the limit the rest is not joined (unicast still works).
                let _ = g.push(addr);
            }
        }
    }
    // The audio control channels arrive in the input protocol too.
    if s.audio.on && s.audio.control {
        let addr = sacn::multicast_group(s.audio.control_universe + 1);
        if !g.contains(&addr) {
            let _ = g.push(addr);
        }
    }
    g
}

/// Up to 47 multicast groups (the stack allows 48, one is mDNS).
pub(crate) type Groups = heapless::Vec<[u8; 4], 47>;

