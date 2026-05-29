use soapysdr;
use std::fs;
use std::path::{Path, PathBuf};
use tetra_config::bluestation::{SharedConfig, StackMode, sec_phy_soapy::CfgSoapySdr};

use tetra_pdus::phy::traits::rxtx_dev::RxTxDevError;
use tetra_pdus::phy::traits::rxtx_dev::RxTxDirection;
use tetra_pdus::phy::traits::rxtx_dev::SoapyStreamErrorCode;

use super::dsp_types::*;
use super::soapy_settings;
use super::soapy_settings::{SdrSettings, SupportedDevice};
use super::soapy_time::{ticks_to_time_ns, time_ns_to_ticks};
use super::sx1255_autocal::{AutocalFrequencies, Sx1255Autocal};

type StreamType = ComplexSample;
const SOAPY_FREQ_OFFSET: f64 = 20000.0;
const SOAPY_RX_STREAM_TIMEOUT_US: i64 = 20_000;
const SOAPY_TX_STREAM_TIMEOUT_US: i64 = 50_000;

pub struct RxResult {
    /// Number of samples read
    pub len: usize,
    /// Sample counter for the first sample read
    pub count: SampleCount,
    /// Hardware timestamp of the first sample read, when reported by the SDR.
    pub time_ns: Option<i64>,
}

pub struct SoapyIo {
    rx_ch: usize,
    tx_ch: usize,
    rx_fs: f64,
    tx_fs: f64,
    /// Timestamp for the first sample read from SDR.
    /// This is subtracted from all following timestamps,
    /// so that sample counter startsB210 from 0 even if timestamp does not.
    initial_time: Option<i64>,
    rx_next_count: SampleCount,
    prev_time_ns: i64,
    last_hardware_time_ns: Option<i64>,
    stale_hardware_time_reads: u64,
    stale_hardware_time_warn_at: u64,
    stream_restarts: u64,
    stream_restart_log_at: u64,

    /// If false, timestamp of latest RX read is used to estimate
    /// current hardware time. This is used in case get_hardware_time
    /// is unacceptably slow or not supported.
    use_get_hardware_time: bool,

    dev: soapysdr::Device,
    /// Receive stream. None if receiving is disabled.
    rx: Option<soapysdr::RxStream<StreamType>>,
    /// Transmit stream. None if transmitting is disabled.
    tx: Option<soapysdr::TxStream<StreamType>>,
    sx1255_autocal: Sx1255Autocal,
}

/// Soapy/Lime timestamps can occasionally jitter by a single sample.
/// Treat tiny deltas as contiguous to avoid triggering large block realignments downstream.
const RX_TIMESTAMP_JITTER_TOLERANCE_SAMPLES: SampleCount = 1;
const HARDWARE_TIME_STALE_WARN_READS: u64 = 8;

/// It is annoying to repeat error handling so do that in a macro.
/// ? could be used but then it could not print which SoapySDR call failed.
macro_rules! soapycheck {
    ($text:literal, $soapysdr_call:expr) => {
        match $soapysdr_call {
            Ok(ret) => ret,
            Err(err) => {
                tracing::error!("SoapySDR: Failed to {}: {}", $text, err);
                return Err(err);
            }
        }
    };
}

impl SoapyIo {
    pub fn new(cfg: &SharedConfig) -> Result<Self, soapysdr::Error> {
        let binding = cfg.config();
        let soapy_cfg = binding
            .phy_io
            .soapysdr
            .as_ref()
            .expect("SoapySdr config must be set for SoapySdr PhyIo");

        let mode = cfg.config().stack_mode;

        let (dev, sdr_settings, detected_device) = open_device(&soapy_cfg, mode)?;
        let librestation_runtime_settings = (detected_device == SupportedDevice::LibreStation).then(|| sdr_settings.clone());

        let rx_ch = sdr_settings.rx_ch;
        let tx_ch = sdr_settings.tx_ch;

        // Get PPM corrected freqs
        let (dl_corrected, _) = soapy_cfg.dl_freq_corrected();
        let (ul_corrected, _) = soapy_cfg.ul_freq_corrected();

        let rx_offset = if detected_device == SupportedDevice::LibreStation {
            0.0
        } else {
            SOAPY_FREQ_OFFSET
        };

        let (rx_freq, tx_freq) = match mode {
            StackMode::Bs => (
                Some(ul_corrected - rx_offset), // Offset RX center frequency from carrier frequency unless the frontend is already channelized.
                Some(dl_corrected),
            ),
            StackMode::Ms => (
                Some(dl_corrected - rx_offset), // Offset RX center frequency from carrier frequency unless the frontend is already channelized.
                Some(ul_corrected),
            ),
            StackMode::Mon => {
                unimplemented!("Monitor mode not implemented yet");
            }
        };

        let rx_enabled = rx_freq.is_some();
        let tx_enabled = tx_freq.is_some();
        let mut sx1255_autocal = Sx1255Autocal::new(
            soapy_cfg.sx1255_autocal.clone(),
            detected_device == SupportedDevice::SXceiver,
            AutocalFrequencies {
                rx_hz: rx_freq,
                tx_hz: tx_freq,
            },
        );

        let mut rx_fs: f64 = 0.0;
        if rx_enabled {
            soapycheck!(
                "set RX sample rate",
                dev.set_sample_rate(soapysdr::Direction::Rx, rx_ch, sdr_settings.fs)
            );
            // Read the actual sample rate obtained and store it
            // to avoid having to read it again every time it is needed.
            rx_fs = soapycheck!("get RX sample rate", dev.sample_rate(soapysdr::Direction::Rx, rx_ch));
        }
        let mut tx_fs: f64 = 0.0;
        if tx_enabled {
            soapycheck!(
                "set TX sample rate",
                dev.set_sample_rate(soapysdr::Direction::Tx, tx_ch, sdr_settings.fs)
            );
            tx_fs = soapycheck!("get TX sample rate", dev.sample_rate(soapysdr::Direction::Tx, tx_ch));
        }

        if rx_enabled {
            // If rx_enabled is true, we already know rx_freq is not None,
            // so unwrap is fine here.
            soapycheck!(
                "set RX center frequency",
                dev.set_frequency(soapysdr::Direction::Rx, rx_ch, rx_freq.unwrap(), soapysdr::Args::new())
            );

            if let Some(ref ant) = sdr_settings.rx_ant {
                soapycheck!("set RX antenna", dev.set_antenna(soapysdr::Direction::Rx, rx_ch, ant.as_str()));
            }

            for (name, gain) in &sdr_settings.rx_gain {
                soapycheck!(
                    "set RX gain",
                    dev.set_gain_element(soapysdr::Direction::Rx, rx_ch, name.as_str(), *gain)
                );
            }
        }

        if tx_enabled {
            soapycheck!(
                "set TX center frequency",
                dev.set_frequency(soapysdr::Direction::Tx, tx_ch, tx_freq.unwrap(), soapysdr::Args::new())
            );

            if let Some(ref ant) = sdr_settings.tx_ant {
                soapycheck!("set TX antenna", dev.set_antenna(soapysdr::Direction::Tx, tx_ch, ant.as_str()));
            }

            for (name, gain) in &sdr_settings.tx_gain {
                soapycheck!(
                    "set TX gain",
                    dev.set_gain_element(soapysdr::Direction::Tx, tx_ch, name.as_str(), *gain)
                );
            }
        }

        sx1255_autocal.startup(&dev, rx_ch, tx_ch);
        sx1255_autocal.startup_loopback_calibration(&dev, rx_ch, tx_ch, rx_fs, &sdr_settings.rx_args, &sdr_settings.tx_args);

        let mut rx_args = soapysdr::Args::new();
        for (key, value) in sdr_settings.rx_args {
            rx_args.set(key, value);
        }

        let mut tx_args = soapysdr::Args::new();
        for (key, value) in sdr_settings.tx_args {
            tx_args.set(key, value);
        }

        let mut rx = if rx_enabled {
            Some(soapycheck!("setup RX stream", dev.rx_stream_args(&[rx_ch], rx_args)))
        } else {
            None
        };
        let mut tx = if tx_enabled {
            Some(soapycheck!("setup TX stream", dev.tx_stream_args(&[tx_ch], tx_args)))
        } else {
            None
        };
        if let Some(rx) = &mut rx {
            soapycheck!("activate RX stream", rx.activate(None));
            tracing::info!(direction = "RX", reason = "startup", "SoapySDR stream activated");
        }
        if let Some(tx) = &mut tx {
            soapycheck!("activate TX stream", tx.activate(None));
            tracing::info!(direction = "TX", reason = "startup", "SoapySDR stream activated");
        }
        set_ad936x_runtime_active();
        if let Some(settings) = &librestation_runtime_settings {
            reapply_librestation_runtime_settings(&dev, settings, rx_freq, tx_freq)?;
        }
        Ok(Self {
            rx_ch,
            tx_ch,
            rx_fs,
            tx_fs,
            initial_time: None,
            rx_next_count: 0,
            prev_time_ns: -1,
            last_hardware_time_ns: None,
            stale_hardware_time_reads: 0,
            stale_hardware_time_warn_at: HARDWARE_TIME_STALE_WARN_READS,
            stream_restarts: 0,
            stream_restart_log_at: 1,
            use_get_hardware_time: sdr_settings.use_get_hardware_time,
            dev,
            rx,
            tx,
            sx1255_autocal,
        })
    }

    pub fn receive(&mut self, buffer: &mut [StreamType]) -> Result<RxResult, RxTxDevError> {
        self.sx1255_autocal.periodic(&self.dev, self.rx_ch, self.tx_ch);
        let result = if let Some(rx) = &mut self.rx {
            // RX is enabled
            match rx.read(&mut [buffer], SOAPY_RX_STREAM_TIMEOUT_US) {
                Ok(len) => {
                    self.sx1255_autocal.rx_startup_compensation().apply(&mut buffer[..len]);

                    // Get timestamp, set initial time if not yet set
                    let time = rx.time_ns();
                    // rust-soapysdr does not let us if a timestamp was available
                    // so we have to guess by checking whether it has changed from its previous value.
                    let timestamp_available = time != self.prev_time_ns;
                    let time_ns = timestamp_available.then_some(time);
                    self.prev_time_ns = time;

                    if self.initial_time.is_none() && timestamp_available {
                        self.initial_time = Some(time - ticks_to_time_ns(self.rx_next_count, self.rx_fs));
                        tracing::trace!("Set initial_time to {} ns", self.initial_time.unwrap());
                    };

                    // Re-compute total count from timestamp (gracefully handles lost samples).
                    let mut count = if timestamp_available {
                        time_ns_to_ticks(time - self.initial_time.unwrap(), self.rx_fs)
                    } else {
                        // If timestamp was not available,
                        // assume the read continues right after the previous read.
                        // Some drivers, particularly SoapyRemote,
                        // may provide a timestamp only in some of the reads.
                        self.rx_next_count
                    };

                    // Smooth tiny timestamp jitter (e.g. +/-1 sample) to keep counters monotonic
                    // This is known to happen for LimeSDR Mini v2 after some time
                    let delta_from_expected = count - self.rx_next_count;
                    if delta_from_expected.abs() <= RX_TIMESTAMP_JITTER_TOLERANCE_SAMPLES {
                        if delta_from_expected != 0 {
                            // Re-anchor phase so persistent +/-1 sample offset is corrected
                            let initial_time = self.initial_time.unwrap() + ticks_to_time_ns(delta_from_expected, self.rx_fs); // unwrap never fails
                            self.initial_time = Some(initial_time);
                            tracing::debug!(
                                "RX timestamp jitter {} sample(s); re-anchoring initial_time by {} ns",
                                delta_from_expected,
                                ticks_to_time_ns(delta_from_expected, self.rx_fs)
                            );
                        }
                        count = self.rx_next_count;
                    }

                    // Store expected sample count for the next sample to be read.
                    // This is used in case timestamp is missing.
                    self.rx_next_count = count + len as SampleCount;

                    Ok(RxResult { len, count, time_ns })
                }
                Err(err) => Err(soapy_error(RxTxDirection::Rx, "read stream", err)),
            }
        } else {
            // RX is disabled
            Err(RxTxDevError::RxReadError)
        };

        if let Err(err) = &result {
            if soapy_error_needs_restart(err) {
                self.restart_stream_after_error(RxTxDirection::Rx, "rx_read_error");
            }
        }

        result
    }

    pub fn transmit(&mut self, buffer: &[StreamType], count: Option<SampleCount>) -> Result<(), RxTxDevError> {
        let result = if let Some(tx) = &mut self.tx {
            if let Some(initial_time) = self.initial_time {
                let at_ns = count.map(|count| initial_time + ticks_to_time_ns(count, self.tx_fs));
                let write_result = tx
                    .write_all(&[buffer], at_ns, false, SOAPY_TX_STREAM_TIMEOUT_US)
                    .map_err(|err| soapy_error(RxTxDirection::Tx, "write stream", err));

                if write_result.is_ok() { check_tx_status(tx) } else { write_result }
            } else {
                // initial_time is not available, so TX is not possible yet
                Err(RxTxDevError::LateTx {
                    target_sample: count.unwrap_or_default(),
                    current_sample: 0,
                    min_headroom_samples: 0,
                    message: "TX requested before RX timestamp established".to_string(),
                })
            }
        } else {
            // TX is disabled
            Err(RxTxDevError::SoapyStreamError {
                direction: RxTxDirection::Tx,
                code: SoapyStreamErrorCode::NotSupported,
                operation: "write stream",
                message: "TX stream is disabled".to_string(),
            })
        };

        if let Err(err) = &result {
            if soapy_error_needs_restart(err) {
                self.restart_stream_after_error(RxTxDirection::Tx, "tx_write_error");
            }
        }

        result
    }

    pub fn current_time(&mut self) -> Result<i64, RxTxDevError> {
        let time = self
            .dev
            .get_hardware_time(None)
            .map_err(|err| soapy_error(RxTxDirection::Device, "get hardware time", err))?;

        if self.last_hardware_time_ns == Some(time) {
            self.stale_hardware_time_reads = self.stale_hardware_time_reads.saturating_add(1);
            if self.stale_hardware_time_reads >= self.stale_hardware_time_warn_at {
                tracing::warn!(
                    hardware_time_ns = time,
                    stale_reads = self.stale_hardware_time_reads,
                    "SoapySDR hardware time has not advanced"
                );
                self.stale_hardware_time_warn_at = self.stale_hardware_time_warn_at.saturating_mul(2).max(1);
            }
        } else {
            self.last_hardware_time_ns = Some(time);
            self.stale_hardware_time_reads = 0;
            self.stale_hardware_time_warn_at = HARDWARE_TIME_STALE_WARN_READS;
        }

        Ok(time)
    }

    fn restart_stream_after_error(&mut self, direction: RxTxDirection, reason: &'static str) {
        self.stream_restarts = self.stream_restarts.saturating_add(1);
        let log_this_restart = self.stream_restarts >= self.stream_restart_log_at;
        if log_this_restart {
            self.stream_restart_log_at = self.stream_restart_log_at.saturating_mul(2).max(1);
        }

        let result = match direction {
            RxTxDirection::Rx => {
                if let Some(rx) = &mut self.rx {
                    if rx.active() {
                        let _ = rx.deactivate(None);
                    }
                    rx.activate(None).map_err(|err| err.to_string())
                } else {
                    Err("RX stream is disabled".to_string())
                }
            }
            RxTxDirection::Tx => {
                if let Some(tx) = &mut self.tx {
                    if tx.active() {
                        let _ = tx.deactivate(None);
                    }
                    tx.activate(None).map_err(|err| err.to_string())
                } else {
                    Err("TX stream is disabled".to_string())
                }
            }
            RxTxDirection::Device => Err("device direction has no stream to restart".to_string()),
        };

        if log_this_restart || result.is_err() {
            match result {
                Ok(()) => tracing::warn!(
                    ?direction,
                    reason,
                    stream_restarts = self.stream_restarts,
                    "SoapySDR stream restarted"
                ),
                Err(err) => tracing::error!(
                    ?direction,
                    reason,
                    stream_restarts = self.stream_restarts,
                    error = %err,
                    "SoapySDR stream restart failed"
                ),
            }
        }
    }

    /// Current hardware time as RX sample count
    pub fn rx_current_count(&mut self) -> Result<SampleCount, RxTxDevError> {
        if !self.rx_enabled() {
            return Ok(0);
        }
        if self.use_get_hardware_time {
            Ok(time_ns_to_ticks(self.current_time()? - self.initial_time.unwrap_or(0), self.rx_fs))
        } else {
            Ok(self.rx_next_count - 1)
        }
    }

    /// Current hardware time as TX sample count
    pub fn tx_current_count(&mut self) -> Result<SampleCount, RxTxDevError> {
        if !self.tx_enabled() {
            return Ok(0);
        }
        if self.use_get_hardware_time {
            Ok(time_ns_to_ticks(self.current_time()? - self.initial_time.unwrap_or(0), self.tx_fs))
        } else {
            // Assumes equal RX and TX sample rates
            // and does not work if RX is disabled.
            // This is not a problem right now but could be fixed if needed.
            Ok(self.rx_next_count - 1)
        }
    }

    pub fn tx_possible(&self) -> bool {
        // initial_time is obtained from the first RX read (that includes a timestamp),
        // so prevent TX before it is available.
        self.tx_enabled() && self.initial_time.is_some()
    }

    pub fn rx_sample_rate(&self) -> f64 {
        self.rx_fs
    }

    pub fn tx_sample_rate(&self) -> f64 {
        self.tx_fs
    }

    pub fn rx_center_frequency(&self) -> Result<f64, soapysdr::Error> {
        self.dev.frequency(soapysdr::Direction::Rx, self.rx_ch)
    }

    pub fn tx_center_frequency(&self) -> Result<f64, soapysdr::Error> {
        self.dev.frequency(soapysdr::Direction::Tx, self.tx_ch)
    }

    pub fn rx_enabled(&self) -> bool {
        self.rx.is_some()
    }

    pub fn tx_enabled(&self) -> bool {
        self.tx.is_some()
    }
}

fn reapply_librestation_runtime_settings(
    dev: &soapysdr::Device,
    settings: &SdrSettings,
    rx_freq: Option<f64>,
    tx_freq: Option<f64>,
) -> Result<(), soapysdr::Error> {
    let rx_ch = settings.rx_ch;
    let tx_ch = settings.tx_ch;

    if let Some(freq) = rx_freq {
        soapycheck!(
            "reapply LibreStation RX center frequency",
            dev.set_frequency(soapysdr::Direction::Rx, rx_ch, freq, soapysdr::Args::new())
        );
    }
    if let Some(freq) = tx_freq {
        soapycheck!(
            "reapply LibreStation TX center frequency",
            dev.set_frequency(soapysdr::Direction::Tx, tx_ch, freq, soapysdr::Args::new())
        );
    }
    if let Some(ref ant) = settings.rx_ant {
        soapycheck!(
            "reapply LibreStation RX antenna",
            dev.set_antenna(soapysdr::Direction::Rx, rx_ch, ant.as_str())
        );
    }
    if let Some(ref ant) = settings.tx_ant {
        soapycheck!(
            "reapply LibreStation TX antenna",
            dev.set_antenna(soapysdr::Direction::Tx, tx_ch, ant.as_str())
        );
    }
    for (name, gain) in &settings.rx_gain {
        soapycheck!(
            "reapply LibreStation RX gain",
            dev.set_gain_element(soapysdr::Direction::Rx, rx_ch, name.as_str(), *gain)
        );
    }
    for (name, gain) in &settings.tx_gain {
        soapycheck!(
            "reapply LibreStation TX gain",
            dev.set_gain_element(soapysdr::Direction::Tx, tx_ch, name.as_str(), *gain)
        );
    }

    tracing::info!("LibreStation AD936x runtime RF settings reapplied");
    Ok(())
}

impl Drop for SoapyIo {
    fn drop(&mut self) {
        tracing::info!("SoapySDR shutdown: deactivating streams and putting AD936x in standby");
        if let Some(tx) = &mut self.tx {
            if tx.active() {
                if let Err(err) = tx.deactivate(None) {
                    tracing::warn!("SoapySDR shutdown: failed to deactivate TX stream: {}", err);
                } else {
                    tracing::info!(direction = "TX", reason = "shutdown", "SoapySDR stream deactivated");
                }
            }
        }
        if let Some(rx) = &mut self.rx {
            if rx.active() {
                if let Err(err) = rx.deactivate(None) {
                    tracing::warn!("SoapySDR shutdown: failed to deactivate RX stream: {}", err);
                } else {
                    tracing::info!(direction = "RX", reason = "shutdown", "SoapySDR stream deactivated");
                }
            }
        }
        set_ad936x_standby();
    }
}

fn find_ad936x_phy_path() -> Option<PathBuf> {
    let entries = fs::read_dir("/sys/bus/iio/devices").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = fs::read_to_string(path.join("name")).ok()?;
        if name.trim() == "ad9361-phy" {
            return Some(path);
        }
    }
    None
}

fn write_attr<P: AsRef<Path>>(base: P, name: &str, value: &str) {
    let path = base.as_ref().join(name);
    if let Err(err) = fs::write(&path, value) {
        tracing::debug!("AD936x attribute write skipped {}={}: {}", path.display(), value, err);
    }
}

fn set_ad936x_runtime_active() {
    let Some(path) = find_ad936x_phy_path() else { return };
    write_attr(&path, "out_altvoltage0_RX_LO_powerdown", "0");
    write_attr(&path, "out_altvoltage1_TX_LO_powerdown", "0");
    write_attr(&path, "ensm_mode", "fdd");
    tracing::info!("AD936x runtime active state requested");
}

fn set_ad936x_standby() {
    let Some(path) = find_ad936x_phy_path() else { return };
    write_attr(&path, "out_altvoltage1_TX_LO_powerdown", "1");
    write_attr(&path, "out_altvoltage0_RX_LO_powerdown", "1");
    write_attr(&path, "ensm_mode", "sleep");
    tracing::info!("AD936x standby state requested");
}

fn check_tx_status(tx: &mut soapysdr::TxStream<StreamType>) -> Result<(), RxTxDevError> {
    let mut chan_mask = 0;
    let mut flags = 0;
    let mut time_ns = 0;
    match tx.read_status(&mut chan_mask, &mut flags, &mut time_ns, 0) {
        Ok(status) => {
            tracing::debug!(status, chan_mask, flags, time_ns, "SoapySDR TX stream status reported after write");
            Ok(())
        }
        Err(err) if err.code == soapysdr::ErrorCode::Timeout || err.code == soapysdr::ErrorCode::NotSupported => Ok(()),
        Err(err) => Err(soapy_error(RxTxDirection::Tx, "read TX status", err)),
    }
}

fn soapy_error(direction: RxTxDirection, operation: &'static str, err: soapysdr::Error) -> RxTxDevError {
    RxTxDevError::SoapyStreamError {
        direction,
        code: map_soapy_error_code(err.code),
        operation,
        message: err.message,
    }
}

fn map_soapy_error_code(code: soapysdr::ErrorCode) -> SoapyStreamErrorCode {
    match code {
        soapysdr::ErrorCode::Timeout => SoapyStreamErrorCode::Timeout,
        soapysdr::ErrorCode::StreamError => SoapyStreamErrorCode::StreamError,
        soapysdr::ErrorCode::Corruption => SoapyStreamErrorCode::Corruption,
        soapysdr::ErrorCode::Overflow => SoapyStreamErrorCode::Overflow,
        soapysdr::ErrorCode::NotSupported => SoapyStreamErrorCode::NotSupported,
        soapysdr::ErrorCode::TimeError => SoapyStreamErrorCode::TimeError,
        soapysdr::ErrorCode::Underflow => SoapyStreamErrorCode::Underflow,
        soapysdr::ErrorCode::Other => SoapyStreamErrorCode::Other,
        _ => SoapyStreamErrorCode::Other,
    }
}

fn soapy_error_needs_restart(err: &RxTxDevError) -> bool {
    match err {
        RxTxDevError::SoapyStreamError { code, .. } => !matches!(
            code,
            SoapyStreamErrorCode::Timeout | SoapyStreamErrorCode::NotSupported | SoapyStreamErrorCode::Other
        ),
        RxTxDevError::LateTx { .. } => false,
        RxTxDevError::RxEndOfData | RxTxDevError::RxReadError => false,
    }
}

// Messy logic related to opening a device follows...

/// Struct to temporarily hold stuff related to opening and detecting a device
struct OpenedDevice {
    dev_args: soapysdr::Args,
    dev: soapysdr::Device,
    driver_key: String,
    hardware_key: String,
    detected_device: SupportedDevice,
    soapyremote_used: bool,
}

fn open_given_device(dev_args: soapysdr::Args) -> Result<OpenedDevice, soapysdr::Error> {
    let soapyremote_used = match dev_args.get("driver") {
        Some("remote") => true,
        _ => false,
    };
    tracing::info!("Trying to open a device with arguments: {}", dev_args);

    let dev_args_copy: soapysdr::Args = dev_args.iter().collect();
    let dev = match soapysdr::Device::new(dev_args_copy) {
        Ok(dev) => dev,
        Err(err) => {
            tracing::info!("Skipping a SoapySDR device because opening failed: {}", err);
            return Err(err);
        }
    };
    let driver_key = dev.driver_key().unwrap_or_default();
    let hardware_key = dev.hardware_key().unwrap_or_default();

    // Check whether the device is supported
    if let Some(detected_device) = SupportedDevice::detect(&driver_key, &hardware_key) {
        tracing::info!(
            "Found supported device with driver_key '{}' hardware_key '{}'",
            driver_key,
            hardware_key
        );
        Ok(OpenedDevice {
            dev_args,
            dev,
            driver_key,
            hardware_key,
            detected_device,
            soapyremote_used,
        })
    } else {
        tracing::info!(
            "Skipping unsupported device with driver_key '{}' hardware_key '{}'",
            driver_key,
            hardware_key
        );
        Err(soapysdr::Error {
            code: soapysdr::ErrorCode::NotSupported,
            message: "Unsupported device".to_string(),
        })
    }
}

/// Enumerate devices and find the first supported device
fn find_supported_device(filter_args: soapysdr::Args) -> Result<OpenedDevice, soapysdr::Error> {
    for dev_args in soapycheck!("Enumerate SoapySDR devices", soapysdr::enumerate(filter_args)) {
        //tracing::info!("Trying to open a device with arguments: {}", args_formatted);
        match open_given_device(dev_args) {
            Ok(opened_device) => return Ok(opened_device),
            Err(_) => {}
        }
    }
    return Err(soapysdr::Error {
        code: soapysdr::ErrorCode::NotSupported,
        message: "No supported devices found".to_string(),
    });
}

/// Open a given device if argument string is given,
/// automatically find the first supported device if not.
fn open_device(soapy_cfg: &CfgSoapySdr, mode: StackMode) -> Result<(soapysdr::Device, SdrSettings, SupportedDevice), soapysdr::Error> {
    let mut opened_device = if let Some(arg_string) = &soapy_cfg.device {
        open_given_device(arg_string.as_str().into())
    } else {
        find_supported_device(soapysdr::Args::new())
    }?;

    let detected_device = opened_device.detected_device;
    let mut sdr_settings = match SdrSettings::get_settings(&soapy_cfg, detected_device, mode) {
        Ok(sdr_settings) => sdr_settings,
        Err(soapy_settings::Error::InvalidConfiguration) => {
            return Err(soapysdr::Error {
                code: soapysdr::ErrorCode::Other,
                message: "Invalid SDR device configuration".to_string(),
            });
        }
    };

    if opened_device.soapyremote_used {
        // Getting hardware time may be too slow over SoapyRemote
        tracing::info!("SoapyRemote detected, forcing use_get_hardware_time=false");
        sdr_settings.use_get_hardware_time = false;
    }

    tracing::info!("Using settings: {:?}", sdr_settings);

    // If additional driver arguments are needed, reopen the device with them
    if sdr_settings.dev_args.len() > 0 {
        // Append additional arguments from settings
        for (key, value) in &sdr_settings.dev_args {
            opened_device.dev_args.set(key.as_str(), value.as_str());
        }

        tracing::info!("Reopening device with additional arguments: {}", opened_device.dev_args);

        // Make sure device gets closed first. Not sure if needed.
        std::mem::drop(opened_device.dev);
        opened_device.dev = soapycheck!(
            "open SoapySDR device with additional arguments",
            soapysdr::Device::new(opened_device.dev_args)
        );
        // Make sure it is still the same device.
        // Unlikely to change, but who knows if a device got connected just in between,
        // or if the device broke from first opening attempt and something else got opened
        // because device arguments were not precise enough to guarantee a specific device.
        let new_driver_key = opened_device.dev.driver_key().unwrap_or_default();
        let new_hardware_key = opened_device.dev.hardware_key().unwrap_or_default();
        if new_driver_key != opened_device.driver_key || new_hardware_key != opened_device.hardware_key {
            tracing::info!(
                "Expected the same driver_key='{}' hardware_key='{}' after reopen, got driver_key='{}' hardware_key='{}'",
                opened_device.driver_key,
                opened_device.hardware_key,
                new_driver_key,
                new_hardware_key
            );
            return Err(soapysdr::Error {
                code: soapysdr::ErrorCode::Other,
                message: "Reopened a different device".to_string(),
            });
        }
    }

    Ok((opened_device.dev, sdr_settings, detected_device))
}
