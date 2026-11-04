use crate::config::Config;
use esp_idf_svc::{
    eventloop::{EspSubscription, EspSystemEventLoop, System},
    hal::modem::Modem,
    nvs::EspDefaultNvsPartition,
    sntp::{EspSntp, SyncStatus},
    wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi, WifiEvent},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

pub struct Network {
    wifi: BlockingWifi<EspWifi<'static>>,
    sntp: Option<EspSntp<'static>>,
    disconnected: Arc<AtomicBool>,
    _connection_events: EspSubscription<'static, System>,
}
impl Network {
    pub fn new(modem: Modem<'static>, nvs: EspDefaultNvsPartition) -> Result<Self, &'static str> {
        let events = EspSystemEventLoop::take().map_err(|_| "Event loop initialization failed")?;
        let wifi = EspWifi::new(modem, events.clone(), Some(nvs))
            .map_err(|_| "Wi-Fi initialization failed")?;
        let disconnected = Arc::new(AtomicBool::new(false));
        let failed = disconnected.clone();
        let connection_events = events
            .subscribe::<WifiEvent, _>(move |event| {
                if matches!(event, WifiEvent::StaDisconnected(_)) {
                    failed.store(true, Ordering::Release);
                }
            })
            .map_err(|_| "Wi-Fi event binding failed")?;
        Ok(Self {
            wifi: BlockingWifi::wrap(wifi, events).map_err(|_| "Wi-Fi event binding failed")?,
            sntp: None,
            disconnected,
            _connection_events: connection_events,
        })
    }
    pub fn connect(&mut self, c: &Config) -> Result<(), &'static str> {
        self.connect_with_progress(c, || {})
    }

    pub fn connect_with_progress(
        &mut self,
        c: &Config,
        mut waiting: impl FnMut(),
    ) -> Result<(), &'static str> {
        let mut next_feedback = Instant::now() + Duration::from_secs(1);
        if !self.wifi.is_up().map_err(|_| "Wi-Fi state failed")? {
            if self.wifi.is_started().unwrap_or(false) {
                self.wifi.stop().map_err(|_| "Wi-Fi stop failed")?;
            }
            self.wifi
                .set_configuration(&Configuration::Client(ClientConfiguration {
                    ssid: c
                        .ssid
                        .as_str()
                        .try_into()
                        .map_err(|_| "SSID exceeds limit")?,
                    password: c
                        .password
                        .as_str()
                        .try_into()
                        .map_err(|_| "Password exceeds limit")?,
                    auth_method: if c.password.is_empty() {
                        AuthMethod::None
                    } else {
                        AuthMethod::WPA2Personal
                    },
                    ..Default::default()
                }))
                .map_err(|_| "Wi-Fi configuration failed")?;
            self.wifi.start().map_err(|_| "Wi-Fi start failed")?;
            log::info!("Connecting Wi-Fi");
            self.disconnected.store(false, Ordering::Release);
            // Start without the blocking wrapper so rejection events can end
            // the attempt immediately. One deadline covers association + DHCP.
            let deadline = Instant::now() + Duration::from_secs(8);
            self.wifi
                .wifi_mut()
                .connect()
                .map_err(|_| "Wi-Fi connection failed or timed out")?;
            while !self.wifi.is_up().map_err(|_| "Wi-Fi state failed")? {
                if self.disconnected.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return Err("Wi-Fi connection failed or timed out");
                }
                if Instant::now() >= next_feedback {
                    waiting();
                    next_feedback = Instant::now() + Duration::from_millis(300);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        log::info!("Wi-Fi ready; synchronizing time before TLS");
        if self.sntp.is_none() {
            self.sntp = Some(EspSntp::new_default().map_err(|_| "SNTP initialization failed")?);
        }
        // SNTP's COMPLETED status can be consumed/reset by the C implementation.
        // A previously synchronized clock remains usable for later manual turns.
        if std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() > 1_700_000_000)
            .unwrap_or(false)
        {
            log::info!("Wi-Fi and synchronized time ready");
            return Ok(());
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.sntp.as_ref().unwrap().get_sync_status() != SyncStatus::Completed {
            if Instant::now() >= deadline {
                return Err("SNTP timed out; TLS not attempted");
            }
            if Instant::now() >= next_feedback {
                waiting();
                next_feedback = Instant::now() + Duration::from_millis(300);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        log::info!("Wi-Fi and synchronized time ready");
        Ok(())
    }
    pub fn stop(&mut self) {
        let _ = self.wifi.stop();
    }
}
