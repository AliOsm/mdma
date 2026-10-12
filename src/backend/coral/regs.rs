//! Registers of the Beagle chip that user space reaches through the `apex` driver:
//! the three 4 KiB pages at 0x40000 (tile configuration), 0x44000 (scalar core) and
//! 0x48000 (user HIB, the host interface block). Names and offsets are libedgetpu's
//! `beagle_csr_offsets.h`. The SCU, clock and thermal registers at 0x1a000 belong to
//! the kernel driver, which takes the chip out of reset on open.

pub const REGION_SIZE: u64 = 0x1000;
pub const REGIONS: [u64; 3] = [TILE_BASE, SCALAR_CORE_BASE, USER_HIB_BASE];
pub const TILE_BASE: u64 = 0x40000;
pub const SCALAR_CORE_BASE: u64 = 0x44000;
pub const USER_HIB_BASE: u64 = 0x48000;

// Tile units. Writes go to the tiles `tileconfig0` selects (broadcast while opening).
pub const DEEP_SLEEP: u64 = 0x40020;
pub const OP_RUN_CONTROL: u64 = 0x400c0;
pub const WIDE_TO_NARROW_RUN_CONTROL: u64 = 0x40110;
pub const NARROW_TO_WIDE_RUN_CONTROL: u64 = 0x40150;
pub const RING_BUS_CONSUMER0_RUN_CONTROL: u64 = 0x40190;
pub const RING_BUS_CONSUMER1_RUN_CONTROL: u64 = 0x401d0;
pub const RING_BUS_PRODUCER_RUN_CONTROL: u64 = 0x40210;
pub const MESH_BUS0_RUN_CONTROL: u64 = 0x40250;
pub const MESH_BUS1_RUN_CONTROL: u64 = 0x40298;
pub const MESH_BUS2_RUN_CONTROL: u64 = 0x402e0;
pub const MESH_BUS3_RUN_CONTROL: u64 = 0x40328;
pub const ERROR_TILE: u64 = 0x40370;

// Scalar core and its DMA units.
pub const SCALAR_CORE_RUN_CONTROL: u64 = 0x44018;
pub const EXECUTE_CONTROL: u64 = 0x44038;
pub const AV_DATA_POP_RUN_CONTROL: u64 = 0x44158;
pub const AV_DATA_POP_RUN_STATUS: u64 = 0x44168;
pub const PARAMETER_POP_RUN_CONTROL: u64 = 0x44198;
pub const PARAMETER_POP_RUN_STATUS: u64 = 0x441a8;
pub const INFEED_RUN_CONTROL: u64 = 0x441d8;
pub const INFEED_RUN_STATUS: u64 = 0x441e0;
pub const OUTFEED_RUN_CONTROL: u64 = 0x44218;
pub const OUTFEED_RUN_STATUS: u64 = 0x44220;
pub const SCALAR_CORE_RUN_STATUS: u64 = 0x44258;
pub const ERROR_SCALAR_CORE: u64 = 0x44260;

// User HIB: the instruction queue, interrupts, DMA pause and error reporting.
pub const INSTRUCTION_QUEUE_CONTROL: u64 = 0x48568;
pub const INSTRUCTION_QUEUE_STATUS: u64 = 0x48570;
pub const INSTRUCTION_QUEUE_DESCRIPTOR_SIZE: u64 = 0x48578;
pub const INSTRUCTION_QUEUE_MINIMUM_SIZE: u64 = 0x48580;
pub const INSTRUCTION_QUEUE_MAXIMUM_SIZE: u64 = 0x48588;
pub const INSTRUCTION_QUEUE_BASE: u64 = 0x48590;
pub const INSTRUCTION_QUEUE_STATUS_BLOCK_BASE: u64 = 0x48598;
pub const INSTRUCTION_QUEUE_SIZE: u64 = 0x485a0;
pub const INSTRUCTION_QUEUE_TAIL: u64 = 0x485a8;
pub const INSTRUCTION_QUEUE_FETCHED_HEAD: u64 = 0x485b0;
pub const INSTRUCTION_QUEUE_COMPLETED_HEAD: u64 = 0x485b8;
pub const INSTRUCTION_QUEUE_INT_CONTROL: u64 = 0x485c0;
pub const INSTRUCTION_QUEUE_INT_STATUS: u64 = 0x485c8;
pub const SC_HOST_INT_CONTROL: u64 = 0x486a0;
pub const SC_HOST_INT_STATUS: u64 = 0x486a8;
pub const TOP_LEVEL_INT_CONTROL: u64 = 0x486b0;
pub const TOP_LEVEL_INT_STATUS: u64 = 0x486b8;
pub const FATAL_ERR_INT_CONTROL: u64 = 0x486c0;
pub const FATAL_ERR_INT_STATUS: u64 = 0x486c8;
pub const SC_HOST_INT_COUNT: u64 = 0x486d0;
pub const DMA_PAUSE: u64 = 0x486d8;
pub const DMA_PAUSED: u64 = 0x486e0;
pub const STATUS_BLOCK_UPDATE: u64 = 0x486e8;
pub const HIB_ERROR_STATUS: u64 = 0x486f0;
pub const HIB_ERROR_MASK: u64 = 0x486f8;
pub const HIB_FIRST_ERROR_STATUS: u64 = 0x48700;
pub const HIB_FIRST_ERROR_TIMESTAMP: u64 = 0x48708;
pub const TILECONFIG0: u64 = 0x48788;
pub const TILECONFIG1: u64 = 0x48790;
pub const DMA_BURST_LIMITER: u64 = 0x487a8;

/// `tileconfig0` value that addresses every tile at once (7 bits).
pub const TILECONFIG_BROADCAST: u64 = 0x7f;
/// Instruction queue control: enable, and let the chip write the status block.
pub const QUEUE_CONTROL_ENABLE: u64 = 1;
pub const QUEUE_CONTROL_STATUS_BLOCK_WRITE: u64 = 1 << 2;
/// Scalar-core interrupts, one bit each in control and status.
pub const SC_HOST_INTERRUPTS: u64 = 4;

/// Values of the run-control registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum RunControl {
    Idle = 0,
    Run = 1,
    Halt = 2,
    SingleStep = 3,
}

/// Every run-control register, scalar core first, then the tiles.
pub const SCALAR_CORE_RUN_CONTROLS: [u64; 5] =
    [SCALAR_CORE_RUN_CONTROL, AV_DATA_POP_RUN_CONTROL, PARAMETER_POP_RUN_CONTROL, INFEED_RUN_CONTROL, OUTFEED_RUN_CONTROL];
pub const TILE_RUN_CONTROLS: [u64; 10] = [
    OP_RUN_CONTROL, NARROW_TO_WIDE_RUN_CONTROL, WIDE_TO_NARROW_RUN_CONTROL, MESH_BUS0_RUN_CONTROL, MESH_BUS1_RUN_CONTROL,
    MESH_BUS2_RUN_CONTROL, MESH_BUS3_RUN_CONTROL, RING_BUS_CONSUMER0_RUN_CONTROL, RING_BUS_CONSUMER1_RUN_CONTROL,
    RING_BUS_PRODUCER_RUN_CONTROL,
];

/// The 16-bit counter of scalar-core interrupt `id` in `SC_HOST_INT_COUNT`.
pub fn sc_host_int_count(raw: u64, id: u64) -> u16 { (raw >> (16 * id)) as u16 }

/// The bits of `HIB_ERROR_STATUS`, as libedgetpu names them.
pub const HIB_ERROR_BITS: [&str; 15] = [
    "inbound_page_fault", "extended_page_fault", "csr_parity_error", "axi_slave_b_error", "axi_slave_r_error",
    "instruction_queue_bad_configuration", "input_actv_queue_bad_configuration", "param_queue_bad_configuration",
    "output_actv_queue_bad_configuration", "instruction_queue_invalid", "input_actv_queue_invalid", "param_queue_invalid",
    "output_actv_queue_invalid", "length_0_dma", "virt_table_rdata_uncorr",
];

pub fn hib_error_names(status: u64) -> String {
    let names: Vec<&str> = HIB_ERROR_BITS.iter().enumerate().filter(|(bit, _)| status & (1 << bit) != 0).map(|(_, name)| *name).collect();
    let unknown = status >> HIB_ERROR_BITS.len();
    match (names.is_empty(), unknown) {
        (true, 0) => "none".into(),
        (_, 0) => names.join(", "),
        (true, _) => format!("unknown bits 0x{unknown:x}"),
        (false, _) => format!("{}, unknown bits 0x{unknown:x}", names.join(", ")),
    }
}
