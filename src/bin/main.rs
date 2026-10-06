#![no_std]
#![no_main]
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

    use embassy_executor::Spawner;
    use embassy_net::Stack;
    use picoserve::response::File;
    use picoserve::routing::{PathRouter, get_service};
    use picoserve::{AppBuilder, AppRouter};
    use static_cell::ConstStaticCell;

    pub const HTTP_PORT: u16 = 80;

    /// Number of HTTP connections that can be served concurrently.
    pub const TASK_POOL_SIZE: usize = 4;

    const INDEX_HTML: &str = "<!doctype html>\
    <html><head><title>ESP32-S3</title></head>\
    <body><h1>Hello from the ESP32-S3!</h1></body></html>\n";

    struct AppProps;

    impl AppBuilder for AppProps {
        type PathRouter = impl PathRouter;

        // Unknown paths get a 404 and other methods on "/" get a 405 from picoserve.
        fn build_app(self) -> picoserve::Router<Self::PathRouter> {
            picoserve::Router::new().route("/", get_service(File::html(INDEX_HTML)))
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

        // Built at compile time so the buffers never pass through the stack.
        static BUFFERS: ConstStaticCell<[Buffers; TASK_POOL_SIZE]> = ConstStaticCell::new(
            [const {
                Buffers {
                    tcp_rx: [0; 1536],
                    tcp_tx: [0; 1536],
                    http: [0; 2048],
                }
            }; TASK_POOL_SIZE],
        );

        for (task_id, buffers) in BUFFERS.take().iter_mut().enumerate() {
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
