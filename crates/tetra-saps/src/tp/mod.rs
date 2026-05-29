use tetra_core::{BitBuffer, BurstType, PhyBlockNum, PhyBlockType, TdmaTime, TrainingSequence};

#[derive(Debug, Clone)]
pub struct TpUnitdataInd {
    /// RF-derived uplink TDMA time, when propagated by PHY.
    pub time: Option<TdmaTime>,
    pub train_type: TrainingSequence,
    pub burst_type: BurstType,
    pub block_type: PhyBlockType,
    /// Undefined for BBK. For all others: [ Block1 | Block2 | Both ]
    pub block_num: PhyBlockNum,
    pub block: BitBuffer,
    /// Received signal strength in dBFS. See RxBurstBits.rssi_dbfs.
    pub rssi_dbfs: f32,
    /// Hardware/SDR timestamp in nanoseconds for the RX path, when available.
    pub rx_time_ns: Option<i64>,
    /// Hardware/SDR sample counter for the RX path, when available.
    pub rx_sample_count: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct TpUnitdataReqSlot {
    pub train_type: TrainingSequence,
    pub burst_type: BurstType,
    pub bbk: Option<BitBuffer>,
    pub blk1: Option<BitBuffer>,
    pub blk2: Option<BitBuffer>,
}
