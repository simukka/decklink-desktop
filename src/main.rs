//! Play out the X11 desktop, or one RandR output of it, on a DeckLink card.
//!
//! Usage: decklink-desktop [--output NAME] [--mode 1080p60] [--scale nearest|bilinear|bicubic|lanczos|lanczos-gpu] [--device 0]
//!
//! Without `--output`, the whole desktop is scaled into `--mode` (default
//! 1080p60). With `--output`, the named RandR monitor (a real or virtual
//! output such as vkms's `Virtual-1`, or one made with `xrandr --setmonitor`)
//! is followed: playout starts when it is enabled, reconfigures when its
//! mode or position changes, and stops when it is disabled. The DeckLink
//! mode is picked to match the output's resolution and refresh rate unless
//! `--mode` forces one.
//!
//! Without `--scale`, a multithreaded bilinear scaler (see `scale.rs`) is used.

mod scale;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use decklink::{CommonDisplayMode, DeckLinkDisplay, DeckLinkPixelFormat, DisplayConfig, ScalingAlgorithm};
use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, Window};
use x11rb::rust_connection::RustConnection;

type AnyResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// How often the RandR configuration is re-checked for changes.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

struct Args {
    output: Option<String>,
    /// `None` = match the output (or 1080p60 when mirroring the desktop).
    mode: Option<CommonDisplayMode>,
    /// `None` = our own multithreaded bilinear; `Some` = one of the decklink crate's scalers.
    scale: Option<ScalingAlgorithm>,
    device: usize,
}

fn parse_args() -> AnyResult<Args> {
    let mut args = Args { output: None, mode: None, scale: None, device: 0 };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--output" => args.output = Some(value()?),
            "--mode" => {
                let name = value()?;
                args.mode = Some(CommonDisplayMode::from_name(&name).ok_or_else(|| format!("unknown mode {name}"))?);
            }
            "--scale" => args.scale = Some(ScalingAlgorithm::from_str(&value()?)),
            "--device" => args.device = value()?.parse()?,
            "-h" | "--help" => {
                println!("decklink-desktop [--output NAME] [--mode 1080p60|1080p5994|1080p50|2160p30|...] [--scale nearest|bilinear|bicubic|lanczos|lanczos-gpu] [--device N]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    Ok(args)
}

/// A rectangle of the root window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Region {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

/// What to play out: which part of the screen, in which DeckLink mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Target {
    region: Region,
    mode: CommonDisplayMode,
    /// False when no DeckLink mode matches the region and it is scaled into 1080p60.
    matched: bool,
}

/// Pick the progressive DeckLink mode with exactly this resolution whose
/// frame rate is closest to `refresh_hz`.
fn matching_mode(width: u16, height: u16, refresh_hz: Option<f64>) -> Option<CommonDisplayMode> {
    let hz = refresh_hz.unwrap_or(60.0);
    let fps_error = |m: &CommonDisplayMode| {
        let (n, d) = m.frame_rate();
        (n as f64 / d as f64 - hz).abs()
    };
    CommonDisplayMode::ALL
        .into_iter()
        .filter(|m| m.dimensions() == (width as u32, height as u32) && !m.name().contains('i'))
        .min_by(|a, b| fps_error(a).total_cmp(&fps_error(b)))
}

/// Watches the X server's RandR configuration.
struct Screen {
    conn: RustConnection,
    root: Window,
}

impl Screen {
    fn connect() -> AnyResult<Self> {
        let (conn, screen_num) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen_num].root;
        conn.randr_query_version(1, 5)?.reply()?;
        Ok(Self { conn, root })
    }

    /// The current target, or `None` if the followed output is disabled/absent.
    fn target(&self, args: &Args) -> AnyResult<Option<Target>> {
        let Some(name) = &args.output else {
            let geom = self.conn.get_geometry(self.root)?.reply()?;
            let region = Region { x: 0, y: 0, width: geom.width, height: geom.height };
            return Ok(Some(Target { region, mode: args.mode.unwrap_or(CommonDisplayMode::Hd1080p60), matched: true }));
        };

        let monitors = self.conn.randr_get_monitors(self.root, true)?.reply()?.monitors;
        for monitor in monitors {
            if self.conn.get_atom_name(monitor.name)?.reply()?.name != name.as_bytes() {
                continue;
            }
            let region = Region { x: monitor.x, y: monitor.y, width: monitor.width, height: monitor.height };
            let refresh = monitor.outputs.first().and_then(|&o| self.refresh_hz(o).ok().flatten());
            let matched = args.mode.or_else(|| matching_mode(region.width, region.height, refresh));
            let mode = matched.unwrap_or(CommonDisplayMode::Hd1080p60);
            return Ok(Some(Target { region, mode, matched: matched.is_some() }));
        }
        Ok(None)
    }

    /// Refresh rate of the mode currently driving `output`, if it is lit.
    fn refresh_hz(&self, output: randr::Output) -> AnyResult<Option<f64>> {
        let info = self.conn.randr_get_output_info(output, x11rb::CURRENT_TIME)?.reply()?;
        if info.crtc == x11rb::NONE {
            return Ok(None);
        }
        let crtc = self.conn.randr_get_crtc_info(info.crtc, x11rb::CURRENT_TIME)?.reply()?;
        let resources = self.conn.randr_get_screen_resources_current(self.root)?.reply()?;
        Ok(resources
            .modes
            .iter()
            .find(|m| m.id == crtc.mode && m.htotal > 0 && m.vtotal > 0)
            .map(|m| m.dot_clock as f64 / (m.htotal as f64 * m.vtotal as f64)))
    }
}

/// A SysV shared-memory segment the X server writes screen captures into,
/// together with its X-side `Shmseg` id once attached.
struct ShmSegment {
    ptr: *mut u8,
    len: usize,
    xid: u32,
}

// The mapping is plain memory; ownership is handed between the capture and
// output threads, never shared.
unsafe impl Send for ShmSegment {}

impl ShmSegment {
    fn new(len: usize) -> AnyResult<(Self, i32)> {
        unsafe {
            let id = libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600);
            if id < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let ptr = libc::shmat(id, std::ptr::null(), 0);
            if ptr as isize == -1 {
                libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
                return Err(std::io::Error::last_os_error().into());
            }
            Ok((Self { ptr: ptr as *mut u8, len, xid: 0 }, id))
        }
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for ShmSegment {
    fn drop(&mut self) {
        unsafe { libc::shmdt(self.ptr as *const libc::c_void) };
    }
}

/// Grabs a region of the root window via MIT-SHM and overlays the cursor via XFixes.
struct Capturer {
    conn: RustConnection,
    root: Window,
    region: Region,
}

impl Capturer {
    fn new(region: Region) -> AnyResult<Self> {
        let (conn, screen_num) = x11rb::connect(None)?;
        let screen = &conn.setup().roots[screen_num];
        let (root, depth) = (screen.root, screen.root_depth);
        let bpp = conn.setup().pixmap_formats.iter().find(|f| f.depth == depth).map(|f| f.bits_per_pixel);
        if depth < 24 || bpp != Some(32) {
            return Err(format!("unsupported root visual: depth {depth}, bpp {bpp:?} (need 24/32-bit)").into());
        }

        conn.shm_query_version()?.reply().map_err(|_| "X server lacks MIT-SHM")?;
        conn.xfixes_query_version(4, 0)?.reply()?;

        Ok(Self { conn, root, region })
    }

    /// Allocate a region-sized segment and attach it to the X server.
    fn new_segment(&self) -> AnyResult<ShmSegment> {
        let (mut shm, shm_id) = ShmSegment::new(self.region.width as usize * self.region.height as usize * 4)?;
        shm.xid = self.conn.generate_id()?;
        let attached: AnyResult<()> = (|| Ok(self.conn.shm_attach(shm.xid, shm_id as u32, false)?.check()?))();
        // Mark for removal now; the segment lives until both we and the X server detach.
        unsafe { libc::shmctl(shm_id, libc::IPC_RMID, std::ptr::null_mut()) };
        attached?;
        Ok(shm)
    }

    /// Capture one BGRA frame (the X server's 32-bit ZPixmap is BGRX on little-endian).
    fn capture(&self, shm: &mut ShmSegment) -> AnyResult<()> {
        let r = self.region;
        self.conn
            .shm_get_image(self.root, r.x, r.y, r.width, r.height, !0, ImageFormat::Z_PIXMAP.into(), shm.xid, 0)?
            .reply()?;
        // The cursor is a hardware overlay, so it isn't in the root window image.
        if let Ok(cursor) = self.conn.xfixes_get_cursor_image()?.reply() {
            Self::blend_cursor(shm.as_mut_slice(), r, &cursor);
        }
        Ok(())
    }

    fn blend_cursor(buf: &mut [u8], r: Region, c: &x11rb::protocol::xfixes::GetCursorImageReply) {
        let (sw, sh) = (r.width as i32, r.height as i32);
        let x0 = c.x as i32 - c.xhot as i32 - r.x as i32;
        let y0 = c.y as i32 - c.yhot as i32 - r.y as i32;
        for cy in 0..c.height as i32 {
            let y = y0 + cy;
            if y < 0 || y >= sh {
                continue;
            }
            for cx in 0..c.width as i32 {
                let x = x0 + cx;
                if x < 0 || x >= sw {
                    continue;
                }
                // Premultiplied ARGB.
                let p = c.cursor_image[(cy * c.width as i32 + cx) as usize];
                let a = p >> 24;
                if a == 0 {
                    continue;
                }
                let inv = 255 - a;
                let o = ((y * sw + x) * 4) as usize;
                for (i, shift) in [(0, 0), (1, 8), (2, 16)] {
                    let src = (p >> shift) & 0xff;
                    buf[o + i] = (src + (buf[o + i] as u32 * inv + 127) / 255).min(255) as u8;
                }
            }
        }
    }
}

/// Play out `target` until Ctrl-C or until the RandR configuration no
/// longer yields the same target.
fn run_session(args: &Args, target: Target, screen: &Screen, running: &AtomicBool) -> AnyResult<()> {
    let label = args.output.as_deref().unwrap_or("desktop");
    if !target.matched {
        log::warn!(
            "{label} is {}x{}, which has no DeckLink mode; scaling into 1080p60 (give it a 1920x1080 or 3840x2160 mode, or pass --mode)",
            target.region.width, target.region.height
        );
    }
    let capturer = Capturer::new(target.region)?;
    let (src_w, src_h) = (target.region.width as usize, target.region.height as usize);

    // Reading back the framebuffer takes most of a 60 fps frame, so capture
    // runs on its own thread, double-buffered: it fills one segment while
    // this thread scales and sends the other.
    let (free_tx, free_rx) = mpsc::channel::<ShmSegment>();
    let (full_tx, full_rx) = mpsc::sync_channel::<(ShmSegment, Duration)>(1);
    for _ in 0..2 {
        free_tx.send(capturer.new_segment()?).unwrap();
    }
    let capture_thread = std::thread::spawn(move || -> Result<(), String> {
        for mut shm in free_rx {
            let started = Instant::now();
            capturer.capture(&mut shm).map_err(|e| e.to_string())?;
            if full_tx.send((shm, started.elapsed())).is_err() {
                break;
            }
        }
        Ok(())
    });

    let (out_w, out_h) = target.mode.dimensions();
    let (fr_num, fr_den) = target.mode.frame_rate();
    let mut display = DeckLinkDisplay::new(DisplayConfig {
        device_index: args.device,
        pixel_format: DeckLinkPixelFormat::Bgra8Bit,
        force_mode: Some(target.mode),
        buffer_count: 4,
        ..DisplayConfig::default()
    });
    display.open()?;
    display.configure(out_w, out_h, fr_num, fr_den)?;

    // Fit the region inside the output, preserving aspect ratio (letterbox/pillarbox).
    let scale = f64::min(out_w as f64 / src_w as f64, out_h as f64 / src_h as f64);
    let (dst_w, dst_h) = ((src_w as f64 * scale).round() as usize, (src_h as f64 * scale).round() as usize);
    let unscaled = (dst_w, dst_h) == (src_w, src_h);
    let (x_off, y_off) = ((out_w as usize - dst_w) / 2, (out_h as usize - dst_h) / 2);
    log::info!(
        "Playing {label} {src_w}x{src_h}+{}+{} -> {} ({dst_w}x{dst_h} in {out_w}x{out_h}), scaler {}",
        target.region.x,
        target.region.y,
        target.mode.name(),
        if unscaled { "none (1:1)" } else { args.scale.map_or("multithreaded bilinear", |a| a.name()) }
    );

    let period = Duration::from_secs_f64(fr_den as f64 / fr_num as f64);
    let mut next = Instant::now();
    let (mut sent, mut dropped, mut work, mut grab) = (0u64, 0u64, Duration::ZERO, Duration::ZERO);
    let mut report_at = Instant::now() + Duration::from_secs(5);
    let mut poll_at = Instant::now() + POLL_INTERVAL;

    while running.load(Ordering::SeqCst) {
        if Instant::now() >= poll_at {
            poll_at += POLL_INTERVAL;
            match screen.target(args)? {
                Some(t) if t == target => {}
                Some(t) => {
                    let r = t.region;
                    log::info!("{label} changed to {}x{}+{}+{} ({}); reconfiguring", r.width, r.height, r.x, r.y, t.mode.name());
                    break;
                }
                None => {
                    log::info!("{label} disabled; stopping playout");
                    break;
                }
            }
        }

        let Ok((shm, grab_time)) = full_rx.recv() else {
            // The capture thread only hangs up on error; join it below to report it.
            break;
        };
        let started = Instant::now();
        grab += grab_time;
        let pixels = shm.as_slice();
        let frame = display.get_frame().ok_or("frame pool empty")?;
        match args.scale {
            _ if unscaled => frame.copy_from_centered(pixels, src_w * 4, src_w, src_h)?,
            Some(algorithm) => frame.copy_with_scaling(pixels, src_w * 4, src_w, src_h, dst_w, dst_h, algorithm)?,
            None => {
                if (dst_w, dst_h) != (out_w as usize, out_h as usize) {
                    frame.fill_black()?;
                }
                let dst_stride = frame.row_bytes() as usize;
                let dst = frame.buffer_mut()?;
                scale::bilinear_bgra(pixels, src_w * 4, src_w, src_h, dst, dst_stride, x_off, y_off, dst_w, dst_h);
            }
        }
        if display.display_last_frame()? {
            sent += 1;
        } else {
            dropped += 1;
        }
        free_tx.send(shm).ok();
        work += started.elapsed();

        if Instant::now() >= report_at {
            let frames = (sent + dropped).max(1);
            log::info!(
                "{sent} frames sent, {dropped} dropped, {:.1} ms/frame scale+send, {:.1} ms capture in parallel (budget {:.1} ms)",
                work.as_secs_f64() * 1000.0 / frames as f64,
                grab.as_secs_f64() * 1000.0 / frames as f64,
                period.as_secs_f64() * 1000.0
            );
            (sent, dropped, work, grab) = (0, 0, Duration::ZERO, Duration::ZERO);
            report_at += Duration::from_secs(5);
        }

        next += period;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            // Fell behind (slow scaler); don't try to catch up with a burst.
            next = now;
        }
    }

    display.close()?;
    drop((free_tx, full_rx));
    capture_thread.join().map_err(|_| "capture thread panicked")??;
    Ok(())
}

fn main() -> AnyResult<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args()?;

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))?;

    #[cfg(feature = "gpu")]
    if args.scale == Some(ScalingAlgorithm::LanczosGpu) && !decklink::gpu::initialize() {
        log::warn!("No usable GPU adapter; lanczos-gpu will fall back to (slow) CPU Lanczos");
    }

    let screen = Screen::connect()?;
    let mut waiting_logged = false;
    while running.load(Ordering::SeqCst) {
        match screen.target(&args)? {
            Some(target) => {
                waiting_logged = false;
                run_session(&args, target, &screen, &running)?;
            }
            None => {
                if !waiting_logged {
                    let name = args.output.as_deref().unwrap_or_default();
                    log::info!("Waiting for output {name} to be enabled (arandr, or xrandr --output {name} --mode 1920x1080)");
                    waiting_logged = true;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    }

    log::info!("Stopping");
    Ok(())
}
