#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Runner, Stack, StackResources};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;
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

const HTTP_PORT: u16 = 80;

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

    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        embassy_net::Config::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        seed,
    );

    spawner.spawn(connection(controller).unwrap());
    spawner.spawn(net_task(runner).unwrap());

    wait_for_ip(stack).await;

    let mut rx_buffer = [0; 1536];
    let mut tx_buffer = [0; 1536];
    let mut request_buffer = [0; 1024];
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));

        if let Err(e) = socket.accept(HTTP_PORT).await {
            warn!("accept error: {e:?}");
            continue;
        }
        info!("connection from {:?}", socket.remote_endpoint());

        if let Err(e) = handle_request(&mut socket, &mut request_buffer).await {
            warn!("connection error: {e:?}");
        }

        socket.close();
        // Give the stack a moment to send the FIN before the socket is dropped.
        Timer::after(Duration::from_millis(50)).await;
        socket.abort();
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

async fn wait_for_ip(stack: Stack<'_>) {
    stack.wait_link_up().await;
    info!("Waiting for DHCP...");
    stack.wait_config_up().await;
    if let Some(config) = stack.config_v4() {
        info!("Got IP: {}", config.address.address());
        info!(
            "HTTP server listening on http://{}:{HTTP_PORT}/",
            config.address.address()
        );
    }
}

/// Reads one HTTP request from `socket` into `buffer` and writes a response.
async fn handle_request(
    socket: &mut TcpSocket<'_>,
    buffer: &mut [u8],
) -> Result<(), embassy_net::tcp::Error> {
    let mut len = 0;

    // Read until the end of the request headers (or the buffer is full).
    loop {
        let n = socket.read(&mut buffer[len..]).await?;
        if n == 0 {
            return Ok(());
        }
        len += n;
        if buffer[..len].windows(4).any(|w| w == b"\r\n\r\n") || len == buffer.len() {
            break;
        }
    }

    // Request line is "<METHOD> <PATH> HTTP/1.x".
    let mut parts = buffer[..len]
        .split(|&b| b == b'\r' || b == b'\n')
        .next()
        .unwrap_or_default()
        .split(|&b| b == b' ');
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    log_request(method, path);

    let response: &[u8] = match (method, path) {
        (b"GET", b"/") => {
            b"HTTP/1.1 200 OK\r\n\
              Content-Type: text/html; charset=utf-8\r\n\
              Connection: close\r\n\
              \r\n\
              <!doctype html>\
              <html><head><title>ESP32-S3</title></head>\
              <body><h1>Hello from the ESP32-S3!</h1></body></html>\r\n"
        }
        (b"GET", _) => {
            b"HTTP/1.1 404 Not Found\r\n\
              Content-Type: text/plain\r\n\
              Connection: close\r\n\
              \r\n\
              Not Found\r\n"
        }
        _ => {
            b"HTTP/1.1 405 Method Not Allowed\r\n\
              Allow: GET\r\n\
              Content-Type: text/plain\r\n\
              Connection: close\r\n\
              \r\n\
              Method Not Allowed\r\n"
        }
    };

    socket.write_all(response).await?;
    socket.flush().await
}

// Kept out of line so the formatting machinery doesn't bloat `handle_request`'s stack frame.
#[inline(never)]
fn log_request(method: &[u8], path: &[u8]) {
    info!(
        "{} {}",
        core::str::from_utf8(method).unwrap_or("?"),
        core::str::from_utf8(path).unwrap_or("?")
    );
}
