//! # Non-volatile Memory Controller
//!
//! This module allows users to interact with non-volatile memory controller.
//!
//! NVMCTRL is an intermediary between memory buses and physical non-volatile
//! memory. It provides means of managing a flash memory content, its properties
//! (cache, wait states, bootloader blocks protection), power management and
//! address remapping if necessary (in case bank mechanism is used). It also
//! provides an indirection mechanism to achieve non-volatile RAM-like memory
//! within last sectors of a physical flash (More in [`smart_eeprom`] module).
//!
//! NVM supports splitting flash into two sections (opt-in feature) called
//! banks. Bank considered active is mapped to _virtual_ address `0x0`, meaning
//! it contains currently executed application. Through NVM command & control
//! interface, banks can be swapped and MCU reset, so the firmware from the
//! other bank will run after restart.
//!
//! Module features:
//! - Erase & write over non-volatile memory in a device.
//! - Swap banks
#![warn(missing_docs)]

pub mod smart_eeprom;

pub use crate::target_device::nvmctrl::ctrla::PRM_A;
use crate::target_device::nvmctrl::ctrlb::CMD_AW;
use crate::target_device::NVMCTRL;
use core::num::NonZeroU32;
use core::ops::Range;

use bitfield::bitfield;

/// Retrieve a total NVM size using HW registers
#[inline(always)]
pub fn retrieve_flash_size() -> u32 {
    static mut FLASHSIZE: Option<NonZeroU32> = None;
    // Safety: Lazy initialization of a static variable - interactions with
    // `Option<NonZeroU32>` should be atomic
    unsafe {
        match FLASHSIZE {
            Some(x) => x.into(),
            None => {
                let nvm = &*NVMCTRL::ptr();
                let nvm_params = nvm.param.read();
                if !nvm_params.psz().is_512() {
                    unreachable!("NVM page size is always expected to be 512 bytes");
                }
                let nvm_pages = nvm_params.nvmp().bits() as u32;
                let flash_size = nvm_pages * 512;
                // Safety: `flash_size` will never be 0
                FLASHSIZE = Some(NonZeroU32::new_unchecked(flash_size));
                flash_size
            }
        }
    }
}

/// Retrieve a bank size using HW registers
#[inline(always)]
pub fn retrieve_bank_size() -> u32 {
    retrieve_flash_size() / 2
}

/// Size of a page in bytes
pub const PAGESIZE: u32 = 512;

/// Size of one block
pub const BLOCKSIZE: u32 = 512 * 16;

/// Non-volatile memory controller
pub struct Nvm {
    /// PAC peripheral
    nvm: NVMCTRL,
}

/// Errors generated by the NVM peripheral
#[derive(Debug)]
pub enum PeripheralError {
    /// NVM error
    NvmError,
    /// Single ECC error
    EccSingleError,
    /// Dual ECC error
    EccDualError,
    /// Locked error
    LockError,
    /// Programming error
    ProgrammingError,
    /// Address error
    AddressError,
}

/// Driver errors
#[non_exhaustive]
#[derive(Debug)]
pub enum Error {
    /// Target sector is protected
    Protected,
    /// Memory region is used by SmartEEPROM
    SmartEepromArea,
    /// Requested protection state already in place
    NoChangeBootProtection,
    /// Errors generated by hardware
    Peripheral(PeripheralError),
    /// The DSU failed in some way
    Dsu(super::dsu::Error),
    /// An alignment requirement was not fulfilled
    Alignment,
}

/// Physical flash banks
#[derive(PartialEq, Debug)]
pub enum PhysicalBank {
    /// Flash bank A
    A,
    /// Flash bank B
    B,
}

#[derive(PartialEq, Debug)]
/// Flash banks identified by which one we boot from.
///
/// Memory layout:
/// ```text
/// [  Active bank  | Inactive bank ]
/// ^               ^               ^
/// 0x0000_0000     flash_size/2    flash_size
/// ```
pub enum Bank {
    /// Bank that is mapped to 0x0000_0000
    ///
    /// Active bank occupies first half of the flash memory.
    Active,
    /// Bank that is not mapped to 0x0000_0000
    ///
    /// Inactive bank occupies second half of the flash memory.
    Inactive,
}

impl Bank {
    /// Provides the address of the bank
    #[inline]
    pub fn address(&self) -> u32 {
        match self {
            Bank::Active => 0,
            Bank::Inactive => retrieve_bank_size(),
        }
    }

    /// Provides bank length in bytes
    #[inline]
    pub fn length(&self) -> u32 {
        retrieve_bank_size()
    }
}

/// NVM result type
pub type Result<T> = core::result::Result<T, Error>;

impl Nvm {
    /// Create a new NVM controller or handle failure from DSU
    #[inline]
    pub fn new(nvm: NVMCTRL) -> Self {
        Self { nvm }
    }

    /// Swap the flash banks. The processor will be reset, after which the
    /// inactive bank will become the active bank.
    ///
    /// # Safety
    /// Ensure there is a working, memory safe program in place in the inactive
    /// bank before calling.
    pub unsafe fn bank_swap(&mut self) -> ! {
        self.command_sync(CMD_AW::BKSWRST);
        // The reset will happen atomically with the rest of the command, so getting
        // here is an error.
        unreachable!();
    }

    /// Set the power reduction mode
    #[inline]
    pub fn power_reduction_mode(&mut self, prm: PRM_A) {
        self.nvm.ctrla.modify(|_, w| w.prm().variant(prm));
    }

    /// Check if the flash is boot protected
    #[inline]
    pub fn is_boot_protected(&self) -> bool {
        !self.nvm.status.read().bpdis().bit()
    }

    /// Get first bank
    #[inline]
    pub fn first_bank(&self) -> PhysicalBank {
        if self.nvm.status.read().afirst().bit() {
            PhysicalBank::A
        } else {
            PhysicalBank::B
        }
    }

    /// Set address for reading/writing
    fn set_address(&mut self, address: u32) {
        unsafe {
            self.nvm
                .addr
                .write(|w| w.addr().bits(address & 0x00ff_ffff));
        }
    }

    /// Determine if the controller is busy writing or erasing
    #[inline]
    pub fn is_ready(&self) -> bool {
        self.nvm.status.read().ready().bit()
    }

    /// Execute a command, do not wait for it to finish
    fn command(&mut self, command: CMD_AW) {
        self.nvm
            .ctrlb
            .write(|w| w.cmdex().key().cmd().variant(command));
    }

    /// Execute a command, wait until it is done
    fn command_sync(&mut self, command: CMD_AW) {
        self.command(command);

        while !self.nvm.intflag.read().done().bit() {}
        self.nvm.intflag.write(|w| w.done().set_bit());
    }

    /// Read the peripheral state to check error flags and clear the up
    /// afterwards
    fn manage_error_states(&mut self) -> Result<()> {
        let read_intflag = self.nvm.intflag.read();
        // Check ADDRE and LOCKE first as it is more specific than PROGE
        let state = if read_intflag.addre().bit_is_set() {
            Err(Error::Peripheral(PeripheralError::AddressError))
        } else if read_intflag.locke().bit_is_set() {
            Err(Error::Peripheral(PeripheralError::LockError))
        } else if read_intflag.proge().bit_is_set() {
            Err(Error::Peripheral(PeripheralError::ProgrammingError))
        } else {
            Ok(())
        };

        // Clear error flags
        self.nvm.intflag.write(|w| w.addre().set_bit());
        self.nvm.intflag.write(|w| w.locke().set_bit());
        self.nvm.intflag.write(|w| w.proge().set_bit());
        state
    }

    /// Read the user page
    #[inline]
    pub fn user_page(&self) -> Userpage {
        let mut buffer = 0_u128;
        let base_addr: *const u8 = 0x0080_4000 as *const u8;

        for i in 0..16 {
            buffer |= unsafe { core::ptr::read_volatile(base_addr.offset(i as isize)) as u128 }
                << (i * 8);
        }

        Userpage(buffer)
    }

    /// Read the calibration area
    #[inline]
    pub fn calibration_area(&self) -> CalibrationArea {
        let mut buffer = 0_u64;
        let base_addr: *const u8 = 0x0080_0080 as *const u8;

        for i in 0..6 {
            buffer |=
                unsafe { core::ptr::read_volatile(base_addr.offset(i as isize)) as u64 } << (i * 8);
        }

        CalibrationArea(buffer)
    }

    /// Read the calibration area for temperatures
    #[inline]
    pub fn temperatures_calibration_area(&self) -> TemperaturesCalibrationArea {
        let mut buffer = 0_u128;
        let base_addr: *const u8 = 0x0080_0100 as *const u8;

        for i in 0..11 {
            buffer |= unsafe { core::ptr::read_volatile(base_addr.offset(i as isize)) as u128 }
                << (i * 8);
        }

        TemperaturesCalibrationArea(buffer)
    }

    /// Enable/disable boot protection on/off
    ///
    /// Userpage's NVM BOOT field defines a memory region that is supposed to be
    /// protected. `NVMCTRL.STATUS.BOOTPROT` is a readonly HW register populated
    /// on reset with a value from a userpage. By default, 0
    #[inline]
    pub fn boot_protection(&mut self, protect: bool) -> Result<()> {
        // Check if requested state differs from current state
        if self.is_boot_protected() != protect {
            // Wait until ready
            while !self.is_ready() {}

            // Requires both command and key so the command is allowed to execute
            if !protect {
                // Issue Set boot protection disable (disable bootprotection)
                self.command_sync(CMD_AW::SBPDIS);
            } else {
                // Issue Clear boot protection disable (enable bootprotection)
                self.command_sync(CMD_AW::CBPDIS);
            }

            self.manage_error_states()
        } else {
            Err(Error::NoChangeBootProtection)
        }
    }

    /// Write to flash memory
    /// If `address` is not word-aligned, an error is returned.
    #[inline]
    pub unsafe fn write(
        &mut self,
        destination_address: u32,
        source_address: u32,
        words: u32,
    ) -> Result<()> {
        // Length of memory step
        let step_size: u32 = core::mem::size_of::<u32>() as u32;
        // Length of data in bytes
        let length = words * step_size;

        let read_addresses = source_address..(source_address + length);
        let write_addresses = destination_address..(destination_address + length);

        if source_address % step_size != 0 {
            return Err(Error::Alignment);
        }

        if destination_address % step_size != 0 {
            return Err(Error::Alignment);
        }

        if self.contains_bootprotected(&write_addresses) {
            Err(Error::Protected)
        } else if self.contains_smart_eeprom(&write_addresses) {
            Err(Error::SmartEepromArea)
        } else {
            while !self.is_ready() {}
            self.command_sync(CMD_AW::PBC);
            // Track whether we have unwritten data in the page buffer
            let mut dirty = false;
            // Zip two iterators, one counter and one with the addr word aligned
            for (destination_address, source_address) in write_addresses
                .step_by(step_size as usize)
                .zip(read_addresses.step_by(step_size as usize))
            {
                // Write to memory, 32 bits, 1 word.
                // The data is placed in the page buffer and ADDR is updated automatically.
                // Memory is not written until the write page command is issued later.
                let value = core::ptr::read_volatile(source_address as *const u32);
                core::ptr::write_volatile(destination_address as *mut u32, value);
                dirty = true;

                // If we are about to cross a page boundary (and run out of page buffer), write
                // to flash
                if destination_address % PAGESIZE >= PAGESIZE - step_size {
                    // Wait until ready
                    while !self.is_ready() {}

                    dirty = false;
                    // Perform a write
                    self.command_sync(CMD_AW::WP);
                }
            }

            // Wait until the last write operation is finished
            while !self.is_ready() {}

            if dirty {
                // The dirty flag has fulfilled its role here, so we don't bother to maintain
                // its invariant anymore. Otherwise, the compiler would warn of
                // unused assignments. Write last page
                self.command_sync(CMD_AW::WP);
            }

            self.manage_error_states()
        }
    }

    /// Erase flash memory.
    ///
    /// Unit of `length` depends on a chosen erasing granularity.
    #[inline]
    pub unsafe fn erase(
        &mut self,
        address: u32,
        length: u32,
        granularity: EraseGranularity,
    ) -> Result<()> {
        // Align to block/page boundary
        // While the NVM will accept any address in the block, we need to compute the
        // aligned address to check for boot protection.
        let flash_address = address - address % granularity.size();
        let range_to_erase = flash_address..(flash_address + length * granularity.size());

        if self.contains_bootprotected(&range_to_erase) {
            Err(Error::Protected)
        } else if self.contains_smart_eeprom(&range_to_erase) {
            Err(Error::SmartEepromArea)
        } else {
            for address in range_to_erase.step_by(granularity.size() as usize) {
                // Set target address to current block/page offset
                self.set_address(address);

                // Wait until ready
                while !self.is_ready() {}

                // Erase block/page, wait for completion
                self.command_sync(granularity.command());

                self.manage_error_states()?
            }

            Ok(())
        }
    }

    fn contains_bootprotected(&self, inp: &Range<u32>) -> bool {
        // Calculate size that is protected for bootloader
        //   * 15 = no bootprotection, default value
        //   * 0 = max bootprotection, 15 * 8Kibyte = 120KiB
        //   * (15 - bootprot) * 8KiB = protected size
        let bootprot = self.nvm.status.read().bootprot().bits();
        let bp_space = 8 * 1024 * (15 - bootprot) as u32;

        let boot = &(Bank::Active.address()..(Bank::Active.address() + bp_space));
        self.is_boot_protected() && range_overlap(inp, boot)
    }

    fn contains_smart_eeprom(&self, _inp: &Range<u32>) -> bool {
        false
    }

    /// Retrieve SmartEERPOM
    #[inline]
    pub fn smart_eeprom(&mut self) -> smart_eeprom::Result {
        smart_eeprom::SmartEepromMode::retrieve(self)
    }
}

#[derive(Copy, Clone, Debug)]
/// Data erased per command
pub enum EraseGranularity {
    /// One block. This erase type is supported by main memory
    Block,
    /// One page. This erase type is supported for the AUX memory
    Page,
}

impl EraseGranularity {
    fn command(&self) -> CMD_AW {
        match self {
            EraseGranularity::Block => CMD_AW::EB,
            EraseGranularity::Page => CMD_AW::EP,
        }
    }

    fn size(&self) -> u32 {
        match self {
            EraseGranularity::Block => BLOCKSIZE,
            EraseGranularity::Page => PAGESIZE,
        }
    }
}

fn range_overlap(a: &Range<u32>, b: &Range<u32>) -> bool {
    // When start == end, the range includes no points
    a.start != a.end && b.start != b.end && a.start <= b.end && b.start <= a.end
}

bitfield! {
    #[derive(Copy, Clone, Default)]
    /// POD-style struct representing NVM user page
    pub struct Userpage(u128);
    impl Debug;
    u32;
    bod33_disable, _: 0;
    bod33_level, _: 8, 1;
    bod33_action, _: 10, 9;
    bod33_hysteresis, _: 14, 11;
    bod12_calibration_parameters, _: 25, 12;
    nvm_bootloader_size, _: 29, 26;
    see_sblk, _: 35, 32;
    see_psz, _: 38, 36;
    ram_ecc_disable, _: 39;
    wdt_enable, _: 48;
    wdt_always_on, _: 49;
    wdt_period, _: 53, 50;
    wdt_window, _: 57, 54;
    wdt_ewoffset, _: 61, 58;
    wdt_wen, _: 62;
    nvm_locks, _: 95, 64;
    user_page, _: 127, 96;
}

bitfield! {
    #[derive(Copy, Clone, Default)]
    /// POD-style struct representing NVM calibration area
    pub struct CalibrationArea(u64);
    impl Debug;
    u32;
    ac_bias, _: 1, 0;
    adc0_biascomp, _: 4, 2;
    adc0_biasrefbuf, _: 7, 5;
    adc0_biasr2r, _: 10, 8;
    adc1_biascomp, _: 18, 16;
    adc1_biasrefbuf, _: 21, 19;
    adc1_biasr2r, _: 24, 22;
    usb_transn, _: 36, 32;
    usb_transp, _: 41, 37;
    usb_trim, _: 44, 42;
}

bitfield! {
    #[derive(Copy, Clone, Default)]
    /// POD-style struct representing NVM calibration area for
    /// temperature calibration
    pub struct TemperaturesCalibrationArea(u128);
    impl Debug;
    u32;
    tli, _: 7, 0;
    tld, _: 11, 8;
    thi, _: 19, 12;
    thd, _: 23, 20;
    vpl, _: 51, 40;
    vph, _: 63, 52;
    vcl, _: 75, 63;
    vch, _: 87, 76;
}
