//! TWIM Driver for the SAM4L
//!
//! The implementation, especially of repeated starts, is quite sensitive to the
//! ordering of operations (e.g. setup DMA, then set command register, then next
//! command register, then enable, then start the DMA transfer). The placement
//! of writes to interrupt enable/disable registers is also significant, but not
//! refactored in such a way that's very logical right now.
//!
//! The point is that until this changes, and this notice is taken away: IF YOU
//! CHANGE THIS DRIVER, TEST RIGOROUSLY!!!
//!


use core::cell::Cell;
use core::mem;
use dma::{DMAChannel, DMAClient, DMAPeripheral};
use helpers::*;
use kernel::common::take_cell::TakeCell;
use kernel::common::volatile_cell::VolatileCell;

use kernel::hil;
use nvic;
use pm;

// Listing of all registers related to the TWIM peripheral.
// Section 27.9 of the datasheet
#[repr(C, packed)]
#[allow(dead_code)]
struct Registers {
    control: usize,
    clock_waveform_generator: usize,
    smbus_timing: usize,
    command: usize,
    next_command: usize,
    receive_holding: usize,
    transmit_holding: usize,
    status: usize,
    interrupt_enable: usize,
    interrupt_disable: usize,
    interrupt_mask: usize,
    status_clear: usize,
    parameter: usize,
    version: usize,
    hsmode_clock_waveform_generator: usize,
    slew_rate: usize,
    hsmod_slew_rate: usize,
}

// Listing of all registers related to the TWIS peripheral.
// Section 28.9 of the datasheet
#[repr(C, packed)]
#[allow(dead_code)]
struct TWISRegisters {
    control: VolatileCell<u32>,
    nbytes: VolatileCell<u32>,
    timing: VolatileCell<u32>,
    receive_holding: VolatileCell<u32>,
    transmit_holding: VolatileCell<u32>,
    packet_error_check: VolatileCell<u32>,
    status: VolatileCell<u32>,
    interrupt_enable: VolatileCell<u32>,
    interrupt_disable: VolatileCell<u32>,
    interrupt_mask: VolatileCell<u32>,
    status_clear: VolatileCell<u32>,
    parameter: VolatileCell<u32>,
    version: VolatileCell<u32>,
    hsmode_timing: VolatileCell<u32>,
    slew_rate: VolatileCell<u32>,
    hsmod_slew_rate: VolatileCell<u32>,
}

// The addresses in memory (7.1 of manual) of the TWIM peripherals
const I2C_BASE_ADDRS: [*mut Registers; 4] = [0x40018000 as *mut Registers,
                                             0x4001C000 as *mut Registers,
                                             0x40078000 as *mut Registers,
                                             0x4007C000 as *mut Registers];

// The addresses in memory (7.1 of manual) of the TWIM peripherals
const I2C_SLAVE_BASE_ADDRS: [*mut TWISRegisters; 2] = [0x40018400 as *mut TWISRegisters,
                                                       0x4001C400 as *mut TWISRegisters];

// There are four TWIM (two wire master interface) peripherals on the SAM4L.
// These likely won't all be used for I2C, but we let the platform decide
// which one to use.
#[derive(Clone,Copy)]
pub enum Location {
    I2C00, // TWIMS0
    I2C01, // TWIMS1
    I2C02, // TWIM2
    I2C03, // TWIM3
}

// Three main I2C speeds
#[derive(Clone,Copy)]
pub enum Speed {
    Standard100k,
    Fast400k,
    FastPlus1M,
}

// This represents an abstraction of the peripheral hardware.
pub struct I2CHw {
    registers: *mut Registers, // Pointer to the I2C registers in memory
    slave_registers: Option<*mut TWISRegisters>, // Pointer to the I2C TWIS registers in memory
    master_clock: pm::Clock,
    slave_clock: Option<pm::Clock>,
    dma: TakeCell<&'static DMAChannel>,
    dma_pids: (DMAPeripheral, DMAPeripheral),
    nvic: nvic::NvicIdx,
    slave_nvic: Option<nvic::NvicIdx>,
    master_client: TakeCell<&'static hil::i2c::I2CHwMasterClient>,
    slave_client: TakeCell<&'static hil::i2c::I2CHwSlaveClient>,
    on_deck: TakeCell<(DMAPeripheral, usize)>,

    slave_enabled: Cell<bool>,
    my_slave_address: Cell<u8>,
    slave_read_buffer: TakeCell<&'static mut [u8]>,
    slave_read_buffer_len: Cell<u8>,
    slave_read_buffer_index: Cell<u8>,
    slave_write_buffer: TakeCell<&'static mut [u8]>,
    slave_write_buffer_len: Cell<u8>,
    slave_write_buffer_index: Cell<u8>,
}

pub static mut I2C0: I2CHw = I2CHw::new(I2C_BASE_ADDRS[0],
                                        Some(I2C_SLAVE_BASE_ADDRS[0]),
                                        pm::Clock::PBA(pm::PBAClock::TWIM0),
                                        Some(pm::Clock::PBA(pm::PBAClock::TWIS0)),
                                        nvic::NvicIdx::TWIM0,
                                        Some(nvic::NvicIdx::TWIS0),
                                        DMAPeripheral::TWIM0_RX,
                                        DMAPeripheral::TWIM0_TX);
pub static mut I2C1: I2CHw = I2CHw::new(I2C_BASE_ADDRS[1],
                                        Some(I2C_SLAVE_BASE_ADDRS[1]),
                                        pm::Clock::PBA(pm::PBAClock::TWIM1),
                                        Some(pm::Clock::PBA(pm::PBAClock::TWIS1)),
                                        nvic::NvicIdx::TWIM1,
                                        Some(nvic::NvicIdx::TWIS1),
                                        DMAPeripheral::TWIM1_RX,
                                        DMAPeripheral::TWIM1_TX);
pub static mut I2C2: I2CHw = I2CHw::new(I2C_BASE_ADDRS[2],
                                        None,
                                        pm::Clock::PBA(pm::PBAClock::TWIM2),
                                        None,
                                        nvic::NvicIdx::TWIM2,
                                        None,
                                        DMAPeripheral::TWIM2_RX,
                                        DMAPeripheral::TWIM2_TX);
pub static mut I2C3: I2CHw = I2CHw::new(I2C_BASE_ADDRS[3],
                                        None,
                                        pm::Clock::PBA(pm::PBAClock::TWIM3),
                                        None,
                                        nvic::NvicIdx::TWIM3,
                                        None,
                                        DMAPeripheral::TWIM3_RX,
                                        DMAPeripheral::TWIM3_TX);

pub const START: usize = 1 << 13;
pub const STOP: usize = 1 << 14;
pub const ACKLAST: usize = 1 << 24;

// Need to implement the `new` function on the I2C device as a constructor.
// This gets called from the device tree.
impl I2CHw {
    const fn new(base_addr: *mut Registers,
                 slave_base_addr: Option<*mut TWISRegisters>,
                 master_clock: pm::Clock,
                 slave_clock: Option<pm::Clock>,
                 nvic: nvic::NvicIdx,
                 slave_nvic: Option<nvic::NvicIdx>,
                 dma_rx: DMAPeripheral,
                 dma_tx: DMAPeripheral)
                 -> I2CHw {
        I2CHw {
            registers: base_addr as *mut Registers,
            slave_registers: slave_base_addr,
            master_clock: master_clock,
            slave_clock: slave_clock,
            dma: TakeCell::empty(),
            dma_pids: (dma_rx, dma_tx),
            nvic: nvic,
            slave_nvic: slave_nvic,
            master_client: TakeCell::empty(),
            slave_client: TakeCell::empty(),
            on_deck: TakeCell::empty(),

            slave_enabled: Cell::new(false),
            my_slave_address: Cell::new(0),
            slave_read_buffer: TakeCell::empty(),
            slave_read_buffer_len: Cell::new(0),
            slave_read_buffer_index: Cell::new(0),
            slave_write_buffer: TakeCell::empty(),
            slave_write_buffer_len: Cell::new(0),
            slave_write_buffer_index: Cell::new(0),
        }
    }

    /// Set the clock prescaler and the time widths of the I2C signals
    /// in the CWGR register to make the bus run at a particular I2C speed.
    fn set_bus_speed(&self) {

        // These parameters are copied from the Michael's TinyOS implementation. Are they correct?
        // Who knows... Michael probably knows. We were originally copying the parameters from the
        // Atmel Software Framework, but those parameters didn't agree with the accelerometer and
        // nearly burned my fingers. Michael's parameters seem to work without danger of injury.
        //
        // Ultimately we should understand what the heck these parameters actually mean and either
        // confirm them or replace them. They almost certainly depend on the clock speed of the
        // CPU, so we'll need to change them if we change the CPU clock speed.
        let (exp, data, stasto, high, low) = (3, 4, 10, 10, 10);

        let cwgr = ((exp & 0x7) << 28) | ((data & 0xF) << 24) | ((stasto & 0xFF) << 16) |
                   ((high & 0xFF) << 8) | ((low & 0xFF) << 0);
        let regs: &mut Registers = unsafe { mem::transmute(self.registers) };
        write_volatile(&mut regs.clock_waveform_generator, cwgr);
    }

    pub fn set_dma(&self, dma: &'static DMAChannel) {
        self.dma.replace(dma);
    }

    pub fn set_master_client(&self, client: &'static hil::i2c::I2CHwMasterClient) {
        self.master_client.replace(client);
    }

    pub fn set_slave_client(&self, client: &'static hil::i2c::I2CHwSlaveClient) {
        self.slave_client.replace(client);
    }

    pub fn handle_interrupt(&self) {
        use kernel::hil::i2c::Error;
        let regs: &mut Registers = unsafe { mem::transmute(self.registers) };

        let old_status = read_volatile(&regs.status);

        write_volatile(&mut regs.status_clear, !0);

        let err = match old_status {
            x if x & (1 <<  8) != 0 /*ANACK*/  => Some(Error::AddressNak),
            x if x & (1 <<  9) != 0 /*DNACK*/  => Some(Error::DataNak),
            x if x & (1 << 10) != 0 /*ARBLST*/ => Some(Error::ArbitrationLost),
            x if x & (1 <<  3) != 0 /*CCOMP*/   => Some(Error::CommandComplete),
            _ => None
        };

        match self.on_deck.take() {
            None => {
                write_volatile(&mut regs.command, 0);
                write_volatile(&mut regs.next_command, 0);

                err.map(|err| {
                    // enable, reset, disable
                    write_volatile(&mut regs.control, 0x1 << 0);
                    write_volatile(&mut regs.control, 0x1 << 7);
                    write_volatile(&mut regs.control, 0x1 << 1);

                    self.master_client.map(|client| {
                        let buf = match self.dma.take() {
                            Some(dma) => {
                                let b = dma.abort_xfer();
                                self.dma.replace(dma);
                                b
                            }
                            None => None,
                        };
                        buf.map(|buf| {
                            client.command_complete(buf, err);
                        });
                    });
                });
            }
            Some((dma_periph, len)) => {
                // Enable transaction error interrupts
                write_volatile(&mut regs.interrupt_enable,
                               (1 << 3)    // CCOMP   - Command completed
                               | (1 << 8)    // ANAK   - Address not ACKd
                               | (1 << 9)    // DNAK   - Data not ACKd
                               | (1 << 10)); // ARBLST - Abitration lost
                self.dma.map(|dma| {
                    let buf = dma.abort_xfer().unwrap();
                    dma.prepare_xfer(dma_periph, buf, len);
                    dma.start_xfer();
                });
            }
        }
    }

    fn setup_xfer(&self, chip: u8, flags: usize, read: bool, len: u8) {
        let regs: &mut Registers = unsafe { mem::transmute(self.registers) };

        // disable before configuring
        write_volatile(&mut regs.control, 0x1 << 1);

        let read = if read { 1 } else { 0 };
        let command = ((chip as usize) << 1) // 7 bit address at offset 1 (8th
                                             // bit is ignored anyway)
                    | flags  // START, STOP & ACKLAST flags
                    | (1 << 15) // VALID
                    | (len as usize) << 16 // NBYTES (at most 255)
                    | read;
        write_volatile(&mut regs.command, command);
        write_volatile(&mut regs.next_command, 0);

        // Enable transaction error interrupts
        write_volatile(&mut regs.interrupt_enable,
                       (1 << 3)    // CCOMP   - Command completed
                       | (1 << 8)    // ANAK   - Address not ACKd
                       | (1 << 9)    // DNAK   - Data not ACKd
                       | (1 << 10)); // ARBLST - Abitration lost
    }

    fn setup_nextfer(&self, chip: u8, flags: usize, read: bool, len: u8) {
        let regs: &mut Registers = unsafe { mem::transmute(self.registers) };

        // disable before configuring
        write_volatile(&mut regs.control, 0x1 << 1);

        let read = if read { 1 } else { 0 };
        let command = ((chip as usize) << 1) // 7 bit address at offset 1 (8th
                                             // bit is ignored anyway)
                    | flags  // START, STOP & ACKLAST flags
                    | (1 << 15) // VALID
                    | (len as usize) << 16 // NBYTES (at most 255)
                    | read;
        write_volatile(&mut regs.next_command, command);

        // Enable
        write_volatile(&mut regs.control, 0x1 << 0);
    }

    fn master_enable(&self) {
        let regs: &mut Registers = unsafe { mem::transmute(self.registers) };

        // Enable to begin transfer
        write_volatile(&mut regs.control, 0x1 << 0);

    }

    pub fn write(&self, chip: u8, flags: usize, data: &'static mut [u8], len: u8) {
        self.dma.map(move |dma| {
            dma.enable();
            dma.prepare_xfer(self.dma_pids.1, data, len as usize);
            self.setup_xfer(chip, flags, false, len);
            self.master_enable();
            dma.start_xfer();
        });
    }

    pub fn read(&self, chip: u8, flags: usize, data: &'static mut [u8], len: u8) {
        self.dma.map(move |dma| {
            dma.enable();
            dma.prepare_xfer(self.dma_pids.0, data, len as usize);
            self.setup_xfer(chip, flags, true, len);
            self.master_enable();
            dma.start_xfer();
        });
    }

    pub fn write_read(&self, chip: u8, data: &'static mut [u8], split: u8, read_len: u8) {
        self.dma.map(move |dma| {
            dma.enable();
            dma.prepare_xfer(self.dma_pids.1, data, split as usize);
            self.setup_xfer(chip, START, false, split);
            self.setup_nextfer(chip, START | STOP, true, read_len);
            self.on_deck.replace((self.dma_pids.0, read_len as usize));
            dma.start_xfer();
        });
    }

    fn enable_interrupts(&self) {
        unsafe {
            nvic::enable(self.nvic);
        }
    }

    fn disable_interrupts(&self) {
        let regs: &mut Registers = unsafe { mem::transmute(self.registers) };
        write_volatile(&mut regs.interrupt_disable, !0);
        unsafe {
            nvic::disable(self.nvic);
        }
    }

    /// Handle possible interrupt for TWIS module.
    pub fn handle_slave_interrupt(&self) {

        self.slave_registers.map(|slave_registers| {
            let regs: &mut TWISRegisters = unsafe { mem::transmute(slave_registers) };

            // Get current status from the hardware.
            let status = regs.status.get();
            let imr = regs.interrupt_mask.get();
            let interrupts = status & imr;

            // Check for errors.
            if interrupts & ((1 << 14) | (1 << 13) | (1 << 12) | (1 << 7) | (1 << 6)) > 0 {
                // From the datasheet: If a bus error (misplaced START or STOP)
                // condition is detected, the SR.BUSERR bit is set and the TWIS
                // waits for a new START condition.
                if interrupts & (1 << 14) > 0 {
                    // Restart and wait for the next start byte
                    regs.status_clear.set(status);
                    return;
                }

                panic!("ERR 0x{:x}", interrupts);
            }

            // Check if we got the address match interrupt
            if interrupts & (1 << 16) > 0 {

                regs.nbytes.set(0);

                // Did we get a read or a write?
                if status & (1 << 5) > 0 {
                    // This means the slave is in transmit mode, AKA we got a
                    // read.

                    // Clear the byte transfer done if set (copied from ASF)
                    regs.status_clear.set(1 << 23);

                    // Setup interrupts that we now care about
                    regs.interrupt_enable.set((1 << 3) | (1 << 23));
                    regs.interrupt_enable
                        .set((1 << 14) | (1 << 13) | (1 << 12) | (1 << 7) | (1 << 6));

                    if self.slave_read_buffer.is_some() {
                        // Have buffer to send, start reading
                        self.slave_read_buffer_index.set(0);
                        let len = self.slave_read_buffer_len.get();

                        if len >= 1 {
                            self.slave_read_buffer.map(|buffer| {
                                regs.transmit_holding.set(buffer[0] as u32);
                            });
                            self.slave_read_buffer_index.set(1);
                        } else {
                            // Send dummy byte
                            regs.transmit_holding.set(0x2e);
                        }

                        // Make it happen by clearing status.
                        regs.status_clear.set(status);


                    } else {
                        // Call to upper layers asking for a buffer to send
                        self.slave_client.map(|client| {
                            client.read_expected();
                        });
                    }

                } else {
                    // Slave is in receive mode, AKA we got a write.

                    // Get transmission complete and rxready interrupts.
                    regs.interrupt_enable.set((1 << 3) | (1 << 0));

                    // Set index to 0
                    self.slave_write_buffer_index.set(0);

                    if self.slave_write_buffer.is_some() {
                        // Clear to continue with existing buffer.
                        regs.status_clear.set(status);

                    } else {
                        // Call to upper layers asking for a buffer to
                        // read into.
                        self.slave_client.map(|client| {
                            client.write_expected();
                        });
                    }
                }

            } else {
                // Did not get address match interrupt.

                if interrupts & (1 << 3) > 0 {
                    // Transmission complete

                    let nbytes = regs.nbytes.get();

                    regs.interrupt_disable.set(0xFFFFFFFF);
                    regs.interrupt_enable.set(1 << 16);
                    regs.status_clear.set(status);

                    if status & (1 << 5) > 0 {
                        // read
                        self.slave_client.map(|client| {
                            self.slave_read_buffer.take().map(|buffer| {
                                client.command_complete(buffer,
                                                        nbytes as u8,
                                                        hil::i2c::SlaveTransmissionType::Read);
                            });
                        });

                    } else {
                        // write

                        let len = self.slave_write_buffer_len.get();
                        let idx = self.slave_write_buffer_index.get();

                        if len > idx {
                            self.slave_write_buffer.map(|buffer| {
                                buffer[idx as usize] = regs.receive_holding.get() as u8;
                            });
                            self.slave_write_buffer_index.set(idx + 1);
                        } else {
                            // Just drop on floor
                            regs.receive_holding.get();
                        }

                        self.slave_client.map(|client| {
                            self.slave_write_buffer.take().map(|buffer| {
                                client.command_complete(buffer,
                                                        nbytes as u8,
                                                        hil::i2c::SlaveTransmissionType::Write);
                            });
                        });
                    }

                } else if interrupts & (1 << 23) > 0 {
                    // Byte transfer finished. Send the next byte from the
                    // buffer.

                    if self.slave_read_buffer.is_some() {
                        // Have buffer to send, start reading
                        let len = self.slave_read_buffer_len.get();
                        let idx = self.slave_read_buffer_index.get();

                        if len > idx {
                            self.slave_read_buffer.map(|buffer| {
                                regs.transmit_holding.set(buffer[idx as usize] as u32);
                            });
                            self.slave_read_buffer_index.set(idx + 1);
                        } else {
                            // Send dummy byte
                            regs.transmit_holding.set(0xdf);
                        }

                    } else {
                        // Send a default byte
                        regs.transmit_holding.set(0xdc);
                    }

                    // Make it happen by clearing status.
                    regs.status_clear.set(status);

                } else if interrupts & (1 << 0) > 0 {
                    // Receive byte ready.

                    if self.slave_write_buffer.is_some() {
                        // Check that the BTF byte is set at the beginning of
                        // the transfer. Sometimes a spurious RX ready interrupt
                        // happens at the beginning (right after the address
                        // byte) that we need to ignore, and checking the BTF
                        // bit fixes that. However, sometimes in the middle of a
                        // transfer we get an RXREADY interrupt where the BTF
                        // bit is NOT set. I don't know why.
                        if status & (1 << 23) > 0 || self.slave_write_buffer_index.get() > 0 {
                            // Have buffer to read into
                            let len = self.slave_write_buffer_len.get();
                            let idx = self.slave_write_buffer_index.get();

                            if len > idx {
                                self.slave_write_buffer.map(|buffer| {
                                    buffer[idx as usize] = regs.receive_holding.get() as u8;
                                });
                                self.slave_write_buffer_index.set(idx + 1);
                            } else {
                                // Just drop on floor
                                regs.receive_holding.get();
                            }
                        } else {
                            // Just drop on floor
                            regs.receive_holding.get();
                        }
                    } else {
                        // Just drop on floor
                        regs.receive_holding.get();
                    }

                    regs.status_clear.set(status);
                }
            }
        });
    }

    /// Receive the bytes the I2C master is writing to us.
    pub fn slave_write_receive(&self, buffer: &'static mut [u8], len: u8) {

        self.slave_write_buffer.replace(buffer);
        self.slave_write_buffer_len.set(len);

        if self.slave_enabled.get() {

            self.slave_registers.map(|slave_registers| {
                let regs: &mut TWISRegisters = unsafe { mem::transmute(slave_registers) };

                let status = regs.status.get();
                let imr = regs.interrupt_mask.get();
                let interrupts = status & imr;

                // Address match status bit still set, so we need to tell the TWIS
                // to continue.
                if (interrupts & (1 << 16) > 0) && (status & (1 << 5) == 0) {
                    regs.status_clear.set(status);
                }
            });
        }
    }

    /// Prepare a buffer for the I2C master to read from after a read call.
    pub fn slave_read_send(&self, buffer: &'static mut [u8], len: u8) {

        self.slave_read_buffer.replace(buffer);
        self.slave_read_buffer_len.set(len);
        self.slave_read_buffer_index.set(0);

        if self.slave_enabled.get() {

            // Check to see if we should send the first byte.
            self.slave_registers.map(|slave_registers| {
                let regs: &mut TWISRegisters = unsafe { mem::transmute(slave_registers) };

                let status = regs.status.get();
                let imr = regs.interrupt_mask.get();
                let interrupts = status & imr;

                // Address match status bit still set. We got this function
                // call in response to an incoming read. Send the first
                // byte.
                if (interrupts & (1 << 16) > 0) && (status & (1 << 5) > 0) {
                    regs.status_clear.set(1 << 23);

                    let len = self.slave_read_buffer_len.get();

                    if len >= 1 {
                        self.slave_read_buffer.map(|buffer| {
                            regs.transmit_holding.set(buffer[0] as u32);
                        });
                        self.slave_read_buffer_index.set(1);
                    } else {
                        // Send dummy byte
                        regs.transmit_holding.set(0x75);
                    }

                    // Make it happen by clearing status.
                    regs.status_clear.set(status);
                }
            });
        }
    }

    fn slave_enable_interrupts(&self) {
        self.slave_nvic.map(|slave_nvic| {
            unsafe {
                nvic::enable(slave_nvic);
            }
        });
    }

    fn slave_disable_interrupts(&self) {
        self.slave_registers.map(|slave_registers| {
            let regs: &mut TWISRegisters = unsafe { mem::transmute(slave_registers) };
            regs.interrupt_disable.set(!0);
        });
        self.slave_nvic.map(|slave_nvic| {
            unsafe {
                nvic::disable(slave_nvic);
            }
        });
    }

    pub fn slave_set_address(&self, address: u8) {
        self.my_slave_address.set(address);
    }

    pub fn slave_listen(&self) {
        self.slave_registers.map(|slave_registers| {
            let regs: &mut TWISRegisters = unsafe { mem::transmute(slave_registers) };

            // Enable and configure
            let control = (((self.my_slave_address.get() as usize) & 0x3F) << 16) |
                           (1 << 14) | // SOAM - stretch on address match
                           (1 << 13) | // CUP - count nbytes up
                           (1 << 4)  | // STREN - stretch clock enable
                           (1 << 2);   // SMATCH - ack on slave address
            regs.control.set(control as u32);

            // Set this separately because that makes the HW happy.
            regs.control.set((control as u32) | 0x1);
        });
    }
}

impl DMAClient for I2CHw {
    fn xfer_done(&self, _pid: DMAPeripheral) {}
}

impl hil::i2c::I2CMaster for I2CHw {
    /// This enables the entire I2C peripheral
    fn enable(&self) {
        // Enable the clock for the TWIM module
        unsafe {
            pm::enable_clock(self.master_clock);
        }

        // If exists, disable slave clock
        self.slave_clock.map(|slave_clock| {
            unsafe {
                pm::disable_clock(slave_clock);
            }
        });

        let regs: &mut Registers = unsafe { mem::transmute(self.registers) };

        // enable, reset, disable
        write_volatile(&mut regs.control, 0x1 << 0);
        write_volatile(&mut regs.control, 0x1 << 7);
        write_volatile(&mut regs.control, 0x1 << 1);

        // Init the bus speed
        self.set_bus_speed();

        // slew
        write_volatile(&mut regs.slew_rate, (0x2 << 28) | (7 << 16) | (7 << 0));

        // clear interrupts
        write_volatile(&mut regs.status_clear, !0);

        self.enable_interrupts();
    }

    /// This disables the entire I2C peripheral
    fn disable(&self) {
        let regs: &mut Registers = unsafe { mem::transmute(self.registers) };
        write_volatile(&mut regs.control, 0x1 << 1);
        unsafe {
            pm::disable_clock(self.master_clock);
        }
        self.disable_interrupts();
    }

    fn write(&self, addr: u8, data: &'static mut [u8], len: u8) {
        I2CHw::write(self, addr, START | STOP, data, len);
    }

    fn read(&self, addr: u8, data: &'static mut [u8], len: u8) {
        I2CHw::read(self, addr, START | STOP, data, len);
    }

    fn write_read(&self, addr: u8, data: &'static mut [u8], write_len: u8, read_len: u8) {
        I2CHw::write_read(self, addr, data, write_len, read_len)
    }
}

impl hil::i2c::I2CSlave for I2CHw {
    fn enable(&self) {
        self.slave_clock.map(|slave_clock| {
            unsafe {
                pm::disable_clock(self.master_clock);
                pm::enable_clock(slave_clock);
            }
        });

        self.slave_registers.map(|slave_registers| {
            let regs: &mut TWISRegisters = unsafe { mem::transmute(slave_registers) };

            // enable, reset, disable
            regs.control.set(0x1 << 0);
            regs.control.set(0x1 << 7);
            regs.control.set(0);

            // slew
            regs.slew_rate.set((0x2 << 28) | (7 << 0));

            // clear interrupts
            regs.status_clear.set(!0);

            // We want to interrupt only on slave address match so we can
            // wait for a message from a master and then decide what to do
            // based on read/write.
            regs.interrupt_enable.set((1 << 16));

            // Also setup all of the error interrupts.
            regs.interrupt_enable.set((1 << 14) | (1 << 13) | (1 << 12) | (1 << 7) | (1 << 6));

            // Enable NVIC
            self.slave_enable_interrupts();
        });

        self.slave_enabled.set(true);
    }

    /// This disables the entire I2C peripheral
    fn disable(&self) {
        self.slave_enabled.set(false);

        self.slave_registers.map(|slave_registers| {
            let regs: &mut TWISRegisters = unsafe { mem::transmute(slave_registers) };

            regs.control.set(0);
            self.slave_clock.map(|slave_clock| {
                unsafe {
                    pm::disable_clock(slave_clock);
                }
            });
        });
        self.slave_disable_interrupts();
    }

    fn set_address(&self, addr: u8) {
        self.slave_set_address(addr);
    }

    fn write_receive(&self, data: &'static mut [u8], max_len: u8) {
        self.slave_write_receive(data, max_len);
    }

    fn read_send(&self, data: &'static mut [u8], max_len: u8) {
        self.slave_read_send(data, max_len);
    }

    fn listen(&self) {
        self.slave_listen();
    }
}

impl hil::i2c::I2CMasterSlave for I2CHw {}

interrupt_handler!(twim0_handler, TWIM0, {
    I2C0.disable_interrupts()
});
interrupt_handler!(twim1_handler, TWIM1, {
    I2C1.disable_interrupts()
});
interrupt_handler!(twim2_handler, TWIM2, {
    I2C2.disable_interrupts()
});
interrupt_handler!(twim3_handler, TWIM3, {
    I2C3.disable_interrupts()
});
interrupt_handler!(twis0_handler, TWIS0);
interrupt_handler!(twis1_handler, TWIS1);
