//! Web interface on port 80.
//!
//! - `/` — status page, public, refreshes itself.
//! - `/config` — settings and firmware update, behind HTTP Basic login
//!   (user `admin`). Settings apply live; nothing restarts unless asked.
//!
//! Actions (`POST /config`, `/update`, `/restart`, `/reset`) need the login
//! and an `X-Requested-With` header. The settings page's scripts send both;
//! a foreign web page cannot add that header without a CORS preflight, which
//! this server never grants, so it cannot act through a logged-in browser.
//!
//! Upload with curl:
//! `curl -u admin:PASSWORD -H 'X-Requested-With: curl' --data-binary @firmware.bin http://<ip>/update`
//!
//! On the setup Wi-Fi the server also acts as a captive portal: requests for
//! any other host (phones probe e.g. connectivitycheck.gstatic.com, which the
//! board's DNS answers with its own address) are redirected to the status
//! page, so the phone offers to "sign in" and opens it.

use alloc::{format, string::String, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_net::{Stack, tcp::TcpSocket};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use embassy_time::{Duration, Instant, Timer, with_deadline, with_timeout};
use esp_bootloader_esp_idf::partitions::FlashStorage;
use log::{info, warn};

use crate::{
    config, core1, dualboot,
    http::{self, Route},
    json,
    ota::{self, BootInfo, ImageError, SECTOR, Updater},
    settings::{self, EthMode, Settings},
    status,
};

/// Flash access plus one sector of buffer, shared by everything that writes
/// flash. Only one upload runs at a time.
pub struct FlashCtx {
    pub flash: FlashStorage<'static>,
    pub buf: [u8; SECTOR],
}

pub type SharedFlash = Mutex<CriticalSectionRawMutex, FlashCtx>;

/// What every web server instance needs.
pub struct WebCtx {
    pub flash: &'static SharedFlash,
    pub boot: BootInfo,
    /// The Wi-Fi MAC, for the default device name.
    pub mac: [u8; 6],
    /// Which partition table is in flash (checked at start-up).
    pub layout: ota::TableState,
    /// What the app slots held at start-up.
    pub slots: heapless::Vec<ota::SlotInfo, 2>,
    /// Why the last in-place update gave up, if it did (from the updater).
    pub in_place_note: Option<heapless::String<128>>,
}

/// Flash holds the bootloader bundled with this firmware (`bootloader/`).
pub static BOOTLOADER_OURS: AtomicBool = AtomicBool::new(false);
/// The installed bootloader is ours at the bundled feature level or newer
/// (another build of it; enough for everything this firmware uses).
pub static BOOTLOADER_COMPATIBLE: AtomicBool = AtomicBool::new(false);
/// An upload has started overwriting the other slot: `WebCtx::slots` no
/// longer describes it.
pub static OTHER_CHANGED: AtomicBool = AtomicBool::new(false);

/// Set once the running image has confirmed itself (see [`ota::confirm`]).
pub static CONFIRMED: AtomicBool = AtomicBool::new(false);
/// Set by `POST /confirm`: keep the new image.
pub static CONFIRM_REQUEST: AtomicBool = AtomicBool::new(false);

/// Set once the board is about to restart (after an update, or on request).
/// Blocks further uploads and the trial confirmation in that window.
pub static RESTART_PENDING: AtomicBool = AtomicBool::new(false);
/// Uptime (ms) of the last web request over the setup Wi-Fi: the start-up
/// window keeps the Wi-Fi on while it is in use.
static AP_WEB_MS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// How long the setup Wi-Fi has gone without a web request.
pub fn ap_web_idle_ms() -> u32 {
    (Instant::now().as_millis() as u32).wrapping_sub(AP_WEB_MS.load(Ordering::Relaxed))
}
/// A firmware update is being written (for the status LED).
pub static UPDATING: AtomicBool = AtomicBool::new(false);

/// The last upload, for `/api/status` (diagnosing stalled uploads).
mod upload_diag {
    use core::sync::atomic::{AtomicU8, AtomicU32, Ordering::Relaxed};
    pub static PHASE: AtomicU8 = AtomicU8::new(0);
    pub static RECEIVED: AtomicU32 = AtomicU32::new(0);
    pub static LEN: AtomicU32 = AtomicU32::new(0);
    /// Uptime (ms) of the last received data.
    pub static LAST_MS: AtomicU32 = AtomicU32::new(0);
    /// Longest wait for data (ms).
    pub static MAX_GAP_MS: AtomicU32 = AtomicU32::new(0);
    /// Longest wait for the LED core to pause, and longest sector write (ms).
    pub static MAX_PAUSE_MS: AtomicU32 = AtomicU32::new(0);
    pub static MAX_WRITE_MS: AtomicU32 = AtomicU32::new(0);
    /// TCP state and receive queue at the abort.
    pub static END_STATE: AtomicU8 = AtomicU8::new(0);
    pub static END_QUEUE: AtomicU32 = AtomicU32::new(0);
    pub const NAMES: [&str; 8] = ["none", "reading", "writing", "finishing", "ok", "timeout", "closed", "rejected"];
    pub fn phase(p: u8) {
        PHASE.store(p, Relaxed);
    }
    pub fn name() -> &'static str {
        NAMES.get(PHASE.load(Relaxed) as usize).copied().unwrap_or("?")
    }
}

/// The whole request head must arrive within this time (a client sending
/// one byte every few seconds would otherwise pass every per-read limit).
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// The first byte must arrive sooner. Browsers open spare connections in
/// advance and send nothing on them; with only [`SOCKETS`] sockets, two of
/// those held the server for 10 s and new connections went unanswered.
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(2);
/// Longest gap between two reads (or writes) during a request.
const READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Slowest accepted upload speed; with 30 s on top, this bounds an upload.
const MIN_UPLOAD_BYTES_PER_SEC: u64 = 4096;
const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_TIMEOUT: Duration = Duration::from_millis(500);

const HEAD_MAX: usize = 1024;
/// Largest settings form accepted.
const FORM_MAX: usize = 2048;

/// Connections served at the same time per network: one for the live page's
/// polling, one for navigation. A connection attempt that finds no free
/// socket is not refused but silently ignored (see `no_rst_task` in main),
/// so the browser retries it about a second later, like a queue.
pub const SOCKETS: usize = 2;

/// TCP buffer sizes per connection. The receive size bounds the upload
/// window; 2 KB still outpaces the flash writes.
pub const RX_LEN: usize = 2048;
pub const TX_LEN: usize = 1536;

/// Sent with every response: no caching, no sniffing, and no framing by
/// other sites (a framed settings page could be clicked through blindly).
const SECURITY_HEADERS: &str = "Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
    X-Frame-Options: DENY\r\nContent-Security-Policy: frame-ancestors 'none'\r\n";

/// What a request asks the server to do after the response.
#[derive(PartialEq, Eq)]
enum After {
    Nothing,
    Restart,
}

/// Serves one connection at a time on `stack`, forever.
pub async fn serve(
    stack: Stack<'static>,
    rx: &'static mut [u8],
    tx: &'static mut [u8],
    ctx: &'static WebCtx,
    portal_host: Option<&'static str>,
) -> ! {
    loop {
        let mut sock = TcpSocket::new(stack, &mut *rx, &mut *tx);
        // The timeout only fires while we have unacknowledged data to send;
        // keep-alive probes make it cover a peer that silently went away.
        sock.set_timeout(Some(Duration::from_secs(15)));
        sock.set_keep_alive(Some(Duration::from_secs(5)));
        if sock.accept(80).await.is_err() {
            continue;
        }
        let ap = portal_host.is_some();
        if ap {
            AP_WEB_MS.store(Instant::now().as_millis() as u32, Ordering::Relaxed);
        }
        let after = handle(&mut sock, ctx, portal_host).await;
        if ap {
            AP_WEB_MS.store(Instant::now().as_millis() as u32, Ordering::Relaxed);
        }
        let _ = with_timeout(FLUSH_TIMEOUT, sock.flush()).await;
        // Send FIN and wait until both sides have closed (usually a few ms),
        // so the socket is free for the next connection right away. Polled:
        // `flush()` is not woken when the FIN is acknowledged, so it sat out
        // its timeout and back-to-back requests found both sockets busy
        // (the client's SYN was dropped and resent after 1 s).
        sock.close();
        wait_closed(&sock, CLOSE_TIMEOUT).await;
        // Only needed if the client did not close: a reset now cannot cut
        // off data it has not read yet.
        sock.abort();
        let _ = with_timeout(Duration::from_millis(20), sock.flush()).await;
        drop(sock);
        if after == After::Restart {
            info!("web: restarting");
            crate::crashlog::planned_default("Restart from the web interface (restart, update, restore or WLED)");
            Timer::after(Duration::from_millis(500)).await;
            // Not in the middle of a flash write from another connection
            // (bootloader, partition table: a cut there needs USB).
            let _flash = ctx.flash.lock().await;
            esp_hal::system::software_reset();
        }
    }
}

/// Waits until the connection is closed on both sides (or `limit` passed).
async fn wait_closed(sock: &TcpSocket<'_>, limit: Duration) {
    use embassy_net::tcp::State;
    let end = Instant::now() + limit;
    while Instant::now() < end && !matches!(sock.state(), State::Closed | State::TimeWait) {
        Timer::after(Duration::from_millis(2)).await;
    }
}

async fn write_all(sock: &mut TcpSocket<'_>, mut data: &[u8]) -> Result<(), ()> {
    while !data.is_empty() {
        // A client that stops reading would otherwise block here forever.
        match with_timeout(READ_TIMEOUT, sock.write(data)).await {
            Ok(Ok(0) | Err(_)) | Err(_) => return Err(()),
            Ok(Ok(n)) => data = &data[n..],
        }
    }
    Ok(())
}

async fn respond(sock: &mut TcpSocket<'_>, status: &str, extra: &str, ctype: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n{SECURITY_HEADERS}{extra}Connection: close\r\n\r\n",
        body.len()
    );
    if write_all(sock, head.as_bytes()).await.is_ok() {
        let _ = write_all(sock, body.as_bytes()).await;
    }
}

async fn text(sock: &mut TcpSocket<'_>, status: &str, body: &str) {
    respond(sock, status, "", "text/plain; charset=utf-8", body).await;
}

fn auth_ok(header: Option<&str>) -> bool {
    let creds = settings::with(|s| format!("{}:{}", config::WEB_USER, s.password()));
    let mut b64 = [0u8; 128];
    http::basic_auth_ok(header, http::base64_encode(creds.as_bytes(), &mut b64))
}

async fn handle(sock: &mut TcpSocket<'_>, ctx: &'static WebCtx, portal_host: Option<&str>) -> After {
    let mut buf = [0u8; HEAD_MAX];
    let mut fill = 0;
    let start = Instant::now();
    let head_deadline = start + HEAD_TIMEOUT;
    let first_deadline = start + FIRST_BYTE_TIMEOUT;
    let head_end = loop {
        if let Some(n) = http::head_len(&buf[..fill]) {
            break n;
        }
        if fill == buf.len() {
            text(sock, "431 Request Header Fields Too Large", "Header too large\n").await;
            return After::Nothing;
        }
        let deadline = if fill == 0 { first_deadline } else { head_deadline };
        match with_deadline(deadline, sock.read(&mut buf[fill..])).await {
            Ok(Ok(0) | Err(_)) | Err(_) => return After::Nothing,
            Ok(Ok(n)) => fill += n,
        }
    };
    let Some(head) = http::parse_head(&buf[..head_end]) else {
        text(sock, "400 Bad Request", "Bad request\n").await;
        return After::Nothing;
    };
    let early = &buf[head_end..fill];

    // Captive portal: anything not addressed to the board goes to its page.
    if let Some(ph) = portal_host
        && head.host != Some(ph)
    {
        redirect(sock, ph).await;
        return After::Nothing;
    }

    let Some(route) = Route::find(head.method, head.path) else {
        match portal_host {
            Some(ph) => redirect(sock, ph).await,
            None => text(sock, "404 Not Found", "Not found\n").await,
        }
        return After::Nothing;
    };

    // Public pages and data.
    match route {
        Route::Index => {
            send_page(sock, INDEX_HTML).await;
            return After::Nothing;
        }
        Route::ArtnetPage => {
            send_page(sock, ARTNET_HTML).await;
            return After::Nothing;
        }
        Route::Status => {
            json(sock, &api_status(ctx)).await;
            return After::Nothing;
        }
        Route::Leds => {
            let p = crate::ledout::preview(128);
            let body = json::object(|o| {
                o.field("outputs", &p);
            });
            json(sock, &body).await;
            return After::Nothing;
        }
        Route::ArtnetData => {
            json(sock, &api_artnet(head.query)).await;
            return After::Nothing;
        }
        Route::Audio => {
            json(sock, &api_audio()).await;
            return After::Nothing;
        }
        _ => {}
    }

    if !auth_ok(head.authorization) {
        if head.authorization.is_some() {
            // A wrong password: slow down guessing (one socket, 1 s).
            Timer::after(Duration::from_secs(1)).await;
        }
        // Pages get the browser's login prompt; scripts get a plain error.
        let extra = if head.method == "GET" {
            "WWW-Authenticate: Basic realm=\"Art-Net node settings\"\r\n"
        } else {
            ""
        };
        respond(sock, "401 Unauthorized", extra, "text/plain; charset=utf-8", "Login required.\n").await;
        return After::Nothing;
    }
    if head.method == "POST" && !head.xrw {
        text(sock, "403 Forbidden", "Missing X-Requested-With header.\n").await;
        return After::Nothing;
    }
    if head.method == "POST" && head.expect_continue && write_all(sock, b"HTTP/1.1 100 Continue\r\n\r\n").await.is_err() {
        return After::Nothing;
    }

    match route {
        Route::ConfigPage => {
            send_page(sock, CONFIG_HTML).await;
            After::Nothing
        }
        Route::Config => {
            json(sock, &api_config(ctx)).await;
            After::Nothing
        }
        Route::DualbootReset => {
            let r = {
                let fc = &mut *ctx.flash.lock().await;
                let paused = core1::pause().await;
                dualboot::reset(&mut fc.flash, &paused)
            };
            match r {
                Ok(msg) => text(sock, "200 OK", msg).await,
                Err(msg) => text(sock, "409 Conflict", msg).await,
            }
            After::Nothing
        }
        Route::AudioBeat => {
            crate::audio::simulate_beat();
            text(sock, "200 OK", "Beat sent.\n").await;
            After::Nothing
        }
        Route::DualbootLog => {
            // What the other firmware logged the last time it ran (the
            // patched WLED: its Ethernet and network).
            let log = {
                let fc = &mut *ctx.flash.lock().await;
                dualboot::read_log(&mut fc.flash, &mut fc.buf)
            };
            match log {
                Some(t) => respond(sock, "200 OK", "", "text/plain; charset=utf-8", &t).await,
                None => text(sock, "404 Not Found", "No log from the other firmware (it has not run, or does not write one).\n").await,
            }
            After::Nothing
        }
        Route::Backup => {
            // All settings as the form that POST /config takes, without the
            // password (restoring keeps the current one).
            let form = settings::current().to_form();
            let body: String = http::form_pairs(&form)
                .filter(|(k, _)| k != "password")
                .map(|(k, v)| {
                    let mut p = String::new();
                    p.push_str(&k);
                    p.push('=');
                    http::url_encode_into(&v, &mut p);
                    p
                })
                .collect::<alloc::vec::Vec<_>>()
                .join("&");
            let name = crate::mdns::host_label(&settings::with(|s| s.name(ctx.mac)));
            let extra = format!(
                "Content-Disposition: attachment; filename=\"{}-settings.txt\"\r\n",
                core::str::from_utf8(name.as_bytes()).unwrap_or("node")
            );
            respond(sock, "200 OK", &extra, "text/plain; charset=utf-8", &body).await;
            After::Nothing
        }
        Route::SaveConfig => {
            let Some(body) = read_body(sock, early, head.content_length, FORM_MAX).await else {
                return After::Nothing;
            };
            save_settings(sock, ctx, &body).await;
            After::Nothing
        }
        Route::Update => {
            let Some(len) = head.content_length else {
                text(sock, "411 Length Required", "Content-Length required\n").await;
                return After::Nothing;
            };
            update(sock, ctx, len, early).await
        }
        Route::Test => {
            let Some(body) = read_body(sock, early, head.content_length, FORM_MAX).await else {
                return After::Nothing;
            };
            match parse_test(&body) {
                Ok(p) => {
                    crate::testpattern::set(p, Instant::now().as_secs());
                    text(sock, "200 OK", if p.is_some() { "Test pattern on.\n" } else { "Test pattern off.\n" }).await;
                }
                Err(msg) => text(sock, "400 Bad Request", msg).await,
            }
            After::Nothing
        }
        Route::MigrateLayout => {
            if let Err(msg) = restart_allowed(ctx) {
                text(sock, "409 Conflict", msg).await;
                return After::Nothing;
            }
            let form = settings::current().to_form();
            let r = {
                let fc = &mut *ctx.flash.lock().await;
                let paused = core1::pause().await;
                crate::ota::migrate_layout(&mut fc.flash, &paused, &mut fc.buf, &form)
            };
            match r {
                Ok(()) => {
                    RESTART_PENDING.store(true, Ordering::Relaxed);
                    text(sock, "200 OK", "Dual-boot layout written. Restarting...\n").await;
                    After::Restart
                }
                Err(msg) => {
                    text(sock, "409 Conflict", msg).await;
                    After::Nothing
                }
            }
        }
        Route::InstallBootloader => {
            // Like a restart: not while new firmware is unconfirmed.
            if let Err(msg) = restart_allowed(ctx) {
                text(sock, "409 Conflict", msg).await;
                return After::Nothing;
            }
            let r = {
                let fc = &mut *ctx.flash.lock().await;
                let paused = core1::pause().await;
                crate::ota::install_bootloader(&mut fc.flash, &paused, &mut fc.buf)
            };
            match r {
                Ok(()) => {
                    BOOTLOADER_OURS.store(true, Ordering::Relaxed);
                    text(sock, "200 OK", "Bootloader installed. It takes effect at the next start.\n").await;
                }
                Err(msg) => text(sock, "409 Conflict", msg).await,
            }
            After::Nothing
        }
        Route::UpdateInPlace => {
            if let Err(msg) = restart_allowed(ctx) {
                text(sock, "409 Conflict", msg).await;
                return After::Nothing;
            }
            if OTHER_CHANGED.load(Ordering::Relaxed) {
                text(sock, "409 Conflict", "An upload is running.\n").await;
                return After::Nothing;
            }
            let (addr, link) = status::with(|st| (st.eth_addr, st.eth_link));
            let (Some((ip, prefix)), true) = (addr, link) else {
                text(sock, "409 Conflict", "The updater works over Ethernet only, and Ethernet has no address now.\n").await;
                return After::Nothing;
            };
            let gateway = settings::with(|s| if s.eth_mode == EthMode::Static { s.gateway } else { None });
            let place = ota::InPlaceAddr {
                ip: ip.octets(),
                prefix,
                gateway: gateway.map_or([0; 4], |g| g.octets()),
                mac: crate::board::eth_mac(),
            };
            let rng = esp_hal::rng::Rng::new();
            let mut token = [0u8; 16];
            for c in token.chunks_mut(4) {
                c.copy_from_slice(&rng.random().to_le_bytes());
            }
            let r = {
                let fc = &mut *ctx.flash.lock().await;
                let paused = core1::pause().await;
                ota::start_in_place(&mut fc.flash, &paused, &mut fc.buf, &place, token)
            };
            match r {
                Ok(()) => {
                    RESTART_PENDING.store(true, Ordering::Relaxed);
                    let mut hex = String::new();
                    for b in token {
                        let _ = core::fmt::Write::write_fmt(&mut hex, format_args!("{b:02x}"));
                    }
                    let body = json::object(|o| {
                        o.field("token", &hex);
                    });
                    respond(sock, "200 OK", "", "application/json", &body).await;
                    After::Restart
                }
                Err(msg) => {
                    text(sock, "409 Conflict", msg).await;
                    After::Nothing
                }
            }
        }
        Route::BootOther => match select_other(ctx).await {
            Ok(k) => {
                let m = match k {
                    ota::Kind::Other => "Starting the other firmware...\n",
                    ota::Kind::Ours => "Starting the other version. Its settings page or the BOOT button leads back.\n",
                };
                text(sock, "200 OK", m).await;
                After::Restart
            }
            Err(msg) => {
                text(sock, "409 Conflict", msg).await;
                After::Nothing
            }
        },
        Route::Confirm => {
            if !ctx.boot.trial || CONFIRMED.load(Ordering::Relaxed) {
                text(sock, "200 OK", "This firmware is already confirmed.\n").await;
                return After::Nothing;
            }
            let up = Instant::now().as_millis();
            if up < config::CONFIRM_AFTER_MS {
                let msg = alloc::format!("Let it run a little longer: confirm in {} s.\n", (config::CONFIRM_AFTER_MS - up).div_ceil(1000));
                text(sock, "409 Conflict", &msg).await;
                return After::Nothing;
            }
            CONFIRM_REQUEST.store(true, Ordering::Relaxed);
            let start = Instant::now();
            while !CONFIRMED.load(Ordering::Relaxed) && start.elapsed() < Duration::from_secs(10) {
                Timer::after(Duration::from_millis(100)).await;
            }
            if CONFIRMED.load(Ordering::Relaxed) {
                text(sock, "200 OK", "Confirmed: this firmware stays.\n").await;
            } else {
                text(sock, "500 Internal Server Error", "Confirming failed; it is retried in the background.\n").await;
            }
            After::Nothing
        }
        Route::Rollback => {
            // Under the flash lock: a confirm running meanwhile has either
            // finished (CONFIRMED) or sees RESTART_PENDING and stops.
            let _flash = ctx.flash.lock().await;
            if !ctx.boot.trial || CONFIRMED.load(Ordering::Relaxed) {
                text(sock, "409 Conflict", "This firmware is confirmed; there is nothing to roll back to.\n").await;
                return After::Nothing;
            }
            // The trial record says this boot already started: the next start
            // switches back to the previous image.
            RESTART_PENDING.store(true, Ordering::Relaxed);
            crate::crashlog::planned("Rolled back on request");
            text(sock, "200 OK", "Rolling back to the previous firmware...\n").await;
            After::Restart
        }
        Route::DebugWifi => {
            crate::status::AP_REQUEST.store(true, Ordering::Relaxed);
            text(sock, "200 OK", "Starting the setup Wi-Fi (until the next restart).\n").await;
            After::Nothing
        }
        Route::DebugPanic => {
            // Tests the crash log and the restart after a panic.
            if let Err(msg) = restart_allowed(ctx) {
                text(sock, "409 Conflict", msg).await;
                return After::Nothing;
            }
            text(sock, "200 OK", "Panicking on purpose...\n").await;
            Timer::after(Duration::from_millis(300)).await;
            panic!("test panic from /debug/panic");
        }
        Route::Restart => {
            if let Err(msg) = restart_allowed(ctx) {
                text(sock, "409 Conflict", msg).await;
                return After::Nothing;
            }
            RESTART_PENDING.store(true, Ordering::Relaxed);
            text(sock, "200 OK", "Restarting...\n").await;
            After::Restart
        }
        Route::Reset => {
            let r = {
                let mut fc = ctx.flash.lock().await;
                let paused = core1::pause().await;
                settings::reset(&mut fc.flash, &paused)
            };
            match r {
                Ok(()) => {
                    settings::set_current(Settings::default());
                    text(sock, "200 OK", "Settings reset to defaults and applied.\n").await;
                }
                Err(e) => {
                    warn!("web: settings reset failed: {:?}", e);
                    text(sock, "500 Internal Server Error", "Resetting the settings failed.\n").await;
                }
            }
            After::Nothing
        }
        // Public routes were answered above.
        _ => After::Nothing,
    }
}

/// `pattern=off|rainbow|chase|solid` and, for solid, `color=rrggbb` plus
/// `white=0..255`.
fn parse_test(body: &[u8]) -> Result<Option<crate::testpattern::Pattern>, &'static str> {
    use crate::testpattern::Pattern;
    let form = core::str::from_utf8(body).map_err(|_| "Bad form data.\n")?;
    let mut pattern = None;
    let mut color = [255u8, 255, 255];
    let mut white = 0u8;
    for (k, v) in http::form_pairs(form) {
        match k.as_ref() {
            "pattern" => pattern = Some(v),
            "color" => {
                let hex = v.trim_start_matches('#');
                if !hex.is_ascii() || hex.len() != 6 {
                    return Err("The colour must be rrggbb.\n");
                }
                for (i, c) in color.iter_mut().enumerate() {
                    *c = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| "The colour must be rrggbb.\n")?;
                }
            }
            "white" => white = v.parse().map_err(|_| "White must be 0 to 255.\n")?,
            _ => {}
        }
    }
    match pattern.as_deref() {
        Some("off") => Ok(None),
        Some("rainbow") => Ok(Some(Pattern::Rainbow)),
        Some("chase") => Ok(Some(Pattern::Chase)),
        Some("solid") => Ok(Some(Pattern::Solid([color[0], color[1], color[2], white]))),
        _ => Err("The pattern must be off, rainbow, chase or solid.\n"),
    }
}

async fn redirect(sock: &mut TcpSocket<'_>, host: &str) {
    let location = format!("Location: http://{host}/\r\n");
    respond(sock, "302 Found", &location, "text/plain; charset=utf-8", "Redirecting\n").await;
}

/// A restart while a new image is unconfirmed would roll it back.
/// Selects the other firmware slot for the next start, after the same checks
/// for every trigger (settings page, BOOT button): not during a trial, and
/// only a complete, verified image. Another firmware gets the handoff
/// (address, LED setup: crate::dualboot). The started firmware stays (no
/// trial). The caller restarts.
pub async fn select_other(ctx: &WebCtx) -> Result<ota::Kind, &'static str> {
    restart_allowed(ctx)?;
    if OTHER_CHANGED.load(Ordering::Relaxed) {
        return Err("The other slot is being rewritten by an upload.\n");
    }
    let r = {
        let mut fc = ctx.flash.lock().await;
        let paused = core1::pause().await;
        crate::ota::boot_other(&mut fc.flash, &paused).inspect(|k| {
            if *k == ota::Kind::Other {
                let fw = dualboot::present().unwrap_or(dualboot::Firmware::Unknown);
                dualboot::write_handoff(&mut fc.flash, &paused, fw);
            }
        })
    };
    match r {
        Ok(k) => {
            RESTART_PENDING.store(true, Ordering::Relaxed);
            Ok(k)
        }
        Err(e) => {
            warn!("web: other slot not started: {:?}", e);
            Err("The other slot holds no complete, intact firmware.\n")
        }
    }
}

/// The app slots and what they hold (as read at start-up), for
/// `/api/status` and `/api/config`.
fn slots_json(a: &mut json::Arr<'_>, ctx: &WebCtx) {
    let changed = OTHER_CHANGED.load(Ordering::Relaxed);
    let next = ota::next_slot().unwrap_or("ota_0");
    for sl in &ctx.slots {
        let stale = changed && !sl.running;
        let kind = match sl.kind {
            _ if stale => "changed",
            Some(ota::Kind::Ours) => "this",
            Some(ota::Kind::Other) => "other",
            None => "empty",
        };
        let opt = |v: &str| (!stale && !v.is_empty()).then(|| alloc::string::String::from(v));
        a.obj(|o| {
            o.field("slot", sl.label)
                .field("running", sl.running)
                .field("next", sl.label == next)
                .field("kind", kind)
                .field("firmware", opt(&sl.firmware))
                .field("version", opt(&sl.version))
                .field("built", opt(&sl.built));
        });
    }
}

fn restart_allowed(ctx: &WebCtx) -> Result<(), &'static str> {
    if RESTART_PENDING.load(Ordering::Relaxed) {
        return Err("The board is already restarting.\n");
    }
    if ctx.boot.trial && !CONFIRMED.load(Ordering::Relaxed) {
        return Err("The running firmware is new and not confirmed yet: confirm it (or roll back) on the status page first.\n");
    }
    Ok(())
}

/// Reads a small body completely (the head may already hold its start).
async fn read_body(sock: &mut TcpSocket<'_>, early: &[u8], len: Option<usize>, max: usize) -> Option<Vec<u8>> {
    let Some(len) = len else {
        text(sock, "411 Length Required", "Content-Length required\n").await;
        return None;
    };
    if len > max {
        text(sock, "413 Content Too Large", "Too much data\n").await;
        return None;
    }
    let mut body = alloc::vec![0u8; len];
    let n = early.len().min(len);
    body[..n].copy_from_slice(&early[..n]);
    let mut fill = n;
    let deadline = Instant::now() + READ_TIMEOUT;
    while fill < len {
        match with_deadline(deadline, sock.read(&mut body[fill..])).await {
            Ok(Ok(0) | Err(_)) | Err(_) => return None,
            Ok(Ok(k)) => fill += k,
        }
    }
    Some(body)
}

async fn save_settings(sock: &mut TcpSocket<'_>, ctx: &WebCtx, body: &[u8]) {
    let Ok(form) = core::str::from_utf8(body) else {
        text(sock, "400 Bad Request", "The form is not valid text.\n").await;
        return;
    };
    let old = settings::current();
    let mut new = old.clone();
    if let Err(msg) = new.apply_form(form, true) {
        text(sock, "400 Bad Request", msg).await;
        return;
    }
    if new == old {
        text(sock, "200 OK", "Nothing changed.\n").await;
        return;
    }
    let saved = {
        let fc = &mut *ctx.flash.lock().await;
        let paused = core1::pause().await;
        settings::save(&mut fc.flash, &paused, &mut fc.buf, &new)
    };
    if let Err(e) = saved {
        warn!("web: saving settings failed: {:?}", e);
        text(sock, "500 Internal Server Error", "Saving the settings failed.\n").await;
        return;
    }
    let ap = settings::Changes::between(&old, &new).ap && status::get().ap_ssid.is_some();
    // The capture set I2S up once: another mic type or pins need a restart.
    let mic = crate::audio::running_mic().is_some_and(|m| m != new.audio.mic) && old.audio.mic != new.audio.mic;
    settings::set_current(new);
    let msg = if ap {
        "Saved and applied. The setup Wi-Fi changed: devices on it need to reconnect.\n"
    } else if mic {
        "Saved. The microphone type and pins apply after a restart.\n"
    } else {
        "Saved and applied.\n"
    };
    text(sock, "200 OK", msg).await;
}

async fn update(sock: &mut TcpSocket<'_>, ctx: &'static WebCtx, len: usize, early: &[u8]) -> After {
    // While the running image is on trial, the other slot holds the only
    // known-good firmware (the rollback target): never overwrite it then.
    if let Err(msg) = restart_allowed(ctx) {
        text(sock, "409 Conflict", msg).await;
        return After::Nothing;
    }
    let Ok(mut guard) = ctx.flash.try_lock() else {
        text(sock, "409 Conflict", "Another update is running\n").await;
        return After::Nothing;
    };
    let fc = &mut *guard;
    // Shown by the status LED until this function returns.
    struct Updating;
    impl Drop for Updating {
        fn drop(&mut self) {
            UPDATING.store(false, Ordering::Relaxed);
        }
    }
    UPDATING.store(true, Ordering::Relaxed);
    let _updating = Updating;

    // The second core is parked per flash operation, not for the whole
    // upload, so the LEDs only stall briefly while each sector is written.
    let begun = {
        let paused = core1::pause().await;
        Updater::begin(&mut fc.flash, &paused, len as u32)
    };
    let mut updater = match begun {
        Ok(u) => {
            // The other slot is overwritten from here on.
            dualboot::forget();
            OTHER_CHANGED.store(true, Ordering::Relaxed);
            u
        }
        Err(e) => {
            let (status, msg) = error_text(e);
            text(sock, status, msg).await;
            return After::Nothing;
        }
    };

    let mut remaining = len;
    let mut fill = early.len().min(remaining);
    fc.buf[..fill].copy_from_slice(&early[..fill]);
    remaining -= fill;
    let mut next_log = 0;
    let deadline = Instant::now() + Duration::from_secs(30 + len as u64 / MIN_UPLOAD_BYTES_PER_SEC);
    {
        use upload_diag::*;
        LEN.store(len as u32, Ordering::Relaxed);
        RECEIVED.store((len - remaining) as u32, Ordering::Relaxed);
        MAX_GAP_MS.store(0, Ordering::Relaxed);
        MAX_PAUSE_MS.store(0, Ordering::Relaxed);
        MAX_WRITE_MS.store(0, Ordering::Relaxed);
        LAST_MS.store(Instant::now().as_millis() as u32, Ordering::Relaxed);
    }

    loop {
        // Fill one sector, or what is left of the image.
        upload_diag::phase(1);
        while fill < SECTOR && remaining > 0 {
            let want = (SECTOR - fill).min(remaining);
            let until = deadline.min(Instant::now() + READ_TIMEOUT);
            let r = with_deadline(until, sock.read(&mut fc.buf[fill..fill + want])).await;
            {
                use upload_diag::*;
                let now = Instant::now().as_millis() as u32;
                let gap = now.wrapping_sub(LAST_MS.load(Ordering::Relaxed));
                MAX_GAP_MS.fetch_max(gap, Ordering::Relaxed);
                LAST_MS.store(now, Ordering::Relaxed);
            }
            match r {
                Ok(Ok(0) | Err(_)) | Err(_) => {
                    warn!("web: upload aborted after {} bytes", updater.written());
                    upload_diag::END_STATE.store(sock.state() as u8, Ordering::Relaxed);
                    upload_diag::END_QUEUE.store(sock.recv_queue() as u32, Ordering::Relaxed);
                    upload_diag::phase(if r.is_err() { 5 } else { 6 });
                    return After::Nothing;
                }
                Ok(Ok(n)) => {
                    fill += n;
                    remaining -= n;
                    upload_diag::RECEIVED.fetch_add(n as u32, Ordering::Relaxed);
                }
            }
        }
        if fill == 0 {
            break;
        }
        upload_diag::phase(2);
        let written = {
            let t = Instant::now();
            let paused = core1::pause().await;
            let t2 = Instant::now();
            let r = updater.write_sector(&mut fc.flash, &paused, &fc.buf[..fill]);
            upload_diag::MAX_PAUSE_MS.fetch_max((t2 - t).as_millis() as u32, Ordering::Relaxed);
            upload_diag::MAX_WRITE_MS.fetch_max(t2.elapsed().as_millis() as u32, Ordering::Relaxed);
            r
        };
        if let Err(e) = written {
            upload_diag::phase(7);
            warn!("web: update rejected: {:?}", e);
            let (status, msg) = error_text(e);
            text(sock, status, msg).await;
            return After::Nothing;
        }
        fill = 0;
        if updater.written() >= next_log {
            info!("web: {} / {} bytes written", updater.written(), len);
            next_log += 256 * 1024;
        }
    }

    upload_diag::phase(3);
    let kind = updater.kind();
    let finished = {
        let paused = core1::pause().await;
        let r = updater.finish(&mut fc.flash, &paused);
        if r.is_ok() && kind == ota::Kind::Other {
            let fw = dualboot::detect(&mut fc.flash).unwrap_or(dualboot::Firmware::Unknown);
            dualboot::write_handoff(&mut fc.flash, &paused, fw);
        }
        r
    };
    upload_diag::phase(if finished.is_ok() { 4 } else { 7 });
    match finished {
        Ok(()) => {
            RESTART_PENDING.store(true, Ordering::Relaxed);
            let msg = match kind {
                ota::Kind::Ours => "Update written and verified. Restarting...\n",
                ota::Kind::Other => {
                    "Other firmware written and verified. Restarting into it. To come back, upload this firmware's .bin in its update page.\n"
                }
            };
            text(sock, "200 OK", msg).await;
            After::Restart
        }
        Err(e) => {
            warn!("web: update failed: {:?}", e);
            let (status, msg) = error_text(e);
            text(sock, status, msg).await;
            After::Nothing
        }
    }
}

fn error_text(e: ota::Error) -> (&'static str, &'static str) {
    use ota::Error as E;
    match e {
        E::NoOtaLayout => (
            "503 Service Unavailable",
            "This board has no update partitions yet. Flash it once over USB.\n",
        ),
        E::TooLarge => ("413 Content Too Large", "The firmware does not fit the update slot.\n"),
        E::Flash => ("500 Internal Server Error", "Writing the flash failed.\n"),
        E::Verify => (
            "500 Internal Server Error",
            "The written firmware failed verification. The running firmware is unchanged.\n",
        ),
        E::Image(i) => (
            "400 Bad Request",
            match i {
                ImageError::TooShort => "The file is too short to be a firmware image.\n",
                ImageError::NotAnImage => {
                    "Not a firmware image. Upload the .bin from `espflash save-image`, not the ELF.\n"
                }
                ImageError::WrongChip => "This firmware is for a different chip.\n",
                ImageError::NoHash => "The image has no SHA-256 appended and cannot be verified.\n",
                ImageError::NoAppDescriptor => "The image has no app descriptor.\n",
            },
        ),
    }
}

// ── Pages ───────────────────────────────────────────────────────────────────
// Built from `web/` by build.rs: shared CSS/JS inlined (each page is a single
// request; the server handles one connection at a time), minified, gzipped.

pub(crate) const INDEX_HTML: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/index.html.gz"));
pub(crate) const ARTNET_HTML: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/artnet.html.gz"));
pub(crate) const CONFIG_HTML: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/config.html.gz"));

/// Flash-resident data (about 35 KB, more than the cache) for the fast-flash
/// read-back check.
pub fn flash_check_data() -> [&'static [u8]; 3] {
    [INDEX_HTML, ARTNET_HTML, CONFIG_HTML]
}

async fn send_page(sock: &mut TcpSocket<'_>, gz: &'static [u8]) {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Encoding: gzip\r\n\
         Content-Length: {}\r\n{SECURITY_HEADERS}Connection: close\r\n\r\n",
        gz.len()
    );
    if write_all(sock, head.as_bytes()).await.is_ok() {
        let _ = write_all(sock, gz).await;
    }
}

async fn json(sock: &mut TcpSocket<'_>, body: &str) {
    respond(sock, "200 OK", "", "application/json", body).await;
}

/// `GET /api/audio`: the microphone's latest values (`on: false` while audio
/// is off).
fn api_audio() -> String {
    use pixelcast_core::audio::{Activity, Status};
    json::object(|o| match crate::audio::snapshot() {
        None => {
            o.field("on", false);
        }
        Some(s) => {
            o.field("on", true)
                .field("status", match s.status {
                    Status::Fault => "fault",
                    Status::Gated => "gated",
                    Status::Active => "active",
                    Status::Clipping => "clipping",
                })
                .field("db", s.input_db)
                .field("level", s.level)
                .field("peak", s.peak)
                .field("beat", s.beat)
                .field("phase", s.phase)
                .field("beat_ms", s.beat_ms)
                .field("bands", s.bands)
                .field("onsets", s.onsets)
                .field("activity", match s.activity {
                    Activity::Silence => "silence",
                    Activity::Noise => "noise",
                    Activity::Music => "music",
                })
                .field("restarts", crate::audio::restarts());
        }
    })
}

fn api_status(ctx: &WebCtx) -> String {
    let s = settings::current();
    let st = status::get();
    let trial = ctx.boot.trial && !CONFIRMED.load(Ordering::Relaxed);
    let now = Instant::now();
    let ld = |a: &core::sync::atomic::AtomicU32| a.load(Ordering::Relaxed);
    let name = s.name(ctx.mac);
    let r = st.artnet;
    let flash = crate::flashinfo::json();
    let last_restart = crate::crashlog::json();
    json::object(|o| {
        o.field("name", &name)
            .obj("firmware", |f| {
                f.field("name", env!("CARGO_PKG_NAME"))
                    .field("version", env!("PXC_VERSION"))
                    .field("slot", ctx.boot.slot)
                    .field("next_slot", ota::next_slot())
                    .arr("slots", |a| slots_json(a, ctx))
                    .field("state", if trial { "trial" } else { "confirmed" })
                    .field(
                        "trial_left_s",
                        if trial { config::TRIAL_DEADLINE_MS.saturating_sub(now.as_millis()) / 1000 } else { 0 },
                    );
            })
            .field("uptime_s", now.as_secs())
            .field("heap_free", esp_alloc::HEAP.free())
            .field("heap_min", ld(&status::HEAP_MIN))
            .field("stack_free_min", crate::stackwatch::free_min())
            .field("default_password", s.password.is_empty() && config::DEFAULT_PASSWORD)
            .obj("ethernet", |e| {
                e.field("present", st.eth_present)
                    .field("link", st.eth_link)
                    .field("address", st.eth_addr.map(|(ip, p)| settings::cidr(ip, p)))
                    .field("source", st.eth_source);
            })
            .obj("wifi", |w| {
                w.field("on", st.ap_ssid.is_some())
                    .field("ssid", st.ap_ssid.as_deref().unwrap_or(""))
                    .field("reason", st.ap_reason)
                    .field(
                        "address",
                        if st.ap_ssid.is_some() { alloc::string::ToString::to_string(&config::AP_IP) } else { String::new() },
                    );
            })
            .obj("artnet", |a| {
                a.field("network", st.artnet_net)
                    .field("window_ms", 1000u32)
                    .field("pps", r.map_or(0, |r| r.pps))
                    .field("dmx", r.map_or(0, |r| r.dmx))
                    .field("universes", r.map_or(0, |r| r.universes))
                    .field("fps_min", r.map_or(0, |r| r.fps_min))
                    .field("fps_max", r.map_or(0, |r| r.fps_max))
                    .field("seq_gaps", r.map_or(0, |r| r.seq_gaps))
                    .field("sync", r.map_or(0, |r| r.syncs))
                    .field("poll", r.map_or(0, |r| r.polls))
                    .field("invalid", r.map_or(0, |r| r.invalid))
                    .field("merging", status::MERGING.load(Ordering::Relaxed));
            })
            .obj("led", |l| led_status(l, &st));
        match crate::testpattern::current() {
            Some((p, started)) => o.obj("test", |t| {
                t.field("pattern", p.key()).field(
                    "off_in_s",
                    crate::testpattern::AUTO_OFF_S.saturating_sub(now.as_secs().saturating_sub(started)),
                );
            }),
            None => o.field("test", json::Raw("null")),
        };
        o.obj("mdns", |m| {
            let host = crate::mdns::host_label(&name);
            m.field("host", format!("{}.local", core::str::from_utf8(host.as_bytes()).unwrap_or("")));
        })
        .obj("upload", |u| {
            use upload_diag::*;
            u.field("phase", name())
                .field("received", ld(&RECEIVED))
                .field("len", ld(&LEN))
                .field("idle_ms", (now.as_millis() as u32).wrapping_sub(ld(&LAST_MS)))
                .field("max_gap_ms", ld(&MAX_GAP_MS))
                .field("max_pause_ms", ld(&MAX_PAUSE_MS))
                .field("max_write_ms", ld(&MAX_WRITE_MS))
                .field("end_tcp_state", END_STATE.load(Ordering::Relaxed))
                .field("end_queue", ld(&END_QUEUE));
        })
        .obj("spi", |p| {
            use crate::board;
            use embassy_net_wiznet as wiz;
            p.field("mhz", ld(&board::SPI_MHZ))
                .field("half_duplex", board::SPI_HALF.load(Ordering::Relaxed))
                .field("txn", ld(&board::SPI_TXNS))
                .field("busy_us", ld(&board::SPI_BUSY_US))
                .field("checks", ld(&wiz::SPI_CHECKS))
                .field("check_errors", ld(&wiz::SPI_CHECK_ERRORS))
                .field("rx_resyncs", ld(&wiz::RX_RESYNCS))
                .field("backlog_max", wiz::RX_BACKLOG_MAX.swap(0, Ordering::Relaxed))
                .field("proc_us", ld(&status::RX_PROC_US))
                .field("copy_us", ld(&status::RX_COPY_US))
                .field("proc_n", ld(&status::RX_PROC_N))
                .field("fifo", board::fifo::ON.load(Ordering::Relaxed))
                .field("fifo_stuck", ld(&board::fifo::STUCK))
                .field("bulk_stuck", ld(&board::SPI_BULK_STUCK));
        })
        .field("flash", json::Raw(&flash))
        .field("last_restart", json::Raw(&last_restart));
    })
}

fn led_status(o: &mut json::Obj<'_>, st: &status::Status) {
    let (n, rate, a) = match st.led_shape {
        Some(sh) => (sh.n, sh.sample_rate_hz, sh.actual_ns()),
        None => (0, 0, (0, 0, 0)),
    };
    o.field("fps", st.led_fps)
        .field("underruns", st.led_underruns)
        .field("frame_us", st.led_frame_us)
        .field("samples_per_bit", n)
        .field("sample_rate", rate)
        .field("measured_rate", st.led_measured_hz)
        .field("t0h", a.0)
        .field("t1h", a.1)
        .field("period", a.2)
        .field("power_mw", st.led_power_mw)
        .field("volts_dv", settings::with(|s| s.led_decivolts))
        .field("power_limited", st.led_power_limited)
        .obj("memory", crate::ledout::pool_json);
}

fn api_artnet(query: &str) -> String {
    let u = http::form_pairs(query)
        .find(|(k, _)| k == "u")
        .and_then(|(_, v)| v.parse::<u16>().ok())
        .filter(|&u| u <= 0x7fff)
        .unwrap_or(0);
    let st = status::get();
    let r = st.artnet;
    // Copy out first: formatting allocates, which should not happen while
    // the lock (a critical section) is held.
    let mut data = [0u8; crate::artnet::DMX_CHANNELS];
    let mut len = 0;
    let mut at = None;
    status::watch(u, |w| {
        len = w.len;
        data[..len].copy_from_slice(&w.data[..len]);
        at = w.at;
    });
    json::object(|o| {
        o.field("network", st.artnet_net)
            .field("led_type", settings::with(|s| s.led_type.key()))
            .field("window_ms", 1000u32)
            .field("pps", r.map_or(0, |r| r.pps))
            .field("seq_gaps", r.map_or(0, |r| r.seq_gaps))
            .field("sync", r.map_or(0, |r| r.syncs))
            .arr("universes", |a| {
                for (i, &fps) in r.iter().flat_map(|r| r.per_universe.iter()).enumerate() {
                    if fps != 0 {
                        a.obj(|o| {
                            o.field("u", i).field("fps", fps);
                        });
                    }
                }
            })
            .obj("watch", |w| {
                w.field("u", u).field("age_ms", at.map(|at| at.elapsed().as_millis()));
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let out = w.key_raw("data");
                out.reserve(len * 2 + 2);
                out.push('"');
                for &b in &data[..len] {
                    out.push(HEX[(b >> 4) as usize] as char);
                    out.push(HEX[(b & 15) as usize] as char);
                }
                out.push('"');
            });
    })
}

fn api_config(ctx: &WebCtx) -> String {
    let s = settings::current();
    let order_name = |o: crate::led::Order| String::from(core::str::from_utf8(&o.name()).unwrap_or("grb"));
    json::object(|o| {
        o.field("name", &s.name)
            .field("default_name", Settings::default().name(ctx.mac))
            .field("eth", if s.eth_mode == EthMode::Static { "static" } else { "dhcp" })
            .field("ip", alloc::string::ToString::to_string(&s.ip))
            .field("prefix", s.prefix)
            .field("gateway", s.gateway.map(|g| alloc::string::ToString::to_string(&g)).unwrap_or_default())
            .field("fallback", s.artnet_fallback)
            .field("ap_channel", s.ap_channel)
            .field("ap_after", s.ap_after_s)
            .field("ap_boot", s.ap_boot_min)
            .field("ap_idle", s.ap_idle_min)
            .field("default_password", s.password.is_empty() && config::DEFAULT_PASSWORD)
            .field("ota_available", ctx.boot.ota_available)
            .field("restart_allowed", restart_allowed(ctx).is_ok())
            .field("other_firmware", dualboot::present().is_some())
            .field("other_reset", dualboot::present().and_then(|f| f.adapter()).is_some_and(|a| a.reset.is_some()))
            .field("other_back", dualboot::present().and_then(|f| f.adapter()).map_or("", |a| a.way_back))
            .arr("slots", |a| slots_json(a, ctx))
            .field("layout", ctx.layout.key())
            .field(
                "bootloader",
                if BOOTLOADER_OURS.load(Ordering::Relaxed) {
                    "bundled"
                } else if BOOTLOADER_COMPATIBLE.load(Ordering::Relaxed) {
                    "compatible"
                } else {
                    "other"
                },
            )
            .field("in_place_note", ctx.in_place_note.as_deref())
            .field("led_type", s.led_type.key())
            .field("order", order_name(s.order))
            .field("brightness", s.brightness)
            .arr("outputs", |a| {
                for out in &s.outputs {
                    a.obj(|o| {
                        o.field("leds", out.leds)
                            .field("univ", out.universe)
                            .field("type", out.ty.map_or("", crate::led::LedType::key))
                            .field("order", out.order.map(order_name).unwrap_or_default())
                            .field("rev", out.reverse)
                            .field("group", out.group)
                            .field("offset", out.offset)
                            .field("start", out.start);
                    });
                }
            })
            .field("t0h", s.timing.t0h_ns)
            .field("t1h", s.timing.t1h_ns)
            .field("period", s.timing.period_ns)
            .field("latch", s.latch_us)
            .field("protocol", s.protocol.key())
            .field("interp", s.interpolate)
            .field("dither", s.dither)
            .field("fast_flash", s.fast_flash)
            .obj("audio", |o| {
                let a = &s.audio;
                o.field("on", a.on)
                    .field("mic", a.mic.kind.key())
                    .field("sel", if a.mic.sel_high { "vdd" } else { "gnd" })
                    .field("din", a.mic.din)
                    .field("ws", a.mic.ws)
                    .field("bclk", a.mic.bclk)
                    .field("mic_pending", crate::audio::running_mic().is_some_and(|m| m != a.mic))
                    .field("proto", a.protocol.key())
                    .field("dest", a.dest.map(|d| alloc::string::ToString::to_string(&d)).unwrap_or_default())
                    .field("univ", a.universe)
                    .field("prio", a.priority)
                    .field("agc", a.params.agc)
                    .field("gain", a.params.gain_db)
                    .field("gate", a.params.gate_db)
                    .field("attack", a.params.attack_ms)
                    .field("release", a.params.release_ms)
                    .field("sens", a.params.sensitivity);
            })
            .field("merge", s.merge.key())
            .arr("presets", |a| {
                for p in settings::parse_presets(&s.timing_presets).iter().flat_map(|l| l.iter()) {
                    a.obj(|o| {
                        o.field("name", p.name)
                            .field("period", p.period)
                            .field("t0h", p.t0h)
                            .field("t1h", p.t1h)
                            .field("latch", p.latch);
                    });
                }
            })
            .field("loss", s.loss.key())
            .field("loss_after", s.loss_after_s)
            .field("loss_color", settings::rgbw_hex(s.loss_color))
            .field("power_led_mw", s.power_mw_per_led)
            .field("power_out_mw", s.power_out_mw)
            .field("power_total_mw", s.power_total_mw)
            .field("led_volts_dv", s.led_decivolts)
            .obj("limits", |l| {
                use crate::ledout;
                l.field("max_leds", crate::led::MAX_LEDS)
                    .field("led_bytes", ledout::copy_budget(ledout::POOL_BYTES))
                    .field("led_bytes_wifi", ledout::copy_budget(ledout::POOL_WIFI_BYTES))
                    .field("max_samples_per_bit", crate::led::MAX_SAMPLES_PER_BIT)
                    .field("max_sample_rate", crate::led::MAX_SAMPLE_RATE_HZ);
            });
    })
}
