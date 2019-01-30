use crate::spc;
use bit_field::BitField;
use embedded_types;
use embedded_types::can::{BaseID, ExtendedDataFrame, ExtendedID};
pub use embedded_types::can::{CanFrame, ID};
use embedded_types::io::Error as IOError;
use s32k144;
use s32k144::can0;

const TX_MAILBOXES: usize = 8;
const RX_MAILBOXES: usize = 8;

pub struct Can<'a> {
    register_block: &'a s32k144::can0::RegisterBlock,
    _spc: &'a spc::Spc<'a>,
}

impl<'a> Can<'a> {
    pub fn init_fd(
        can: &'a s32k144::can0::RegisterBlock,
        spc: &'a spc::Spc<'a>,
        settings: &CanSettings,
    ) -> Result<Self, CanError> {
        // XXX: When the CAN FD feature is enabled, do not use the PRESDIV, RJW, PSEG1, PSEG2, and
        // PROPSEG fields of the CTRL1 register for CAN bit timing. Instead use the CBT register's
        // EPRESDIV, ERJW, EPSEG1, EPSEG2, and EPROPSEG fields.

        return Ok(Can {
            register_block: can,
            _spc: spc,
        });
    }

    pub fn init(
        can: &'a s32k144::can0::RegisterBlock,
        spc: &'a spc::Spc<'a>,
        settings: &CanSettings,
    ) -> Result<Self, CanError> {
        let source_frequency = match settings.clock_source {
            ClockSource::Sys => spc.core_freq(),
            ClockSource::Soscdiv2 => spc.soscdiv2_freq().ok_or(CanError::ClockSourceDisabled)?,
        };

        if source_frequency % settings.can_frequency != 0
            || source_frequency < settings.can_frequency * 5
        {
            return Err(CanError::SettingsError);
        }

        // TODO: check if message_buffer_settings are longer than max MB available

        let presdiv = (source_frequency / settings.can_frequency) / 25;
        let tqs = (source_frequency / (presdiv + 1)) / settings.can_frequency;

        // Table 50-26 in datasheet, can standard compliant settings
        let (pseg2, rjw) = if tqs >= 8 && tqs < 10 {
            (1, 1)
        } else if tqs >= 10 && tqs < 15 {
            (3, 2)
        } else if tqs >= 15 && tqs < 20 {
            (6, 2)
        } else if tqs >= 20 && tqs < 26 {
            (7, 3)
        } else {
            panic!("there should be between 8 and 25 tqs in an bit");
        };

        let pseg1 = ((tqs - (pseg2 + 1)) / 2) - 1;
        let propseg = tqs - (pseg2 + 1) - (pseg1 + 1) - 2;

        reset(can);

        // first set clock source
        can.ctrl1
            .modify(|_, w| w.clksrc().bit(settings.clock_source == ClockSource::Sys));

        enable(can);
        enter_freeze(can);

        #[rustfmt::skip]
        can.mcr.modify(|_, w| {
            w.rfen().bit(settings.rx_fifo)
                .srxdis().bit(!settings.self_reception)
                .irmq().bit(settings.individual_masking)
                .aen().bit(true)
                .dma().bit(settings.rx_fifo && false);
            unsafe { w.maxmb().bits((RX_MAILBOXES + TX_MAILBOXES) as u8 - 1) };
            w
        });

        #[rustfmt::skip]
        can.ctrl1.modify(|_, w| unsafe {
            // Prescaler division factor
            w.presdiv().bits(presdiv as u8)
                // Phase segment 1
                .pseg1().bits(pseg1 as u8)
                // Phase segment 2
                .pseg2().bits(pseg2 as u8)
                // Propagation segment
                .propseg().bits(propseg as u8)
                // Resync jump width
                .rjw().bits(rjw as u8)
                // Loop back mode
                .lpb().bit(settings.loopback_mode)
        });

        if settings.loopback_mode {
            can.fdctrl.modify(|_, w| w.tdcen()._0());
        }

        // set filter mask to accept all
        // TODO: Make better logic for setting filters
        can.rxmgmask.write(|w| unsafe { w.bits(0) });

        /*
        • Initialize the Message Buffers
        • The Control and Status word of all Message Buffers must be initialized
        • If Rx FIFO was enabled, the ID filter table must be initialized
        • Other entries in each Message Buffer should be initialized as required
         */

        let filter_frame = CanFrame::from(ExtendedDataFrame::new(ExtendedID::new(0))); // TODO: set filters better then on extended data frames

        for mb in 0..TX_MAILBOXES {
            inactivate_mailbox(can, mb as usize);
            write_mailbox(
                can,
                &MailboxHeader::default_transmit(),
                &filter_frame,
                mb as usize,
            )
            .unwrap();
        }

        for mb in TX_MAILBOXES..(TX_MAILBOXES + RX_MAILBOXES) {
            inactivate_mailbox(can, mb as usize);
            write_mailbox(
                can,
                &MailboxHeader::default_receive(),
                &filter_frame,
                mb as usize,
            )
            .unwrap();
        }

        // clear all interrupt flags so data wont dangle
        can.iflag1.write(|w| unsafe { w.bits(0xffff_ffff) });

        leave_freeze(can);

        // Make some acceptance test to see if the configurations have been applied

        return Ok(Can {
            register_block: can,
            _spc: spc,
        });
    }

    /// Does not attempt to swap frames if all mailboxes are full, not suitable for frames
    /// that need to live up to some timing requirements, as priority inversion might be unavoidable.
    pub fn transmit_quick(&self, frame: &CanFrame) -> Result<(), IOError> {
        let mut header = MailboxHeader::default_transmit();
        header.code = MessageBufferCode::Transmit(TransmitBufferState::DataRemote);

        for i in 0..TX_MAILBOXES {
            if read_mailbox_code(self.register_block, i)
                == MessageBufferCode::Transmit(TransmitBufferState::Inactive)
            {
                match write_mailbox(self.register_block, &header, frame, i) {
                    Ok(()) => return Ok(()),
                    Err(_) => (),
                }
            }
        }
        Err(IOError::BufferExhausted)
    }

    /// If there are no free Mailboxes, the frame with lowest priority will be aborted and returned upon success
    pub fn transmit(&self, frame: &CanFrame) -> Result<Option<CanFrame>, IOError> {
        let mut highest_id = 0;
        let mut mailbox_number = usize::max_value();

        let mut transmit_header = MailboxHeader::default_transmit();
        transmit_header.code = MessageBufferCode::Transmit(TransmitBufferState::DataRemote);

        for i in 0..TX_MAILBOXES {
            let (header, old_frame) = read_mailbox(self.register_block, i);
            match header.code {
                MessageBufferCode::Transmit(TransmitBufferState::Inactive) => {
                    write_mailbox(self.register_block, &transmit_header, frame, i).unwrap();
                    return Ok(None);
                }
                MessageBufferCode::Transmit(TransmitBufferState::DataRemote) => {
                    if u32::from(old_frame.id()) > highest_id {
                        highest_id = u32::from(frame.id());
                        mailbox_number = i;
                    }
                }
                _ => unreachable!(),
            }
        }

        if highest_id > u32::from(frame.id()) {
            let aborted_frame = abort_mailbox(self.register_block, mailbox_number);
            write_mailbox(self.register_block, &transmit_header, frame, mailbox_number).unwrap();
            Ok(aborted_frame)
        } else {
            Err(IOError::BufferExhausted)
        }
    }

    pub fn receive(&self) -> Result<CanFrame, IOError> {
        for i in TX_MAILBOXES..(TX_MAILBOXES + RX_MAILBOXES) {
            let new_message = self.register_block.iflag1.read().bits().get_bit(i);
            if new_message {
                let (_header, frame) = read_mailbox(self.register_block, i);
                return Ok(frame);
            }
        }
        Err(IOError::BufferExhausted)
    }
}

pub struct CanSettings {
    /// When asserted, this bit enables the generation of the TWRNINT and RWRNINT flags in the Error and
    /// Status Register 1 (ESR1). If WRNEN is negated, the TWRNINT and RWRNINT flags will always be zero,
    /// independent of the values of the error counters, and no warning interrupt will ever be generated. This bit
    /// can be written in Freeze mode only because it is blocked by hardware in other modes.
    pub warning_interrupt: bool,

    /// This bit defines whether FlexCAN is allowed to receive frames transmitted by itself. If this bit is asserted,
    /// frames transmitted by the module will not be stored in any MB, regardless if the MB is programmed with
    /// an ID that matches the transmitted frame, and no interrupt flag or interrupt signal will be generated due to
    /// the frame reception.
    pub self_reception: bool,

    /// This bit controls whether the Rx FIFO feature is enabled or not. When RFEN is set, MBs 0 to
    /// 5 cannotbe used for normal reception and transmission because the corresponding memory
    /// region (0x80-0xDC) is used by the FIFO engine as well as additional MBs (up to 32,
    /// depending on `CTRL2[RFFN]` setting) which are used as Rx FIFO ID filter table elements.
    /// `RFEN` also impacts the definition of the minimum numbers of peripheral clocks per CAN bit
    /// as described in Table 55-31. This bit can be written in Freeze mode only, because it is
    /// blocked by hardware in other modes.
    ///
    /// This bit cannot be set when CAN FG operation is enabled (see `FDEN` bit).
    pub rx_fifo: bool,

    /// This bit indicates whether Rx matching process will be based either on individual masking and queue or
    /// on masking scheme with CAN_RXMGMASK, CAN_RX14MASK, CAN_RX15MASK and
    /// CAN_RXFGMASK.
    pub individual_masking: bool,

    /// This bit configures FlexCAN to operate in Loop-Back mode. In this mode, FlexCAN performs an internal
    /// loop back that can be used for self test operation. The bit stream output of the transmitter is fed back
    /// internally to the receiver input. The Rx CAN input pin is ignored and the Tx CAN output goes to the
    /// recessive state (logic 1). FlexCAN behaves as it normally does when transmitting, and treats its own
    /// transmitted message as a message received from a remote node.
    pub loopback_mode: bool,

    /// This bit selects the clock source to the CAN Protocol Engine (PE) to be either the peripheral clock or the
    /// oscillator clock. The selected clock is the one fed to the prescaler to generate the Serial Clock (Sclock). In
    /// order to guarantee reliable operation
    pub clock_source: ClockSource,

    pub can_frequency: u32,
}

impl Default for CanSettings {
    fn default() -> Self {
        CanSettings {
            warning_interrupt: false,
            self_reception: true,
            rx_fifo: false,
            individual_masking: false,
            loopback_mode: false,
            can_frequency: 1000000,
            clock_source: ClockSource::Soscdiv2,
        }
    }
}

/// This bit selects the clock source to the CAN Protocol Engine (PE) to be either the peripheral clock or the
/// oscillator clock.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ClockSource {
    /// The CAN engine clock source is the oscillator clock. Under this condition, the oscillator clock
    /// frequency must be lower than the bus clock.
    Soscdiv2,

    /// The CAN engine clock source is the peripheral clock.
    Sys,
}

impl Default for ClockSource {
    fn default() -> Self {
        ClockSource::Soscdiv2
    }
}

#[derive(Clone, PartialEq)]
enum MessageBufferCode {
    Receive(ReceiveBufferCode),
    Transmit(TransmitBufferState),
}

#[derive(Clone, PartialEq)]
struct ReceiveBufferCode {
    pub state: ReceiveBufferState,
    /// FlexCAN is updating the contents of the MB, the CPU must not access the MB
    pub busy: bool,
}

#[derive(Clone, PartialEq)]
enum ReceiveBufferState {
    /// MB is not active
    Inactive,

    /// MB is active and empty
    Empty,

    /// MB is full
    Full,

    /// MV is beeing overwritten into a full buffer
    Overrun,

    /// A frame was configured to recongnize a Remote Reuqest Frame and transmit a response Frame in return
    Ranswer,
}

#[derive(Clone, PartialEq)]
enum TransmitBufferState {
    /// MB is not active
    Inactive,

    /// MB is aborted
    Abort,

    /// MB is a tx data frame or tx RTR frame depending on RTR bit
    DataRemote,

    /// MV is a Tx response frame from an incoming RTR frame
    Tanswer,
}

impl From<MessageBufferCode> for u8 {
    fn from(code: MessageBufferCode) -> u8 {
        match code {
            MessageBufferCode::Receive(ref r) => match r.state {
                ReceiveBufferState::Inactive => {
                    0u8.set_bit(0, r.busy).set_bits(1..4, 0b000).get_bits(0..4)
                }
                ReceiveBufferState::Empty => {
                    0u8.set_bit(0, r.busy).set_bits(1..4, 0b010).get_bits(0..4)
                }
                ReceiveBufferState::Full => {
                    0u8.set_bit(0, r.busy).set_bits(1..4, 0b001).get_bits(0..4)
                }
                ReceiveBufferState::Overrun => {
                    0u8.set_bit(0, r.busy).set_bits(1..4, 0b011).get_bits(0..4)
                }
                ReceiveBufferState::Ranswer => {
                    0u8.set_bit(0, r.busy).set_bits(1..4, 0b101).get_bits(0..4)
                }
            },
            MessageBufferCode::Transmit(ref t) => match *t {
                TransmitBufferState::Inactive => 0b1000,
                TransmitBufferState::Abort => 0b1001,
                TransmitBufferState::DataRemote => 0b1100,
                TransmitBufferState::Tanswer => 0b1110,
            },
        }
    }
}

impl MessageBufferCode {
    fn decode(code: u8) -> Result<Self, ()> {
        match code {
            0b1000 => Ok(MessageBufferCode::Transmit(TransmitBufferState::Inactive)),
            0b1001 => Ok(MessageBufferCode::Transmit(TransmitBufferState::Abort)),
            0b1100 => Ok(MessageBufferCode::Transmit(TransmitBufferState::DataRemote)),
            0b1110 => Ok(MessageBufferCode::Transmit(TransmitBufferState::Tanswer)),
            0b0000 => Ok(MessageBufferCode::Receive(ReceiveBufferCode {
                state: ReceiveBufferState::Inactive,
                busy: false,
            })),
            0b0001 => Ok(MessageBufferCode::Receive(ReceiveBufferCode {
                state: ReceiveBufferState::Inactive,
                busy: true,
            })),
            0b0100 => Ok(MessageBufferCode::Receive(ReceiveBufferCode {
                state: ReceiveBufferState::Empty,
                busy: false,
            })),
            0b0101 => Ok(MessageBufferCode::Receive(ReceiveBufferCode {
                state: ReceiveBufferState::Empty,
                busy: true,
            })),
            0b0010 => Ok(MessageBufferCode::Receive(ReceiveBufferCode {
                state: ReceiveBufferState::Full,
                busy: false,
            })),
            0b0011 => Ok(MessageBufferCode::Receive(ReceiveBufferCode {
                state: ReceiveBufferState::Full,
                busy: true,
            })),
            0b0110 => Ok(MessageBufferCode::Receive(ReceiveBufferCode {
                state: ReceiveBufferState::Overrun,
                busy: false,
            })),
            0b0111 => Ok(MessageBufferCode::Receive(ReceiveBufferCode {
                state: ReceiveBufferState::Overrun,
                busy: true,
            })),
            0b1010 => Ok(MessageBufferCode::Receive(ReceiveBufferCode {
                state: ReceiveBufferState::Ranswer,
                busy: false,
            })),
            0b1011 => Ok(MessageBufferCode::Receive(ReceiveBufferCode {
                state: ReceiveBufferState::Ranswer,
                busy: true,
            })),
            _ => Err(()),
        }
    }
}

struct MailboxHeader {
    /// This bit indicates if the transmitting node is error active or error passive.
    pub error_state_indicator: bool,

    /// This 4-bit field can be accessed (read or write) by the CPU and by the FlexCAN module
    /// itself, as part of the message buffer matching and arbitration process.
    pub code: MessageBufferCode,

    /// This 16-bit field is a copy of the Free-Running Timer, captured for Tx and Rx frames at
    /// the time when the beginning of the Identifier field appears on the CAN bus
    pub time_stamp: u16,

    /// This 3-bit field is used only when LPRIO_EN bit is set in CAN_MCR, and it only makes
    /// sense for Tx mailboxes. These bits are not transmitted. They are appended to the regular
    /// ID to define the transmission priority.
    pub priority: u8,
}

impl MailboxHeader {
    pub fn default_transmit() -> Self {
        MailboxHeader {
            error_state_indicator: false,
            code: MessageBufferCode::Transmit(TransmitBufferState::Inactive),
            time_stamp: 0,
            priority: 0,
        }
    }

    pub fn default_receive() -> Self {
        MailboxHeader {
            error_state_indicator: false,
            code: MessageBufferCode::Receive(ReceiveBufferCode {
                state: ReceiveBufferState::Empty,
                busy: false,
            }),
            time_stamp: 0,
            priority: 0,
        }
    }
}

fn enable(can: &can0::RegisterBlock) {
    can.mcr.modify(|_, w| w.mdis()._0());

    // Wait until the module has been enabled
    while can.mcr.read().lpmack().is_1() {}
}

fn disable(can: &can0::RegisterBlock) {
    can.mcr.modify(|_, w| w.mdis()._1());

    // Wait until module has entered low power mode. Effectively waits until all current
    // transmissions or reception processes have finished, and asserts that the module is disabled.
    while can.mcr.read().lpmack().is_0() {}
}

fn reset(can: &can0::RegisterBlock) {
    disable(can);

    // Set peripheral clock as clock source
    // TODO: remove this?
    can.ctrl1.modify(|_, w| w.clksrc()._1());

    // TODO: do we need this?
    enable(can);

    // Soft reset; resets FlexCAN-internal state machines and some memory-mapped registers.
    can.mcr.modify(|_, w| w.softrst()._1());
    while can.mcr.read().softrst().is_1() {}

    disable(can);
}

fn enter_freeze(can: &can0::RegisterBlock) {
    can.mcr.modify(|_, w| w.frz()._1().halt()._1());
    // TODO: Check whether CAN_MCR[MDIS] (Module Disable) is set to 1. If it is, clear it to 0.
    // Done in `enable()`. Panic if it is 1 here?
    while can.mcr.read().frzack().is_0() {
        // TODO: panic if timeout reached (for now).
        //
        // TODO: poll until timeout (ref. p. 1638) is reached.
        // More steps needed if timeout reached.
    }
}

fn leave_freeze(can: &can0::RegisterBlock) {
    can.mcr.modify(|_, w| w.halt()._0().frz()._0());
    while can.mcr.read().frzack().is_1() {}
}

#[derive(Debug)]
pub enum CanError {
    FreezeModeError,
    ClockSourceDisabled,
    SettingsError,
    ConfigurationFailed,
    BusyMailboxWriteAttempted,
}

fn read_mailbox_code(can: &can0::RegisterBlock, mailbox: usize) -> MessageBufferCode {
    let start_adress = mailbox * 4;
    let code = MessageBufferCode::decode(
        can.embedded_ram[start_adress]
            .read()
            .bits()
            .get_bits(24..28) as u8,
    )
    .unwrap();
    // The read might have caused a lock, need to read the timer to unlock all mailboxes just in case.
    let _time = can.timer.read();
    code
}

fn abort_mailbox(can: &can0::RegisterBlock, mailbox: usize) -> Option<CanFrame> {
    // TODO: this function is untested, test it (it requires mcr.aen() bit set as well)
    let start_adress = mailbox * 4;
    if MessageBufferCode::decode(
        can.embedded_ram[start_adress]
            .read()
            .bits()
            .get_bits(24..28) as u8,
    ) == Ok(MessageBufferCode::Transmit(TransmitBufferState::DataRemote))
    {
        can.iflag1.write(|w| unsafe { w.bits(1 << mailbox) });
        can.embedded_ram[start_adress].write(|w| unsafe {
            w.bits(
                0u32.set_bits(
                    24..28,
                    u8::from(MessageBufferCode::Transmit(TransmitBufferState::Abort)) as u32,
                )
                .get_bits(0..32),
            )
        });
        while can.iflag1.read().bits() & (1 << mailbox) != 0 {}
        let (header, frame) = read_mailbox(can, mailbox);

        match header.code {
            MessageBufferCode::Transmit(TransmitBufferState::Abort) => Some(frame),
            MessageBufferCode::Transmit(TransmitBufferState::Inactive) => None,
            _ => unreachable!(),
        }
    } else {
        None
    }
}

/// Inactivates the mailbox as described in datasheet 50.5.7.2
///
/// Because the user is not able to synchronize the CODE field update with the FlexCAN
/// internal processes, an inactivation can have the following consequences:
///  - A frame in the bus that matches the filtering of the inactivated Rx Mailbox may be lost without notice, even if there are other Mailboxes with the same filter
///  - A frame containing the message within the inactivated Tx Mailbox may be transmitted without setting the respective IFLAG
fn inactivate_mailbox(can: &can0::RegisterBlock, mailbox: usize) {
    //TODO: consider clearing interrupt
    let start_adress = mailbox * 4;
    match MessageBufferCode::decode(
        can.embedded_ram[start_adress]
            .read()
            .bits()
            .get_bits(24..28) as u8,
    ) {
        Ok(MessageBufferCode::Transmit(_)) => can.embedded_ram[start_adress].write(|w| unsafe {
            w.bits(
                0u32.set_bits(
                    24..28,
                    u8::from(MessageBufferCode::Transmit(TransmitBufferState::Inactive)) as u32,
                )
                .get_bits(0..32),
            )
        }),
        Ok(MessageBufferCode::Receive(_)) => can.embedded_ram[start_adress].write(|w| unsafe {
            w.bits(
                0u32.set_bits(
                    24..28,
                    u8::from(MessageBufferCode::Receive(ReceiveBufferCode {
                        state: ReceiveBufferState::Inactive,
                        busy: false,
                    })) as u32,
                )
                .get_bits(0..32),
            )
        }),
        Err(_) => can.embedded_ram[start_adress].write(|w| unsafe { w.bits(0) }),
    }
}

/// Write to mailbox in such order that if Code is transfer active, a transfer will be initiated
///
/// This function will fail if the buffer is currently full, empty, waiting to transmit data, or contains a remote frame response.
/// If this is the case, a abort will need to occur first.
///
/// If a write is succseeded the interrupt flag will also be cleared. This is so the IRQ doesn't try to access outdated data.
fn write_mailbox(
    can: &can0::RegisterBlock,
    header: &MailboxHeader,
    frame: &CanFrame,
    mailbox: usize,
) -> Result<(), CanError> {
    let start_adress = mailbox * 4;

    // Check if the mailbox is ready for a write
    let current_code = can.embedded_ram[start_adress]
        .read()
        .bits()
        .get_bits(24..28) as u8;

    match MessageBufferCode::decode(current_code).unwrap() {
        MessageBufferCode::Transmit(TransmitBufferState::DataRemote) => {
            return Err(CanError::BusyMailboxWriteAttempted);
        }
        MessageBufferCode::Transmit(TransmitBufferState::Tanswer) => {
            return Err(CanError::BusyMailboxWriteAttempted);
        }
        MessageBufferCode::Receive(ReceiveBufferCode {
            state: ReceiveBufferState::Empty,
            busy: _,
        }) => return Err(CanError::BusyMailboxWriteAttempted),
        MessageBufferCode::Receive(ReceiveBufferCode {
            state: ReceiveBufferState::Overrun,
            busy: _,
        }) => return Err(CanError::BusyMailboxWriteAttempted),
        MessageBufferCode::Receive(ReceiveBufferCode {
            state: ReceiveBufferState::Full,
            busy: _,
        }) => return Err(CanError::BusyMailboxWriteAttempted),
        MessageBufferCode::Receive(ReceiveBufferCode {
            state: ReceiveBufferState::Ranswer,
            busy: _,
        }) => return Err(CanError::BusyMailboxWriteAttempted),
        _ => (),
    }

    // Clear the interrupt flag so it's clear that this transmission have not finished
    can.iflag1.write(|w| unsafe { w.bits(1 << mailbox) });

    // 3. Write the ID word and priority
    let extended_id = match frame.id() {
        ID::BaseID(_) => false,
        ID::ExtendedID(_) => true,
    };

    if extended_id {
        unsafe {
            can.embedded_ram[start_adress + 1].modify(|_, w| {
                w.bits(
                    0u32.set_bits(0..29, frame.id().into())
                        .set_bits(29..32, header.priority as u32)
                        .get_bits(0..32),
                )
            })
        };
    } else {
        unsafe {
            can.embedded_ram[start_adress + 1].modify(|_, w| {
                w.bits(
                    0u32.set_bits(18..29, frame.id().into())
                        .set_bits(29..32, header.priority as u32)
                        .get_bits(0..32),
                )
            })
        };
    }

    // 4. Write the data bytes.
    let data_length = if let CanFrame::DataFrame(data_frame) = *frame {
        for index in 0..data_frame.data().len() as usize {
            can.embedded_ram[start_adress + 2 + index / 4].modify(|r, w| {
                let mut bitmask = r.bits();
                bitmask.set_bits(
                    32 - (8 * (1 + index % 4))..(32 - 8 * (index % 4)),
                    data_frame.data()[index] as u32,
                );
                unsafe { w.bits(bitmask) }
            });
        }
        data_frame.data().len()
    } else {
        0
    };

    let remote_frame = match *frame {
        CanFrame::DataFrame(_) => false,
        CanFrame::RemoteFrame(_) => true,
    };

    // 5. Write the DLC, Control, and CODE fields of the Control and Status word to activate the MB
    can.embedded_ram[start_adress + 0].write(|w| unsafe {
        w.bits(
            0u32.set_bit(31, false) // not CAN-FD frame
                .set_bit(29, header.error_state_indicator)
                .set_bits(24..28, u8::from(header.code.clone()) as u32)
                .set_bit(22, true) // SRR needs to be 1 to adhere to can specs
                .set_bit(21, extended_id)
                .set_bit(20, remote_frame)
                .set_bits(16..20, data_length as u32)
                .set_bits(0..15, header.time_stamp as u32)
                .get_bits(0..32),
        )
    });

    Ok(())
}

fn read_mailbox(can: &can0::RegisterBlock, mailbox: usize) -> (MailboxHeader, CanFrame) {
    let start_adress = mailbox * 4;

    // TODO: Check that mailbox is within valid range and return error (panic?) if not

    // 1. Read control and Status word
    let mut cs = can.embedded_ram[start_adress].read().bits();

    // 2. read untill mail box no longer busy
    while let MessageBufferCode::Receive(code) =
        MessageBufferCode::decode(cs.get_bits(24..28) as u8).unwrap()
    {
        if code.busy {
            cs = can.embedded_ram[start_adress].read().bits();
        } else {
            break;
        }
    }

    // 3. Read contents of the mailbox
    let extended_id = cs.get_bit(21);
    let id = if extended_id {
        ID::ExtendedID(ExtendedID::new(
            can.embedded_ram[start_adress + 1]
                .read()
                .bits()
                .get_bits(0..28),
        ))
    } else {
        ID::BaseID(BaseID::new(
            can.embedded_ram[start_adress + 1]
                .read()
                .bits()
                .get_bits(18..28) as u16,
        ))
    };
    let dlc = cs.get_bits(16..20) as usize;

    let remote_frame = cs.get_bit(20);

    let frame = if remote_frame {
        CanFrame::from(embedded_types::can::RemoteFrame::new(id))
    } else {
        let mut frame = embedded_types::can::DataFrame::new(id);
        frame.set_data_length(dlc);
        for i in 0..dlc {
            frame.data_as_mut()[i] = can.embedded_ram[start_adress + 2 + i / 4]
                .read()
                .bits()
                .get_bits((32 - 8 * (1 + i % 4))..32 - 8 * (i % 4))
                as u8;
        }
        CanFrame::from(frame)
    };

    let priority = can.embedded_ram[start_adress + 1]
        .read()
        .bits()
        .get_bits(29..32);

    let header = MailboxHeader {
        error_state_indicator: cs.get_bit(29),
        code: MessageBufferCode::decode(cs.get_bits(24..28) as u8).unwrap(),
        time_stamp: cs.get_bits(0..15) as u16,
        priority: priority as u8,
    };

    // 4. Ack proper flag
    can.iflag1.write(|w| unsafe { w.bits(1 << mailbox) });

    // 6. Read Free running timer to unlock mailbox
    let _time = can.timer.read();

    (header, frame.into())
}
