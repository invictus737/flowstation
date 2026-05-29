//! Resampling, buffering and timestamp handling
//! between SDR device and modulator/demodulator code.

use rustfft;
use std::time::{Duration, Instant};
use tetra_config::bluestation::SharedConfig;
use tetra_core::TdmaTime;

use tetra_pdus::phy::traits::rxtx_dev::RxTxDev;
use tetra_pdus::phy::traits::rxtx_dev::RxTxDevError;
use tetra_pdus::phy::traits::rxtx_dev::TxSlotBits;
use tetra_pdus::phy::traits::rxtx_dev::{RxSlotBits, RxTiming};
use tetra_pdus::phy::traits::rxtx_dev::{RxTxDirection, SoapyStreamErrorCode};

use crate::phy::components::soapy_dev;

use super::demodulator;
use super::dsp_types::*;
use super::fcfb;
use super::modulator;
use super::soapyio;

pub struct SdrConfig<'a> {
    /// SoapySDR device arguments
    pub dev_args: &'a [(&'a str, &'a str)],
    /// SDR RX center frequency
    pub rx_freq: Option<f64>,
    /// SDR TX center frequency
    pub tx_freq: Option<f64>,
}

#[derive(Default)]
pub struct PhyConfig<'a> {
    /// Downlink/uplink carrier frequency pairs to monitor.
    /// Uplink frequency can be set to None to monitor downlink only.
    pub monitor_frequencies: &'a [(f64, Option<f64>)],
    /// Downlink carrier frequencies for a BS.
    pub bs_dl_frequencies: &'a [f64],
    /// Uplink carrier frequencies for a BS.
    pub bs_ul_frequencies: &'a [f64],
}

const DIRECT_MODEM_RATE_TOLERANCE_HZ: f64 = 1.0;
const DIRECT_MODEM_CENTER_TOLERANCE_HZ: f64 = 1.0;
const DIRECT_MODEM_BLOCK_SAMPLES: usize = 255 * demodulator::SPS;

fn is_direct_modem_rate(sample_rate: f64) -> bool {
    (sample_rate - demodulator::SAMPLE_RATE).abs() <= DIRECT_MODEM_RATE_TOLERANCE_HZ
}

fn is_direct_single_carrier(center_frequency: Option<f64>, carrier_frequencies: &[f64]) -> bool {
    let Some(center_frequency) = center_frequency else {
        return false;
    };
    let [carrier_frequency] = carrier_frequencies else {
        return false;
    };

    (center_frequency - carrier_frequency).abs() <= DIRECT_MODEM_CENTER_TOLERANCE_HZ
}

pub struct RxTxDevSoapySdr {
    sdr: soapyio::SoapyIo,
    rx_dsp: Option<RxDsp>,
    tx_dsp: Option<TxDsp>,
    timing: PhyTimingCounters,
}

type FftPlanner = rustfft::FftPlanner<RealSample>;

impl RxTxDevSoapySdr {
    pub fn new(cfg: &SharedConfig) -> Self {
        let mut fft_planner = rustfft::FftPlanner::new();

        // TODO FIXME currently no MS and MON support in the below statement; need to fix
        let config_guard = cfg.config();
        let soapy_cfg = config_guard
            .as_ref()
            .phy_io
            .soapysdr
            .as_ref()
            .expect("Soapysdr config must be set for Soapysdr PhyIo");

        let (dl_corrected, dl_err) = soapy_cfg.dl_freq_corrected();
        let (ul_corrected, ul_err) = soapy_cfg.ul_freq_corrected();

        tracing::info!(
            "Freqs: DL / UL: {:.6} MHz / {:.6} MHz   PPM: {:.2} -> err {:.0} / {:.0} hz, adj {:.6} MHz / {:.6} MHz",
            soapy_cfg.dl_freq / 1e6,
            soapy_cfg.ul_freq / 1e6,
            soapy_cfg.ppm_err,
            dl_err,
            ul_err,
            dl_corrected / 1e6,
            ul_corrected / 1e6
        );

        let phy_config = soapy_dev::PhyConfig {
            bs_dl_frequencies: &[dl_corrected],
            bs_ul_frequencies: &[ul_corrected],
            ..Default::default()
        };

        let mut sdr = soapyio::SoapyIo::new(cfg).unwrap();

        Self {
            rx_dsp: if sdr.rx_enabled() {
                Some(RxDsp::new(&mut fft_planner, &mut sdr, &phy_config))
            } else {
                None
            },

            tx_dsp: if sdr.tx_enabled() {
                Some(TxDsp::new(&mut fft_planner, &mut sdr, &phy_config))
            } else {
                None
            },

            sdr,
            timing: Default::default(),
        }
    }

    /// Process a block of received signal.
    /// Return true if processing can be continued,
    /// false if a slot has been demodulated and rxtx_timeslot should return.
    fn process_rx_block(&mut self) -> Result<bool, RxTxDevError> {
        if let Some(rx_dsp) = &mut self.rx_dsp {
            let direct_modem = rx_dsp.direct_modem();
            match rx_dsp.process_block(&mut self.sdr, &mut self.timing) {
                Err(RxTxDevError::SoapyStreamError {
                    direction: RxTxDirection::Rx,
                    code: SoapyStreamErrorCode::Timeout,
                    ..
                }) if direct_modem => {
                    tracing::trace!("PHY direct RX block not ready; ending RX processing for this TDMA tick");
                    Ok(false)
                }
                Err(
                    err @ RxTxDevError::SoapyStreamError {
                        direction: RxTxDirection::Rx,
                        code: SoapyStreamErrorCode::Timeout,
                        ..
                    },
                ) => {
                    tracing::warn!(error = %err, "PHY RX stream timeout; ending RX processing for this TDMA tick");
                    Ok(false)
                }
                result => result,
            }
        } else {
            Ok(false)
        }
    }

    /// Produce a block of transmit signal.
    /// Return true if processing can be continued,
    /// false if more data is needed
    /// or if it wants to wait before producing more.
    fn process_tx_block(&mut self, tx_slot: &[TxSlotBits]) -> Result<bool, RxTxDevError> {
        if let Some(tx_dsp) = &mut self.tx_dsp {
            if self.sdr.tx_possible() {
                tx_dsp.process_block(
                    &mut self.sdr,
                    self.rx_dsp.as_ref().map(|rx_dsp| rx_dsp.rx_block_count),
                    tx_slot,
                    &mut self.timing,
                )
            } else {
                Ok(false)
            }
        } else {
            Ok(false)
        }
    }
}

impl RxTxDev for RxTxDevSoapySdr {
    fn rxtx_timeslot<'a>(
        &'a mut self,
        tx_slot: &[TxSlotBits],
        // TODO multiple demodulators
    ) -> Result<Vec<Option<RxSlotBits<'a>>>, RxTxDevError> {
        let started = Instant::now();
        // First generate as much TX signal as possible at the moment.
        while match self.process_tx_block(tx_slot) {
            Ok(continue_processing) => continue_processing,
            Err(err) => {
                self.timing.record_rxtx_duration(started.elapsed(), Some(&err));
                return Err(err);
            }
        } {}

        while match self.process_rx_block() {
            Ok(continue_processing) => continue_processing,
            Err(err) => {
                self.timing.record_rxtx_duration(started.elapsed(), Some(&err));
                return Err(err);
            }
        } {
            // Continue producing TX signal if possible.
            while match self.process_tx_block(tx_slot) {
                Ok(continue_processing) => continue_processing,
                Err(err) => {
                    self.timing.record_rxtx_duration(started.elapsed(), Some(&err));
                    return Err(err);
                }
            } {}
        }

        self.timing.record_rxtx_duration(started.elapsed(), None);
        if let Some(rx_dsp) = &mut self.rx_dsp {
            Ok(rx_dsp.take_slot_bits())
        } else {
            Ok(Default::default())
        }
    }
}

#[derive(Default)]
struct PhyTimingCounters {
    late_tx_events: u64,
    late_tx_blocks: u64,
    tx_write_errors: u64,
    rx_sample_gaps: u64,
    rx_sample_gap_samples: i64,
    rxtx_timeslots: u64,
}

impl PhyTimingCounters {
    fn record_late_tx(
        &mut self,
        skipped_blocks: fcfb::BlockCount,
        target_block: fcfb::BlockCount,
        current_block: fcfb::BlockCount,
        min_ahead_blocks: fcfb::BlockCount,
    ) {
        self.late_tx_events = self.late_tx_events.saturating_add(1);
        self.late_tx_blocks = self.late_tx_blocks.saturating_add(skipped_blocks.max(0) as u64);

        if should_log_low_rate(self.late_tx_events) {
            tracing::error!(
                late_tx_events = self.late_tx_events,
                late_tx_blocks = self.late_tx_blocks,
                skipped_blocks,
                target_block,
                current_block,
                min_ahead_blocks,
                "PHY late TX block skipped"
            );
        }
    }

    fn record_tx_write_error(&mut self, err: &RxTxDevError) {
        self.tx_write_errors = self.tx_write_errors.saturating_add(1);
        if should_log_low_rate(self.tx_write_errors) {
            tracing::error!(
                tx_write_errors = self.tx_write_errors,
                error = %err,
                "PHY TX write/status error"
            );
        }
    }

    fn record_rx_sample_gap(&mut self, samples_lost: SampleCount, samples_to_skip: SampleCount, skipped_blocks: fcfb::BlockCount) {
        self.rx_sample_gaps = self.rx_sample_gaps.saturating_add(1);
        self.rx_sample_gap_samples = self.rx_sample_gap_samples.saturating_add(samples_lost.abs());
        if should_log_low_rate(self.rx_sample_gaps) {
            tracing::warn!(
                rx_sample_gaps = self.rx_sample_gaps,
                rx_sample_gap_samples = self.rx_sample_gap_samples,
                samples_lost,
                samples_to_skip,
                skipped_blocks,
                "PHY RX sample gap detected"
            );
        }
    }

    fn record_rxtx_duration(&mut self, duration: Duration, err: Option<&RxTxDevError>) {
        self.rxtx_timeslots = self.rxtx_timeslots.saturating_add(1);
        let duration_us = duration.as_micros();
        let slow = duration > Duration::from_millis(25);

        if slow || self.rxtx_timeslots == 1 || self.rxtx_timeslots % 256 == 0 || err.is_some() {
            if let Some(err) = err {
                tracing::warn!(
                    rxtx_timeslots = self.rxtx_timeslots,
                    duration_us,
                    late_tx_blocks = self.late_tx_blocks,
                    tx_write_errors = self.tx_write_errors,
                    rx_sample_gaps = self.rx_sample_gaps,
                    error = %err,
                    "PHY rxtx_timeslot ended with error"
                );
            } else if slow {
                tracing::warn!(
                    rxtx_timeslots = self.rxtx_timeslots,
                    duration_us,
                    late_tx_blocks = self.late_tx_blocks,
                    tx_write_errors = self.tx_write_errors,
                    rx_sample_gaps = self.rx_sample_gaps,
                    "PHY rxtx_timeslot duration high"
                );
            } else {
                tracing::debug!(
                    rxtx_timeslots = self.rxtx_timeslots,
                    duration_us,
                    late_tx_blocks = self.late_tx_blocks,
                    tx_write_errors = self.tx_write_errors,
                    rx_sample_gaps = self.rx_sample_gaps,
                    "PHY rxtx_timeslot duration"
                );
            }
        }
    }
}

fn should_log_low_rate(count: u64) -> bool {
    count == 1 || count.is_power_of_two()
}

struct RxDsp {
    rx_fcfb: Option<fcfb::AnalysisInputProcessor>,
    direct_modem: bool,

    rx_block_size: fcfb::InputBlockSize,
    rx_buffer: Vec<ComplexSample>,
    /// How much of rx_buffer has been filled
    rx_buffer_i: usize,
    rx_block_count: fcfb::BlockCount,
    rx_next_sample_count: Option<SampleCount>,

    monitors: Vec<MonitorDlUlPair>,
    ul_demodulators: Vec<DemodulatorChannel>,
    current_rx_timing: RxTiming,
}

impl RxDsp {
    fn direct_modem(&self) -> bool {
        self.direct_modem
    }

    fn new(fft_planner: &mut FftPlanner, sdr: &mut soapyio::SoapyIo, phy_config: &PhyConfig) -> Self {
        let sdr_sample_rate = sdr.rx_sample_rate();
        let direct_modem = is_direct_modem_rate(sdr_sample_rate)
            && phy_config.monitor_frequencies.is_empty()
            && is_direct_single_carrier(sdr.rx_center_frequency().ok(), phy_config.bs_ul_frequencies);

        let rx_fcfb_params = fcfb::AnalysisInputParameters {
            // Use a bin spacing of 500 Hz.
            // This is a submultiple of the 72 kHz modem sample rate
            // and allows tuning in steps of 500 Hz.
            fft_size: (sdr_sample_rate / 500.0).round() as usize,
            center_frequency: sdr.rx_center_frequency().unwrap(),
            sample_rate: sdr_sample_rate,
            overlap: fcfb::Overlap::O1_4,
        };

        let fcfb = (!direct_modem).then(|| fcfb::AnalysisInputProcessor::new(fft_planner, rx_fcfb_params));
        let rx_block_size = if let Some(fcfb) = &fcfb {
            fcfb.input_block_size()
        } else {
            fcfb::InputBlockSize {
                new: DIRECT_MODEM_BLOCK_SAMPLES,
                overlap: 0,
            }
        };

        if direct_modem {
            tracing::info!(
                sample_rate = sdr_sample_rate,
                block_samples = DIRECT_MODEM_BLOCK_SAMPLES,
                "Using direct 72 kS/s LibreStation modem RX path"
            );
        }

        Self {
            rx_block_size,
            rx_buffer: vec![num::zero(); rx_block_size.overlap + rx_block_size.new],
            rx_buffer_i: 0,
            rx_fcfb: fcfb,
            direct_modem,
            rx_block_count: 0,
            rx_next_sample_count: None,

            monitors: phy_config
                .monitor_frequencies
                .iter()
                .map(|(dl_freq, ul_freq)| MonitorDlUlPair {
                    dl: DemodulatorChannel::new(fft_planner, rx_fcfb_params, *dl_freq, demodulator::Mode::DlUnsynchronized),
                    ul: ul_freq
                        .as_ref()
                        .map(|ul_freq| DemodulatorChannel::new(fft_planner, rx_fcfb_params, *ul_freq, demodulator::Mode::Idle)),
                })
                .collect(),

            ul_demodulators: phy_config
                .bs_ul_frequencies
                .iter()
                .map(|ul_freq| {
                    if direct_modem {
                        DemodulatorChannel::new_direct(demodulator::Mode::Ul)
                    } else {
                        DemodulatorChannel::new(fft_planner, rx_fcfb_params, *ul_freq, demodulator::Mode::Ul)
                    }
                })
                .collect(),
            current_rx_timing: Default::default(),
        }
    }

    fn process_block(&mut self, sdr: &mut soapyio::SoapyIo, timing: &mut PhyTimingCounters) -> Result<bool, RxTxDevError> {
        if self.direct_modem {
            return self.process_direct_block(sdr, timing);
        }

        self.receive_block(sdr, timing)?;

        let fcfb_result = self.rx_fcfb.as_mut().unwrap().process(&self.rx_buffer[..], self.rx_block_count);
        let rx_timing = self.current_rx_timing;

        let mut continue_processing = true;

        for pair in self.monitors.iter_mut() {
            let continue_dl = pair.dl.process(fcfb_result, self.rx_block_count, rx_timing);
            if let Some(ul) = &mut pair.ul {
                ul.demodulator.sync_to_demodulator(&pair.dl.demodulator);
                continue_processing = ul.process(fcfb_result, self.rx_block_count, rx_timing) && continue_processing;
            } else {
                continue_processing = continue_dl && continue_processing;
            }
        }

        for demod in self.ul_demodulators.iter_mut() {
            continue_processing = demod.process(fcfb_result, self.rx_block_count, rx_timing) && continue_processing;
        }

        Ok(continue_processing)
    }

    fn process_direct_block(&mut self, sdr: &mut soapyio::SoapyIo, timing: &mut PhyTimingCounters) -> Result<bool, RxTxDevError> {
        while self.rx_buffer_i < self.rx_block_size.new {
            let remaining = self.rx_block_size.new - self.rx_buffer_i;
            let receive_result = sdr.receive(&mut self.rx_buffer[self.rx_buffer_i..self.rx_buffer_i + remaining]);
            let result = match receive_result {
                Ok(result) => result,
                Err(
                    err @ RxTxDevError::SoapyStreamError {
                        direction: RxTxDirection::Rx,
                        code: SoapyStreamErrorCode::Timeout,
                        ..
                    },
                ) if self.rx_buffer_i > 0 => {
                    tracing::trace!(
                        partial_samples = self.rx_buffer_i,
                        target_samples = self.rx_block_size.new,
                        error = %err,
                        "PHY direct RX timeout after partial block; preserving partial samples"
                    );
                    return Ok(false);
                }
                Err(err) => return Err(err),
            };

            let rx_timing = RxTiming {
                time_ns: result.time_ns,
                sample_count: Some(result.count),
            };
            self.current_rx_timing = rx_timing;

            if let Some(expected_count) = self.rx_next_sample_count {
                let samples_lost = result.count - expected_count;
                if samples_lost != 0 {
                    timing.record_rx_sample_gap(samples_lost, 0, 0);
                }
            }
            self.rx_next_sample_count = Some(result.count + result.len as SampleCount);

            let start = self.rx_buffer_i;
            let end = start + result.len;
            for demod in self.ul_demodulators.iter_mut() {
                demod.process_direct(&self.rx_buffer[start..end], result.count, rx_timing);
            }

            self.rx_buffer_i = end;
            if result.len == 0 {
                tracing::trace!("PHY direct RX returned zero samples; waiting for more data");
                return Ok(false);
            }
        }

        self.rx_buffer_i = 0;
        self.rx_block_count += 1;
        Ok(false)
    }

    fn receive_block(&mut self, sdr: &mut soapyio::SoapyIo, timing: &mut PhyTimingCounters) -> Result<(), RxTxDevError> {
        self.rx_block_count += 1;

        // Copy overlapping part from previous block to the beginning
        self.rx_buffer
            .copy_within(self.rx_block_size.new..self.rx_block_size.new + self.rx_block_size.overlap, 0);
        self.rx_buffer_i = self.rx_block_size.overlap;

        loop {
            let result = sdr.receive(&mut self.rx_buffer[self.rx_buffer_i..])?;

            let block_size = self.rx_block_size.new as SampleCount;
            let expected_count = self.rx_block_count as SampleCount * block_size + self.rx_buffer_i as SampleCount;
            let samples_lost = result.count - expected_count;
            self.current_rx_timing = RxTiming {
                time_ns: result.time_ns,
                sample_count: Some(result.count),
            };
            if samples_lost != 0 {
                // Samples have been lost.
                // Mark RX buffer as empty and skip the right number of samples
                // to receive the next full processing block in the next iteration.

                // Expected sample count for the next read,
                // assuming no more samples are lost.
                let next_count = result.count + result.len as SampleCount;
                // div_euclid always rounds down (towards negative numbers),
                // so use it with negations to round up to the next block.
                let next_possible_block = -next_count.div_euclid(-block_size) + 1;
                let next_block_beginning = next_possible_block * block_size;

                let mut samples_to_skip = next_block_beginning - next_count;

                timing.record_rx_sample_gap(samples_lost, samples_to_skip, next_possible_block - self.rx_block_count);

                self.rx_block_count = next_possible_block;
                self.rx_buffer_i = 0;

                // Repeat reads until the correct number of samples has been skipped.
                while samples_to_skip > 0 {
                    let result = sdr.receive(&mut self.rx_buffer[0..samples_to_skip as usize])?;
                    samples_to_skip -= result.len as SampleCount;
                }
            } else {
                self.rx_buffer_i += result.len;
                if self.rx_buffer_i == self.rx_buffer.len() {
                    // tracing::trace!("Received processing block {} ({} samples in SDR buffer)",
                    //     self.rx_block_count,
                    //     // incorrect if time is not available but does not really matter
                    //     sdr.rx_current_count().unwrap_or(0) - (result.count + result.len as SampleCount - 1),
                    // );
                    return Ok(());
                }
            }
        }
    }

    fn take_slot_bits<'a>(&'a mut self) -> Vec<Option<RxSlotBits<'a>>> {
        // TODO: avoid dynamic allocation here?
        let mut slot_bits = Vec::with_capacity(2 * self.monitors.len() + self.ul_demodulators.len());

        for pair in self.monitors.iter_mut() {
            slot_bits.push(pair.dl.demodulator.take_demodulated_slot());
            slot_bits.push(if let Some(ul) = &mut pair.ul {
                ul.demodulator.take_demodulated_slot()
            } else {
                None
            });
        }

        for demod in self.ul_demodulators.iter_mut() {
            slot_bits.push(demod.demodulator.take_demodulated_slot());
        }

        slot_bits
    }
}

struct TxDsp {
    fcfb: Option<fcfb::SynthesisOutputProcessor>,
    direct_modem: bool,
    direct_block_samples: usize,
    block_count: fcfb::BlockCount,
    schedule_initialized: bool,
    initial_time: i64,
    modulators: Vec<ModulatorChannel>,
    headroom: TxHeadroomLimiter,
    timing_config: TxTimingConfig,
    tx_direct_input: Vec<ComplexSample>,
    tx_output: Vec<ComplexSample>,
}

#[derive(Clone, Copy, Debug)]
struct TxTimingConfig {
    min_ahead_blocks: fcfb::BlockCount,
    max_ahead_blocks: fcfb::BlockCount,
    max_rx_ahead_blocks: fcfb::BlockCount,
}

impl TxTimingConfig {
    fn from_env(default_min_ahead_blocks: fcfb::BlockCount) -> Self {
        let mut cfg = Self {
            min_ahead_blocks: default_min_ahead_blocks,
            max_ahead_blocks: 60,
            max_rx_ahead_blocks: 60,
        };

        cfg.min_ahead_blocks = read_env_block_count("FLOWSTATION_PHY_TX_MIN_AHEAD_BLOCKS", cfg.min_ahead_blocks, 1);
        cfg.max_ahead_blocks = read_env_block_count("FLOWSTATION_PHY_TX_MAX_AHEAD_BLOCKS", cfg.max_ahead_blocks, cfg.min_ahead_blocks);
        cfg.max_rx_ahead_blocks = read_env_block_count(
            "FLOWSTATION_PHY_TX_MAX_RX_AHEAD_BLOCKS",
            cfg.max_rx_ahead_blocks,
            cfg.min_ahead_blocks,
        );

        tracing::info!(
            min_ahead_blocks = cfg.min_ahead_blocks,
            max_ahead_blocks = cfg.max_ahead_blocks,
            max_rx_ahead_blocks = cfg.max_rx_ahead_blocks,
            "PHY TX timing headroom configured"
        );

        cfg
    }
}

fn read_env_block_count(name: &'static str, default: fcfb::BlockCount, min: fcfb::BlockCount) -> fcfb::BlockCount {
    match std::env::var(name) {
        Ok(value) => match value.parse::<fcfb::BlockCount>() {
            Ok(parsed) => parsed.max(min),
            Err(err) => {
                tracing::warn!(name, value, error = %err, default, "Ignoring invalid PHY timing environment knob");
                default
            }
        },
        Err(_) => default,
    }
}

fn read_env_real(name: &'static str, default: RealSample) -> RealSample {
    match std::env::var(name) {
        Ok(value) => match value.parse::<RealSample>() {
            Ok(parsed) => parsed,
            Err(err) => {
                tracing::warn!(name, value, error = %err, default, "Ignoring invalid PHY real-valued environment knob");
                default
            }
        },
        Err(_) => default,
    }
}

impl TxDsp {
    fn new(fft_planner: &mut FftPlanner, sdr: &mut soapyio::SoapyIo, phy_config: &PhyConfig) -> Self {
        let sdr_sample_rate = sdr.tx_sample_rate();
        let direct_modem =
            is_direct_modem_rate(sdr_sample_rate) && is_direct_single_carrier(sdr.tx_center_frequency().ok(), phy_config.bs_dl_frequencies);
        let fcfb_params = fcfb::SynthesisOutputParameters {
            ifft_size: (sdr_sample_rate / 500.0).round() as usize,
            center_frequency: sdr.tx_center_frequency().unwrap(),
            sample_rate: sdr_sample_rate,
            overlap: fcfb::Overlap::O1_4,
        };

        let fcfb = (!direct_modem).then(|| fcfb::SynthesisOutputProcessor::new(fft_planner, fcfb_params));

        let mut modulators = Vec::<ModulatorChannel>::new();
        for dl_freq in phy_config.bs_dl_frequencies {
            modulators.push(if direct_modem {
                ModulatorChannel::new_direct(modulator::Mode::Dl)
            } else {
                ModulatorChannel::new(fft_planner, fcfb_params, *dl_freq, modulator::Mode::Dl)
            });
        }

        if direct_modem {
            tracing::info!(
                sample_rate = sdr_sample_rate,
                block_samples = DIRECT_MODEM_BLOCK_SAMPLES,
                "Using direct 72 kS/s LibreStation modem TX path"
            );
        }

        Self {
            fcfb,
            direct_modem,
            direct_block_samples: DIRECT_MODEM_BLOCK_SAMPLES,
            block_count: 0,
            schedule_initialized: false,
            initial_time: 0, // TODO: get it from RX
            modulators,
            headroom: TxHeadroomLimiter::from_env(),
            timing_config: TxTimingConfig::from_env(2),
            tx_direct_input: Vec::new(),
            tx_output: Vec::new(),
        }
    }

    fn process_block(
        &mut self,
        sdr: &mut soapyio::SoapyIo,
        latest_rx_block: Option<fcfb::BlockCount>,
        tx_slot: &[TxSlotBits],
        timing: &mut PhyTimingCounters,
    ) -> Result<bool, RxTxDevError> {
        let current_sample = sdr.tx_current_count()?;
        let output_block_size = if self.direct_modem {
            self.direct_block_samples
        } else {
            self.fcfb.as_ref().unwrap().output_block_size()
        };
        // Current time as block count
        let current_block = current_sample.div_euclid(output_block_size as SampleCount);

        let d = self.block_count - current_block;
        // Skip TX blocks in the past or in too near future
        let dmin = self.timing_config.min_ahead_blocks;
        if !self.schedule_initialized {
            self.block_count = current_block + dmin;
            self.schedule_initialized = true;
            self.align_direct_modulators(tx_slot, output_block_size);
            tracing::info!(
                target_block = self.block_count,
                current_block,
                min_ahead_blocks = dmin,
                "PHY TX schedule initialized"
            );
        } else if d < dmin {
            let new_block_count = current_block + dmin;
            let skipped_blocks = new_block_count - self.block_count;
            timing.record_late_tx(skipped_blocks, self.block_count, current_block, dmin);
            tracing::debug!(
                target_block = self.block_count,
                current_block,
                skipped_blocks,
                min_ahead_blocks = dmin,
                "Too late to produce TX block, skipping forward"
            );
            self.block_count = new_block_count;
            self.align_direct_modulators(tx_slot, output_block_size);
        }
        // Limit how far into future TX blocks are generated
        let dmax = self.timing_config.max_ahead_blocks;
        if d > dmax {
            return Ok(false);
        }
        // Also limit how far from the latest RX block TX blocks are generated.
        // This prevents TX from ending up in an infinite loop
        // which does not give a chance for RX signal to get processed.
        //
        // This is not strictly necessary right now but might become useful
        // with different modulator operating modes in the future.
        //
        // Maybe the limit using hardware time above is redundant.
        if let Some(latest_rx_block) = latest_rx_block {
            let d_rx = self.block_count - latest_rx_block;
            if d_rx > self.timing_config.max_rx_ahead_blocks {
                return Ok(false);
            }
        }

        if self.direct_modem {
            self.tx_direct_input.clear();
            self.tx_direct_input.resize(self.direct_block_samples, ComplexSample::ZERO);

            for (modulator, tx_slot) in self.modulators.iter_mut().zip(tx_slot) {
                if !modulator.process_direct(self.block_count, self.direct_block_samples, tx_slot, &mut self.tx_direct_input) {
                    return Ok(false);
                }
            }

            self.headroom.apply_in_place(&mut self.tx_direct_input);

            // TODO: compensate for delay of SDR
            let sdr_sample_count = self.tx_direct_input.len() as SampleCount * self.block_count;

            // Increment block count before calling sdr.transmit with ?,
            // so we do not end up producing the same block again even if transmit fails.
            self.block_count += 1;

            if let Err(err) = sdr.transmit(&self.tx_direct_input, Some(sdr_sample_count)) {
                timing.record_tx_write_error(&err);
                return Err(err);
            }
        } else {
            let fcfb = self.fcfb.as_mut().unwrap();
            for (modulator, tx_slot) in self.modulators.iter_mut().zip(tx_slot) {
                if !modulator.process(fcfb, self.block_count, tx_slot) {
                    return Ok(false);
                }
            }

            let tx_signal = fcfb.process();
            let scale = self.headroom.next_scale(tx_signal);

            // TODO: compensate for delay of SDR
            let sdr_sample_count = tx_signal.len() as SampleCount * self.block_count;

            // Increment block count before calling sdr.transmit with ?,
            // so we do not end up producing the same block again even if transmit fails.
            self.block_count += 1;

            let transmit_result = if scale == 1.0 {
                sdr.transmit(tx_signal, Some(sdr_sample_count))
            } else {
                TxHeadroomLimiter::copy_scaled(tx_signal, &mut self.tx_output, scale);
                sdr.transmit(&self.tx_output, Some(sdr_sample_count))
            };

            if let Err(err) = transmit_result {
                timing.record_tx_write_error(&err);
                return Err(err);
            }
        }

        // tracing::trace!("Produced transmit block {} ({} samples in future)",
        //     self.block_count - 1,
        //     sdr_sample_count - sdr.tx_current_count().unwrap_or(0),
        // );

        Ok(true)
    }

    fn align_direct_modulators(&mut self, tx_slot: &[TxSlotBits], block_samples: usize) {
        if !self.direct_modem {
            return;
        }

        for (modulator, tx_slot) in self.modulators.iter_mut().zip(tx_slot) {
            modulator.align_direct_reference(tx_slot.time, self.block_count as SampleCount * block_samples as SampleCount);
        }
    }
}

struct TxHeadroomLimiter {
    scale: RealSample,
    target: RealSample,
    target2: RealSample,
    recovery_per_block: RealSample,
    warn_cooldown_blocks: usize,
}

impl TxHeadroomLimiter {
    fn new(target: RealSample, recovery_per_block: RealSample) -> Self {
        Self {
            scale: 1.0,
            target,
            target2: target * target,
            recovery_per_block,
            warn_cooldown_blocks: 0,
        }
    }

    fn from_env() -> Self {
        let target = read_env_real("FLOWSTATION_PHY_TX_HEADROOM_TARGET", 0.85).clamp(0.1, 1.0);
        let recovery = read_env_real("FLOWSTATION_PHY_TX_HEADROOM_RECOVERY_PER_BLOCK", 1.0005).clamp(1.0, 1.01);
        Self::new(target, recovery)
    }

    fn next_scale(&mut self, input: &[ComplexSample]) -> RealSample {
        let mut peak2: RealSample = 0.0;
        for sample in input {
            peak2 = peak2.max(sample.re * sample.re + sample.im * sample.im);
        }

        let desired_scale = if peak2 > self.target2 { self.target / peak2.sqrt() } else { 1.0 };

        if desired_scale < self.scale {
            self.scale = desired_scale;
            if self.warn_cooldown_blocks == 0 {
                tracing::warn!(
                    "TX headroom limiter: reducing digital drive to {:.3} (block peak {:.3}, target {:.3})",
                    self.scale,
                    peak2.sqrt(),
                    self.target
                );
                self.warn_cooldown_blocks = 6000;
            }
        } else {
            self.scale = (self.scale * self.recovery_per_block).min(1.0);
            self.warn_cooldown_blocks = self.warn_cooldown_blocks.saturating_sub(1);
        }

        self.scale
    }

    fn copy_scaled(input: &[ComplexSample], output: &mut Vec<ComplexSample>, scale: RealSample) {
        output.clear();
        output.extend(input.iter().map(|sample| *sample * scale));
    }

    fn apply_in_place(&mut self, samples: &mut [ComplexSample]) {
        let scale = self.next_scale(samples);
        if scale == 1.0 {
            return;
        }

        for sample in samples {
            *sample *= scale;
        }
    }
}

struct DemodulatorChannel {
    downconverter: Option<fcfb::AnalysisOutputProcessor>,
    demodulator: demodulator::Demodulator,
}

impl DemodulatorChannel {
    fn new(
        fft_planner: &mut FftPlanner,
        analysis_in_params: fcfb::AnalysisInputParameters,
        frequency: f64,
        mode: demodulator::Mode,
    ) -> Self {
        Self {
            downconverter: Some(fcfb::AnalysisOutputProcessor::new_with_frequency(
                fft_planner,
                analysis_in_params,
                demodulator::SAMPLE_RATE,
                frequency,
                Some(25000.0),
            )),
            demodulator: demodulator::Demodulator::new(mode),
        }
    }

    fn new_direct(mode: demodulator::Mode) -> Self {
        Self {
            downconverter: None,
            demodulator: demodulator::Demodulator::new(mode),
        }
    }

    /// Return true if processing should be continued,
    /// false if a new demodulated slot is available.
    fn process(&mut self, fcfb_result: &fcfb::AnalysisIntermediateResult, block_count: fcfb::BlockCount, rx_timing: RxTiming) -> bool {
        let samples = self.downconverter.as_mut().unwrap().process(fcfb_result);
        self.demodulator.set_rx_timing(rx_timing);
        for (i, sample) in samples.iter().enumerate() {
            // TODO: include delay of FCFB in sample count
            self.demodulator.sample(
                *sample,
                block_count as SampleCount * samples.len() as SampleCount + i as SampleCount,
            );
        }
        !self.demodulator.demodulated_slot_available()
    }

    fn process_direct(&mut self, samples: &[ComplexSample], start_sample_count: SampleCount, rx_timing: RxTiming) -> bool {
        self.demodulator.set_rx_timing(rx_timing);
        for (i, sample) in samples.iter().enumerate() {
            self.demodulator.sample(*sample, start_sample_count + i as SampleCount);
        }
        !self.demodulator.demodulated_slot_available()
    }
}

struct ModulatorChannel {
    upconverter: Option<fcfb::SynthesisInputProcessor>,
    modulator: modulator::Modulator,
    /// Buffer for modulated signal at modulator sample rate.
    buffer: Option<fcfb::InputBuffer>,
    direct_buffer: Vec<ComplexSample>,
    /// How much of buffer is filled
    buffer_i: usize,
}

impl ModulatorChannel {
    fn new(
        fft_planner: &mut FftPlanner,
        synthesis_out_params: fcfb::SynthesisOutputParameters,
        frequency: f64,
        mode: modulator::Mode,
    ) -> Self {
        let upconverter = fcfb::SynthesisInputProcessor::new_with_frequency(
            fft_planner,
            synthesis_out_params,
            modulator::SAMPLE_RATE,
            frequency,
            Some(25000.0),
        );
        let buffer = upconverter.make_input_buffer();
        Self {
            buffer: Some(buffer),
            direct_buffer: Vec::new(),
            buffer_i: 0,
            upconverter: Some(upconverter),
            modulator: modulator::Modulator::new(mode),
        }
    }

    fn new_direct(mode: modulator::Mode) -> Self {
        Self {
            upconverter: None,
            modulator: modulator::Modulator::new(mode),
            buffer: None,
            direct_buffer: Vec::new(),
            buffer_i: 0,
        }
    }

    fn process(&mut self, fcfb: &mut fcfb::SynthesisOutputProcessor, block_count: fcfb::BlockCount, tx_slot: &TxSlotBits) -> bool {
        let buffer = self.buffer.as_mut().unwrap();
        let buf = buffer.buffer_in();
        while self.buffer_i < buf.len() {
            // TODO: include delay of FCFB in sample count
            match self.modulator.sample(
                block_count as SampleCount * buf.len() as SampleCount + self.buffer_i as SampleCount,
                tx_slot,
            ) {
                Ok(sample) => {
                    buf[self.buffer_i] = sample;
                    self.buffer_i += 1;
                }
                Err(modulator::Error::NeedMoreData) => {
                    return false;
                }
            }
        }
        fcfb.add(self.upconverter.as_mut().unwrap().process(buffer.buffer(), block_count));

        let _ = buffer.prepare_for_new_samples();
        self.buffer_i = 0;
        true
    }

    fn process_direct(
        &mut self,
        block_count: fcfb::BlockCount,
        block_samples: usize,
        tx_slot: &TxSlotBits,
        output: &mut [ComplexSample],
    ) -> bool {
        if self.direct_buffer.len() != block_samples {
            self.direct_buffer.clear();
            self.direct_buffer.resize(block_samples, ComplexSample::ZERO);
            self.buffer_i = 0;
        }

        while self.buffer_i < block_samples {
            match self.modulator.sample(
                block_count as SampleCount * block_samples as SampleCount + self.buffer_i as SampleCount,
                tx_slot,
            ) {
                Ok(sample) => {
                    self.direct_buffer[self.buffer_i] = sample;
                    self.buffer_i += 1;
                }
                Err(modulator::Error::NeedMoreData) => {
                    return false;
                }
            }
        }
        for (out, sample) in output.iter_mut().zip(self.direct_buffer.iter()) {
            *out += *sample;
        }
        self.buffer_i = 0;
        true
    }

    fn align_direct_reference(&mut self, slot_time: TdmaTime, slot_begin_sample: SampleCount) {
        self.modulator.align_reference_time(slot_time, slot_begin_sample);
        self.buffer_i = 0;
    }
}

struct MonitorDlUlPair {
    dl: DemodulatorChannel,
    ul: Option<DemodulatorChannel>,
}
