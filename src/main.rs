#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

extern crate alloc;

use alloc::format;
use esp32c3_hal as hal;
use esp_backtrace as _;

use esp_println::logger::init_logger;
use esp_println::println;
use esp_wifi::initialize;
use esp_wifi::wifi::{WifiController, WifiDevice, WifiEvent, WifiMode, WifiState};

use hal::clock::{ClockControl, CpuClock};
use hal::gpio::{AnyPin, Output, PushPull};
use hal::systimer::SystemTimer;
use hal::timer::Wdt;
use hal::{embassy, peripherals::Peripherals, prelude::*, timer::TimerGroup, Rtc};
use hal::{Rng, IO};

use embassy_executor::Executor;
use embassy_executor::_export::StaticCell;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Timer};

use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, IpListenEndpoint, Ipv4Address, Stack, StackResources};
use embedded_svc::wifi::{ClientConfiguration, Configuration, Wifi};

use log::*;

const SSID: &str = env!("WIFI_SSID");
const PASSWORD: &str = env!("WIFI_PASSWORD");

macro_rules! singleton {
    ($val:expr) => {{
        type T = impl Sized;
        static STATIC_CELL: StaticCell<T> = StaticCell::new();
        let (x,) = STATIC_CELL.init(($val,));
        x
    }};
}

#[global_allocator]
static ALLOCATOR: esp_alloc::EspHeap = esp_alloc::EspHeap::empty();

static EXECUTOR: StaticCell<Executor> = StaticCell::new();

static STATUS_LED_CHANNEL: Channel<CriticalSectionRawMutex, (), 4> = Channel::new();

fn init_heap() {
    const HEAP_SIZE: usize = 32 * 1024;

    extern "C" {
        static mut _heap_start: u32;
    }
    unsafe {
        let heap_start = &_heap_start as *const _ as usize;
        ALLOCATOR.init(heap_start as *mut u8, HEAP_SIZE);
    }
}

#[entry]
fn main() -> ! {
    init_logger(log::LevelFilter::Info);
    esp_wifi::init_heap();
    init_heap();

    let peripherals = Peripherals::take();
    let system = peripherals.SYSTEM.split();
    let clocks = ClockControl::configure(system.clock_control, CpuClock::Clock160MHz).freeze();
    let mut rtc = Rtc::new(peripherals.RTC_CNTL);
    let systimer = SystemTimer::new(peripherals.SYSTIMER);
    let timer_group0 = TimerGroup::new(peripherals.TIMG0, &clocks);
    let timer_group1 = TimerGroup::new(peripherals.TIMG1, &clocks);

    // Init watchdogs
    rtc.swd.disable();
    rtc.rwdt.disable();
    let mut wdt0 = timer_group0.wdt;
    let mut wdt1 = timer_group1.wdt;
    wdt0.start(2u64.secs());
    wdt1.start(2u64.secs());

    // Init GPIO
    let io = IO::new(peripherals.GPIO, peripherals.IO_MUX);
    let wifi_led = io.pins.gpio12.into_push_pull_output();
    let status_led = io.pins.gpio13.into_push_pull_output();

    let mut rng = Rng::new(peripherals.RNG);
    let seed = random_u64(&mut rng);

    initialize(systimer.alarm0, rng, &clocks).unwrap();

    let (wifi_interface, controller) = esp_wifi::wifi::new(WifiMode::Sta);

    let config = Config::Dhcp(Default::default());

    // Init network stack
    let stack = &*singleton!(Stack::new(
        wifi_interface,
        config,
        singleton!(StackResources::<8>::new()),
        seed
    ));

    // let mut x = 0u32;
    // unsafe {
    //     asm!( "addi {0}, {0}, 1", inout(reg) x);
    // }
    // info!("{}", x);

    embassy::init(&clocks, timer_group0.timer0);
    let executor = EXECUTOR.init(Executor::new());

    executor.run(|spawner| {
        spawner.spawn(watchdog_feeder(wdt0, wdt1)).unwrap();
        spawner
            .spawn(connection(controller, wifi_led.into()))
            .unwrap();
        spawner.spawn(net_task(stack)).unwrap();
        spawner.spawn(status_led_task(status_led.into())).unwrap();
        spawner.spawn(http_client(stack)).unwrap();
        spawner.spawn(http_client(stack)).unwrap();
        spawner.spawn(api_server(stack, 1)).unwrap();
        spawner.spawn(api_server(stack, 2)).unwrap();
        spawner.spawn(api_server(stack, 3)).unwrap();
        spawner.spawn(api_server(stack, 4)).unwrap();
    });
}

fn random_u64(rng: &mut Rng) -> u64 {
    let h: [u8; 4] = rng.random().to_le_bytes();
    let l: [u8; 4] = rng.random().to_le_bytes();
    let mut res = [0u8; 8];
    res[..4].copy_from_slice(&h);
    res[4..].copy_from_slice(&l);

    u64::from_le_bytes(res)
}

#[embassy_executor::task]
async fn status_led_task(mut led: AnyPin<Output<PushPull>>) -> ! {
    loop {
        STATUS_LED_CHANNEL.recv().await;
        led.set_high().unwrap();
        Timer::after(Duration::from_millis(200)).await;
        led.set_low().unwrap();
        Timer::after(Duration::from_millis(200)).await;
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController, mut led: AnyPin<Output<PushPull>>) -> ! {
    info!("start connection task");
    info!("Device capabilities: {:?}", controller.get_capabilities());
    loop {
        match esp_wifi::wifi::get_wifi_state() {
            WifiState::StaConnected => {
                // wait until we're no longer connected
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                led.set_low().unwrap();
                error!("Wifi disconnected");
                Timer::after(Duration::from_millis(5000)).await
            }
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: SSID.into(),
                password: PASSWORD.into(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            info!("Starting wifi");
            controller.start().await.unwrap();
            info!("Wifi started!");
        }
        info!("About to connect...");

        match controller.connect().await {
            Ok(_) => {
                led.set_high().unwrap();
                info!("Wifi connected!")
            }
            Err(e) => {
                error!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<WifiDevice>) -> ! {
    stack.run().await
}

#[embassy_executor::task]
async fn watchdog_feeder(
    mut wdt0: Wdt<hal::peripherals::TIMG0>,
    mut wdt1: Wdt<hal::peripherals::TIMG1>,
) -> ! {
    loop {
        wdt0.feed();
        wdt1.feed();
        Timer::after(Duration::from_millis(500)).await;
    }
}

#[embassy_executor::task(pool_size = 4)]
async fn http_client(stack: &'static Stack<WifiDevice>) {
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    info!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config() {
            info!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    loop {
        Timer::after(Duration::from_millis(1_000)).await;

        let mut socket = TcpSocket::new(&stack, &mut rx_buffer, &mut tx_buffer);

        socket.set_timeout(Some(embassy_net::SmolDuration::from_secs(10)));

        let remote_endpoint = (Ipv4Address::new(142, 250, 185, 115), 80);
        info!("connecting...");
        let r = socket.connect(remote_endpoint).await;
        if let Err(e) = r {
            error!("connect error: {:?}", e);
            continue;
        }
        info!("connected!");
        let mut buf = [0; 1024];
        loop {
            use embedded_io::asynch::Write;
            let r = socket
                .write_all(b"GET / HTTP/1.0\r\nHost: www.mobile-j.de\r\n\r\n")
                .await;
            if let Err(e) = r {
                error!("write error: {:?}", e);
                break;
            }
            let n = match socket.read(&mut buf).await {
                Ok(0) => {
                    info!("read EOF");
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    error!("read error: {:?}", e);
                    break;
                }
            };
            println!("{}", core::str::from_utf8(&buf[..n]).unwrap());
        }
        Timer::after(Duration::from_millis(10000)).await;
    }
}

#[embassy_executor::task(pool_size = 4)]
async fn api_server(stack: &'static Stack<WifiDevice>, n: u8) {
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    let mut socket = TcpSocket::new(&stack, &mut rx_buffer, &mut tx_buffer);
    socket.set_timeout(Some(embassy_net::SmolDuration::from_secs(10)));
    loop {
        info!("Wait for connection...");
        let r = socket
            .accept(IpListenEndpoint {
                addr: None,
                port: 80,
            })
            .await;
        info!("Connected {n}...");

        if let Err(e) = r {
            println!("connect error: {:?}", e);
            continue;
        }

        STATUS_LED_CHANNEL.send(()).await;

        use embedded_io::asynch::Write;

        let mut buffer = [0u8; 1024];
        let mut pos = 0;
        loop {
            match socket.read(&mut buffer).await {
                Ok(0) => {
                    println!("read EOF");
                    break;
                }
                Ok(len) => {
                    let to_print =
                        unsafe { core::str::from_utf8_unchecked(&buffer[..(pos + len)]) };

                    if to_print.contains("\r\n\r\n") {
                        info!("{}", to_print);
                        break;
                    }

                    pos += len;
                }
                Err(e) => {
                    println!("read error: {:?}", e);
                    break;
                }
            };
        }

        let r = socket
            .write_all(
                format!(
                    "HTTP/1.0 200 OK\r\n\r\n\
            <html>\
                <body>\
                    <h1>Hello from {n} server!</h1>\
                </body>\
            </html>\r\n\
            "
                )
                .as_bytes(),
            )
            .await;
        if let Err(e) = r {
            println!("write error: {:?}", e);
        }

        let r = socket.flush().await;
        if let Err(e) = r {
            println!("flush error: {:?}", e);
        }

        socket.close();
        Timer::after(Duration::from_millis(1000)).await; // magic
        socket.abort();
    }
}
