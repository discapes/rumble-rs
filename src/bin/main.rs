#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use core::net::Ipv4Addr;

use embassy_executor::Spawner;
use embassy_net::{Runner, StackResources, tcp::TcpSocket};
use embassy_time::{Duration, Timer};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::pixelcolor::raw::RawU16;
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::clock::CpuClock;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::dma_buffers;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::rng::Rng;
use esp_hal::spi::master::Spi;
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_radio::{
    Controller,
    wifi::{ClientConfig, ModeConfig, WifiController, WifiDevice, WifiEvent, WifiStaState},
};
use mipidsi::interface::SpiInterface;
use rumble_rs::jpeg::JpegDecoder;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("PANIC: {}", info);
    loop {}
}

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

esp_bootloader_esp_idf::esp_app_desc!();

/// Find a two-byte marker (e.g. SOI=0xFFD8, EOI=0xFFD9) in a byte slice.
fn find_marker(data: &[u8], b0: u8, b1: u8) -> Option<usize> {
    data.windows(2).position(|w| w[0] == b0 && w[1] == b1)
}

const SSID: &str = "ylikellotus";
const PASSWORD: &str = "alakerta";

const DISPLAY_HEIGHT: u16 = 170;

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 64 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    // -----------------------------------------------------------------------
    // Display init (ST7789 320×170 over SPI with DMA)
    // -----------------------------------------------------------------------
    let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) = dma_buffers!(32000);
    let dma_rx_buf = DmaRxBuf::new(rx_descriptors, rx_buffer).unwrap();
    let dma_tx_buf = DmaTxBuf::new(tx_descriptors, tx_buffer).unwrap();

    let mut delay = esp_hal::delay::Delay::new();

    let dc = Output::new(peripherals.GPIO15, Level::Low, OutputConfig::default());
    let mut rst = Output::new(peripherals.GPIO7, Level::Low, OutputConfig::default());
    rst.set_high();

    let spi = Spi::new(
        peripherals.SPI2,
        esp_hal::spi::master::Config::default().with_frequency(Rate::from_mhz(80)),
    )
    .unwrap()
    .with_sck(peripherals.GPIO4)
    .with_mosi(peripherals.GPIO5)
    .with_miso(peripherals.GPIO16)
    .with_dma(peripherals.DMA_CH0)
    .with_buffers(dma_rx_buf, dma_tx_buf)
    .into_async();

    let cs = Output::new(peripherals.GPIO6, Level::High, OutputConfig::default());
    let spi_device = ExclusiveDevice::new(spi, cs, delay).unwrap();

    let spi_buffer = mk_static!([u8; 10240], [0u8; 10240]);
    let di = SpiInterface::new(spi_device, dc, spi_buffer);

    let mut display = mipidsi::Builder::new(mipidsi::models::ST7789, di)
        .reset_pin(rst)
        .display_size(170, 320)
        .invert_colors(mipidsi::options::ColorInversion::Inverted)
        .orientation(mipidsi::options::Orientation::new().rotate(mipidsi::options::Rotation::Deg90))
        .display_offset(35, 0)
        .init(&mut delay)
        .unwrap();

    println!("Display initialized");

    // -----------------------------------------------------------------------
    // WiFi init
    // -----------------------------------------------------------------------
    let esp_radio_ctrl = &*mk_static!(Controller<'static>, esp_radio::init().unwrap());

    let (controller, interfaces) =
        esp_radio::wifi::new(esp_radio_ctrl, peripherals.WIFI, Default::default()).unwrap();

    let wifi_interface = interfaces.sta;

    let config = embassy_net::Config::dhcpv4(Default::default());

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        wifi_interface,
        config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(runner)).ok();

    // Wait for link
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    // -----------------------------------------------------------------------
    // MJPEG streaming loop
    // -----------------------------------------------------------------------
    // Heap-allocated TCP buffers — larger RX = larger TCP window = better throughput
    let mut rx_buffer = vec![0u8; 16384];
    let mut tx_buffer = vec![0u8; 1024];

    // Frame accumulation buffer (~30KB on heap)
    let mut frame_buf: Vec<u8> = vec![0u8; 30 * 1024];
    let mut frame_len: usize = 0;
    let mut in_frame = false;

    let mut decoder = JpegDecoder::new().expect("failed to create JPEG decoder");
    println!("JPEG decoder created");

    loop {
        Timer::after(Duration::from_millis(1_000)).await;

        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));

        let remote_endpoint = (Ipv4Addr::new(172, 20, 10, 8), 3000);
        println!("connecting to 172.20.10.8:3000...");
        let r = socket.connect(remote_endpoint).await;
        if let Err(e) = r {
            println!("connect error: {:?}", e);
            continue;
        }
        println!("connected!");

        let mut tcp_buf = [0u8; 4096];

        loop {
            let n = match embedded_io_async::Read::read(&mut socket, &mut tcp_buf).await {
                Ok(0) => {
                    println!("connection closed");
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    println!("read error: {:?}", e);
                    break;
                }
            };

            // Scan received chunk for JPEG SOI/EOI markers using bulk copy
            let mut pos = 0;
            while pos < n {
                if !in_frame {
                    // Scan for SOI marker (0xFF 0xD8) in remaining data
                    if let Some(soi) = find_marker(&tcp_buf[pos..n], 0xFF, 0xD8) {
                        frame_len = 0;
                        in_frame = true;
                        pos += soi; // advance to SOI start
                    } else {
                        break; // no SOI in this chunk
                    }
                }

                // Bulk copy remaining tcp data into frame_buf
                let space = frame_buf.len() - frame_len;
                let avail = n - pos;
                let copy_len = avail.min(space);
                if copy_len == 0 {
                    // Frame too large, discard
                    in_frame = false;
                    frame_len = 0;
                    pos += 1;
                    continue;
                }

                frame_buf[frame_len..frame_len + copy_len]
                    .copy_from_slice(&tcp_buf[pos..pos + copy_len]);

                // Scan for EOI in newly copied data (check cross-boundary too)
                let scan_start = if frame_len > 0 { frame_len - 1 } else { 0 };
                frame_len += copy_len;
                pos += copy_len;

                if let Some(eoi) = find_marker(&frame_buf[scan_start..frame_len], 0xFF, 0xD9) {
                    let eoi_end = scan_start + eoi + 2;
                    in_frame = false;

                    // Rewind pos: we consumed past the EOI, put leftover back
                    let consumed_past_eoi = frame_len - eoi_end;
                    pos -= consumed_past_eoi;

                    // Decode and display the complete JPEG frame
                    let jpeg_data = &mut frame_buf[..eoi_end];
                    match decoder.decode(jpeg_data, |block_idx, block_width, block_height, data| {
                        let start_row = (block_idx as u16) * block_height;

                        // Clamp to display height
                        let visible_rows = if start_row + block_height > DISPLAY_HEIGHT {
                            if start_row >= DISPLAY_HEIGHT {
                                return;
                            }
                            DISPLAY_HEIGHT - start_row
                        } else {
                            block_height
                        };

                        let end_row = start_row + visible_rows - 1;
                        let pixel_count = (block_width as usize) * (visible_rows as usize);

                        // Direct cast: decoder outputs RGB565-LE which matches native u16
                        // layout on this little-endian CPU. Rgb565 wraps RawU16(u16).
                        let pixels = unsafe {
                            core::slice::from_raw_parts(
                                data.as_ptr() as *const u16,
                                pixel_count,
                            )
                        };
                        let _ = display.set_pixels(
                            0,
                            start_row,
                            block_width - 1,
                            end_row,
                            pixels.iter().map(|&raw| Rgb565::from(RawU16::new(raw))),
                        );
                    }) {
                        Ok(_) => {}
                        Err(e) => {
                            println!("decode error: {}", e);
                        }
                    }

                    frame_len = 0;
                }
            }
        }
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.capabilities());
    loop {
        match esp_radio::wifi::sta_state() {
            WifiStaState::Connected => {
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                Timer::after(Duration::from_millis(5000)).await
            }
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = ModeConfig::Client(
                ClientConfig::default()
                    .with_ssid(SSID.into())
                    .with_password(PASSWORD.into()),
            );
            controller.set_config(&client_config).unwrap();
            println!("Starting wifi");
            controller.start_async().await.unwrap();
            println!("Wifi started!");
        }
        println!("About to connect...");

        match controller.connect_async().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}
