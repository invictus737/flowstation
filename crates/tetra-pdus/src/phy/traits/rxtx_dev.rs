use tetra_core::TdmaTime;
use tetra_core::TrainingSequence;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum RxTxDirection {
    Rx,
    Tx,
    Device,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SoapyStreamErrorCode {
    Timeout,
    StreamError,
    Corruption,
    Overflow,
    NotSupported,
    TimeError,
    Underflow,
    Other,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum RxTxDevError {
    RxEndOfData,
    RxReadError,
    SoapyStreamError {
        direction: RxTxDirection,
        code: SoapyStreamErrorCode,
        operation: &'static str,
        message: String,
    },
    LateTx {
        target_sample: i64,
        current_sample: i64,
        min_headroom_samples: i64,
        message: String,
    },
}

impl std::fmt::Display for RxTxDevError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RxEndOfData => write!(f, "RX end of data"),
            Self::RxReadError => write!(f, "RX read error"),
            Self::SoapyStreamError {
                direction,
                code,
                operation,
                message,
            } => write!(f, "SoapySDR {:?} {} failed with {:?}: {}", direction, operation, code, message),
            Self::LateTx {
                target_sample,
                current_sample,
                min_headroom_samples,
                message,
            } => write!(
                f,
                "late TX target_sample={} current_sample={} min_headroom_samples={}: {}",
                target_sample, current_sample, min_headroom_samples, message
            ),
        }
    }
}

impl std::error::Error for RxTxDevError {}

#[derive(Debug, Default, Clone, Copy)]
pub struct RxTiming {
    /// Hardware/SDR timestamp in nanoseconds for the first known RX sample.
    /// None means the PHY path has not propagated the real timestamp yet.
    pub time_ns: Option<i64>,
    /// Hardware/SDR sample counter for the first known RX sample.
    /// None means the PHY path has not propagated the real counter yet.
    pub sample_count: Option<i64>,
}

#[derive(Debug, Default)]
pub struct RxBurstBits<'a> {
    pub train_type: TrainingSequence,
    pub bits: &'a [u8],
    /// Received signal strength in dBFS (dB relative to ADC full-scale).
    /// 0.0 = full scale, negative = weaker signal. Not calibrated to dBm.
    pub rssi_dbfs: f32,
}

#[derive(Debug, Default)]
pub struct RxSlotBits<'a> {
    /// Number of slot received
    pub time: TdmaTime,
    /// Real RX timing metadata, when available from the SDR path.
    pub rx_timing: RxTiming,
    /// Burst received in full slot
    pub slot: RxBurstBits<'a>,
    /// Burst received in subslot 1
    pub subslot1: RxBurstBits<'a>,
    /// Burst received in subslot 2
    pub subslot2: RxBurstBits<'a>,
}

#[derive(Debug, Default)]
pub struct TxSlotBits<'a> {
    /// Number of slot to transmit
    pub time: TdmaTime,
    /// Burst to transmit in full slot
    pub slot: Option<&'a [u8]>,
    // /// Burst to transmit in subslot 1
    // pub subslot1: Option<&'a [u8]>,
    // /// Burst to transmit in subslot 2
    // pub subslot2: Option<&'a [u8]>,
}

/// Trait for RX/TX devices that work with full slots.
pub trait RxTxDev {
    fn rxtx_timeslot(&mut self, tx_slot: &[TxSlotBits]) -> Result<Vec<Option<RxSlotBits<'_>>>, RxTxDevError>;
}
