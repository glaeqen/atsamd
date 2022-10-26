//! Power Management Unit abstraction
//!
//! Provides sleep mode and shutdown support.
//!
//! # Notes on accessing sleep modes
//! * Memory validity post-sleep may depend on the retention settings, refer
//!   to the datasheet to see what needs to be done after a wakeup.
//! * Sleep modes (does not include shutdown) will not be entered if there is a
//!   debugger is connected. Any connected debugger will stop the PMU from
//!   putting the core into sleep.
//! * Any peripheral kept alive during sleep modes should implement a recovery
//!   step that defines its state in the abstraction.
//! * Retaining I/O lines may not be suitable for all applications, review your
//!   usage before enabling it.
//! * Shutdown requires a power-on reset to start back up and does not care
//!   about connected debuggers. If shutdown is called unconditionally at the
//!   start of the program, it may lock the user out.
#![deny(warnings)]
#![deny(missing_docs)]
pub use crate::pac::pm::{
    bkupcfg::BRAMCFG_A as BkupConfig,
    hibcfg::{BRAMCFG_A as HibBkupConfig, RAMCFG_A as HibRamConfig},
    sleepcfg::SLEEPMODE_A as SleepMode,
    stdbycfg::{FASTWKUP_A as FastWakeup, RAMCFG_A as StdbyRamConfig},
};
pub use crate::pac::PM;
pub use cortex_m::peripheral::SCB;

/// Power manager peripheral
pub struct Pmu {
    /// Peripheral
    pm: PM,
}

/// Hibernation mode
pub struct HibMode(bool);
/// Standby mode
pub struct StdbyMode(bool);
/// Backup power mode
pub struct BkupMode(bool);

impl Pmu {
    /// Setup a new power manager instance and clear IORET
    ///
    /// If the device had retained memory from before a sleep
    /// the debugger should be able to access the RAM freely after
    /// this
    pub fn new(pm: PM) -> Self {
        let mut pmu = Self { pm };

        // Clearing this bit after wakeup enables debugger access if
        // I/O was retained
        pmu.ioret(false);

        pmu
    }

    /// Set/clear I/O retain
    fn ioret(&mut self, ioret: bool) {
        self.pm.ctrla.write(|w| w.ioret().bit(ioret));
    }

    /// Wait for the device to be ready for sleep
    fn sleep_ready(&self) {
        while !self.pm.intflag.read().sleeprdy().bit() {}
    }

    /// Setup hibernation
    pub fn configure_hib(
        &mut self,
        ram_cfg: HibRamConfig,
        bkup_cfg: HibBkupConfig,
        retain_io: bool,
    ) -> HibMode {
        self.pm
            .hibcfg
            .write(|w| w.bramcfg().variant(bkup_cfg).ramcfg().variant(ram_cfg));

        HibMode(retain_io)
    }

    /// Apply hibernation config and sleep on exit
    pub fn apply_hib(&mut self, mode: HibMode, scb: &mut SCB) {
        self.ioret(mode.0);
        self.pm
            .sleepcfg
            .write(|w| w.sleepmode().variant(SleepMode::HIBERNATE));

        self.sleep_ready();
        scb.set_sleeponexit();
    }

    /// Setup Standby
    pub fn configure_stdby(
        &mut self,
        ram_cfg: StdbyRamConfig,
        fast_wakeup: FastWakeup,
        retain_io: bool,
    ) -> StdbyMode {
        // Configure RAM retention and static power-wakeup time tradeoff
        self.pm
            .stdbycfg
            .write(|w| w.fastwkup().variant(fast_wakeup).ramcfg().variant(ram_cfg));

        StdbyMode(retain_io)
    }

    /// Apply standby config and sleep on exit
    pub fn apply_stdby(&mut self, mode: StdbyMode, scb: &mut SCB) {
        self.ioret(mode.0);
        self.pm
            .sleepcfg
            .write(|w| w.sleepmode().variant(SleepMode::STANDBY));

        self.sleep_ready();
        scb.set_sleeponexit();
    }

    /// Configure backup power mode
    pub fn configure_bkup(&mut self, bkup_cfg: BkupConfig, retain_io: bool) -> BkupMode {
        // Configure RAM retention
        self.pm.bkupcfg.write(|w| w.bramcfg().variant(bkup_cfg));

        BkupMode(retain_io)
    }

    /// Apply backup config and sleep on exit
    pub fn apply_bkup(&mut self, mode: BkupMode, scb: &mut SCB) {
        self.ioret(mode.0);
        self.pm
            .sleepcfg
            .write(|w| w.sleepmode().variant(SleepMode::BACKUP));

        self.sleep_ready();
        scb.set_sleeponexit();
    }

    /// Set the mode in the off-state
    pub fn shutdown(&mut self, scb: &mut SCB) {
        self.pm
            .sleepcfg
            .write(|w| w.sleepmode().variant(SleepMode::OFF));

        scb.set_sleeponexit();
    }
}
