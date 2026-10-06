#![no_std]
#![no_main]
#![feature(allocator_api)]
#![feature(impl_trait_in_assoc_type)]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use embassy_net::{Runner, StackResources};
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config, ControllerConfig, Interface, WifiController,
};
use log::{info, warn};
use static_cell::StaticCell;

extern crate alloc;

// Wi-Fi credentials are baked in at compile time. Set them in the environment
// (or the `[env]` table of `.cargo/esp-config.toml`) before building.
const SSID: &str = env!("WIFI_SSID");
const PASSWORD: &str = env!("WIFI_PASSWORD");

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // generator version: 1.4.0
    // generator parameters: -o esp32s3 -o unstable-hal -o esp32s3-mini-1-psram -o claude
    // (extended by hand with: -o alloc -o wifi -o embassy -o log -o esp-backtrace)
    esp_println::logger::init_logger(log::LevelFilter::Info);

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // The following pins are used to bootstrap the chip. They are available
    // for use, but check the datasheet of the module for more information on them.
    // - GPIO0
    // - GPIO3
    // - GPIO45
    // - GPIO46
    // These GPIO pins are in use by some feature of the module and should not be used.
    let _gpio26 = peripherals.GPIO26;
    let _gpio27 = peripherals.GPIO27;
    let _gpio28 = peripherals.GPIO28;
    let _gpio29 = peripherals.GPIO29;
    let _gpio30 = peripherals.GPIO30;
    let _gpio31 = peripherals.GPIO31;
    let _gpio32 = peripherals.GPIO32;

    // The Wi-Fi driver needs a heap.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
    esp_alloc::heap_allocator!(size: 32 * 1024);

    // The module's 8 MB of PSRAM extends the heap. Regions are used in the
    // order they're added, so this is only used once internal RAM is full.
    // Interface mode (quad/octal) and size are detected at runtime.
    // Note: atomics don't work in PSRAM on the ESP32-S3, so never heap-allocate
    // anything containing an `Atomic*`.
    let psram = esp_hal::psram::Psram::new(peripherals.PSRAM, Default::default());
    esp_alloc::psram_allocator!(&psram);
    info!("PSRAM heap: {} KB", psram.raw_parts().1 / 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    info!("Embassy initialized!");

    gps::spawn(
        spawner,
        peripherals.UART1,
        peripherals.GPIO5,
        peripherals.GPIO6,
        peripherals.GPIO7,
    );

    let station_config = Config::Station(
        StationConfig::default()
            .with_ssid(SSID.try_into().expect("SSID is too long"))
            .with_authentication(AuthenticationMethodConfig::Wpa2Personal(
                PASSWORD.try_into().expect("password is too long"),
            )),
    );
    let controller = WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(station_config),
    )
    .expect("Failed to initialize Wi-Fi controller");
    let wifi_interface = Interface::station();

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // One socket per web task, plus one for DHCP.
    static RESOURCES: StaticCell<StackResources<{ web::TASK_POOL_SIZE + 1 }>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        embassy_net::Config::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        seed,
    );

    spawner.spawn(connection(controller).unwrap());
    spawner.spawn(net_task(runner).unwrap());

    web::spawn(spawner, stack);

    // Log the address each time DHCP (re)configures the interface.
    loop {
        stack.wait_config_up().await;
        if let Some(config) = stack.config_v4() {
            info!(
                "HTTP server listening on http://{}:{}/",
                config.address.address(),
                web::HTTP_PORT
            );
        }
        stack.wait_config_down().await;
        warn!("Lost IP configuration");
    }
}

/// Keeps the station connected, reconnecting whenever the link drops.
#[allow(
    clippy::large_stack_frames,
    reason = "the esp-radio connect/disconnect futures are large"
)]
#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    loop {
        info!("Connecting to '{SSID}'...");
        match controller.connect_async().await {
            Ok(_) => {
                info!("Wi-Fi connected");
                let info = controller.wait_for_disconnect_async().await;
                warn!("Wi-Fi disconnected: {info:?}");
            }
            Err(e) => warn!("Failed to connect to Wi-Fi: {e:?}"),
        }
        Timer::after(Duration::from_secs(5)).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}

/// HTTP server: a pool of picoserve tasks sharing one router.
mod web {
    #![allow(
        clippy::large_stack_frames,
        reason = "picoserve's server future is large and is moved into the task pool when \
        spawned; embassy's task macro doesn't forward attributes to its generated wrapper"
    )]

    use alloc::boxed::Box;
    use embassy_executor::Spawner;
    use embassy_net::Stack;
    use embassy_sync::pubsub::WaitResult;
    use esp_alloc::ExternalMemory;
    use log::warn;
    use picoserve::response::chunked::{ChunkWriter, ChunkedResponse, Chunks, ChunksWritten};
    use picoserve::routing::{PathRouter, get};
    use picoserve::{AppBuilder, AppRouter};

    use crate::gps;

    pub const HTTP_PORT: u16 = 80;

    /// Number of HTTP connections that can be served concurrently.
    pub const TASK_POOL_SIZE: usize = 4;

    struct AppProps;

    impl AppBuilder for AppProps {
        type PathRouter = impl PathRouter;

        // Unknown paths get a 404 and other methods on "/" get a 405 from picoserve.
        fn build_app(self) -> picoserve::Router<Self::PathRouter> {
            picoserve::Router::new().route("/", get(|| async { ChunkedResponse::new(NmeaStream) }))
        }
    }

    /// Streams NMEA sentences from the GPS to the client until it disconnects.
    struct NmeaStream;

    impl Chunks for NmeaStream {
        fn content_type(&self) -> &'static str {
            "text/plain; charset=us-ascii"
        }

        async fn write_chunks<W: picoserve::io::Write>(
            self,
            mut writer: ChunkWriter<W>,
        ) -> Result<ChunksWritten, W::Error> {
            // There's a subscriber slot for every web task, so this can't fail.
            let mut sentences = gps::SENTENCES
                .subscriber()
                .expect("no free NMEA subscriber slot");

            // Runs until a write fails because the client has gone away.
            loop {
                match sentences.next_message().await {
                    WaitResult::Message(sentence) => {
                        writer.write_chunk(&sentence).await?;
                        writer.flush().await?;
                    }
                    WaitResult::Lagged(n) => {
                        warn!("NMEA stream client fell behind; skipped {n} sentences")
                    }
                }
            }
        }
    }

    /// Per-connection buffers for one web task.
    struct Buffers {
        tcp_rx: [u8; 1536],
        tcp_tx: [u8; 1536],
        http: [u8; 2048],
    }

    /// Spawns [`TASK_POOL_SIZE`] server tasks listening on [`HTTP_PORT`].
    pub fn spawn(spawner: Spawner, stack: Stack<'static>) {
        let app = picoserve::make_static!(AppRouter<AppProps>, AppProps.build_app());
        static CONFIG: picoserve::Config =
            picoserve::Config::const_default().keep_connection_alive();

        // Allocated zeroed in PSRAM so they never pass through the stack and
        // don't use internal RAM. They're needed for the life of the program.
        let buffers = Box::<[Buffers; TASK_POOL_SIZE], _>::new_zeroed_in(ExternalMemory);
        // SAFETY: `Buffers` is plain byte arrays, so all zeroes is a valid value.
        let buffers = Box::leak(unsafe { buffers.assume_init() });

        for (task_id, buffers) in buffers.iter_mut().enumerate() {
            spawner.spawn(web_task(task_id, stack, app, &CONFIG, buffers).unwrap());
        }
    }

    /// Each task serves one connection at a time.
    #[embassy_executor::task(pool_size = TASK_POOL_SIZE)]
    async fn web_task(
        task_id: usize,
        stack: Stack<'static>,
        app: &'static AppRouter<AppProps>,
        config: &'static picoserve::Config,
        buffers: &'static mut Buffers,
    ) -> ! {
        picoserve::Server::new(app, config, &mut buffers.http)
            .listen_and_serve(
                task_id,
                stack,
                HTTP_PORT,
                &mut buffers.tcp_rx,
                &mut buffers.tcp_tx,
            )
            .await
            .into_never()
    }
}

/// ATGM336H-6N-74 GNSS module: logs the NMEA 0183 sentences it outputs.
mod gps {
    use embassy_executor::Spawner;
    use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
    use embassy_sync::pubsub::PubSubChannel;
    use esp_hal::Async;
    use esp_hal::gpio::{InputPin, Level, Output, OutputConfig, OutputPin};
    use esp_hal::uart::{self, Uart, UartRx, UartTx};
    use heapless::Vec;
    use log::{info, warn};

    /// The ATGM336H-6N-74 defaults to 115200 baud, 8N1.
    const BAUD_RATE: u32 = 115_200;

    /// NMEA sentences are at most 82 characters; leave room for proprietary ones.
    const MAX_SENTENCE_LEN: usize = 128;

    /// One NMEA sentence, including its trailing CR LF.
    pub type Sentence = Vec<u8, { MAX_SENTENCE_LEN + 2 }>;

    /// Sentences buffered for each subscriber before it starts missing them.
    const QUEUE_LEN: usize = 16;

    /// Every sentence received from the GPS, with one subscriber slot per web
    /// task. Publishing never waits: subscribers that fall behind lose the oldest.
    pub static SENTENCES: PubSubChannel<
        CriticalSectionRawMutex,
        Sentence,
        QUEUE_LEN,
        { crate::web::TASK_POOL_SIZE },
        0,
    > = PubSubChannel::new();

    /// Starts the GPS and spawns a task that logs each sentence it sends.
    ///
    /// Pins are named from the GPS's side: `gps_tx` is its TXD0 output (module
    /// pin 2, our RX), `gps_rx` its RXD0 input (pin 3, our TX), and `gps_reset`
    /// its active-low nRESET (pin 9).
    pub fn spawn(
        spawner: Spawner,
        uart: impl uart::Instance + 'static,
        gps_tx: impl InputPin + 'static,
        gps_rx: impl OutputPin + 'static,
        gps_reset: impl OutputPin + 'static,
    ) {
        // Hold the GPS out of reset.
        let reset = Output::new(gps_reset, Level::High, OutputConfig::default());

        let config = uart::Config::default().with_baudrate(BAUD_RATE);
        let (rx, tx) = Uart::new(uart, config)
            .expect("invalid GPS UART config")
            .with_rx(gps_tx)
            .with_tx(gps_rx)
            .into_async()
            .split();

        spawner.spawn(gps_task(rx, tx, reset).unwrap());
    }

    // `_tx` and `_reset` are held so their pins stay configured; TX is unused
    // until we need to send commands to the GPS.
    #[embassy_executor::task]
    async fn gps_task(
        mut rx: UartRx<'static, Async>,
        _tx: UartTx<'static, Async>,
        _reset: Output<'static>,
    ) {
        let mut chunk = [0u8; 64];
        let mut line = [0u8; MAX_SENTENCE_LEN];
        let mut len = 0;
        // Set when a line overflows `line` or is corrupted; skip to the next newline.
        let mut discarding = false;

        loop {
            let n = match rx.read_async(&mut chunk).await {
                Ok(n) => n,
                Err(e) => {
                    warn!("GPS UART error: {e:?}");
                    discarding = true;
                    continue;
                }
            };

            for &byte in &chunk[..n] {
                match byte {
                    b'\n' => {
                        if !discarding && len > 0 {
                            handle_sentence(&line[..len]);
                        }
                        len = 0;
                        discarding = false;
                    }
                    b'\r' => {}
                    _ if discarding => {}
                    _ if len == line.len() => {
                        warn!("GPS sentence longer than {MAX_SENTENCE_LEN} bytes, dropped");
                        discarding = true;
                    }
                    _ => {
                        line[len] = byte;
                        len += 1;
                    }
                }
            }
        }
    }

    /// Logs a sentence (without its line ending) and publishes it to [`SENTENCES`].
    fn handle_sentence(sentence: &[u8]) {
        let text = match core::str::from_utf8(sentence) {
            Ok(text) if text.is_ascii() => text,
            _ => {
                warn!("GPS sent non-ASCII data; check the baud rate");
                return;
            }
        };
        info!("GPS: {text}");

        let mut message = Sentence::new();
        // Can't fail: `sentence` is at most MAX_SENTENCE_LEN bytes.
        let _ = message.extend_from_slice(sentence);
        let _ = message.extend_from_slice(b"\r\n");
        SENTENCES.immediate_publisher().publish_immediate(message);
    }
}
