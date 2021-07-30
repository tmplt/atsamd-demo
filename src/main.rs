//! atsamd-demo
#![no_main]
#![no_std]

use core::fmt::Write;

use atsamd_hal::{
    clock::v2::{
        dpll::Dpll, gclk, gclk::Gclk1Div, gclkio::GclkOut, pclk::Pclk, retrieve_clocks, xosc::*,
        xosc32k::*,
    },
    gpio::v2::Pin,
    gpio::v2::*,
    icm::*,
    prelude::*,
    sercom::*,
    time::U32Ext,
};
use panic_halt as _;
use rtic::app;

// SHA Test data
static MESSAGE_REF0: [u32; 16] = [
    0x11111111, 0x22222222, 0x33333333, 0x44444444, 0x55555555, 0x66666666, 0x77777777, 0x88888888,
    0x99999999, 0xaaaaaaaa, 0xbbbbbbbb, 0xcccccccc, 0xdddddddd, 0xeeeeeeee, 0xffffffff, 0x00000000,
];

static MESSAGE_REF1: [u32; 16] = [
    0x80636261, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x18000000,
];

// Expected SHA1 sum result
static MESSAGE_SHA1_RES: [u32; 8] = [
    0x363e99a9, 0x6a810647, 0x71253eba, 0x6cc25078, 0x9dd8d09c, 0x00000000, 0x00000000, 0x00000000,
];

static MESSAGE_SHA224_RES: [u32; 8] = [
    0x227d0923, 0x22d80534, 0x77a44286, 0xb355a2bd, 0xe4bcad2a, 0xf7b3a0bd, 0xa79d6ce3, 0x00000000,
];
static MESSAGE_SHA256_RES: [u32; 8] = [
    0xbf1678ba, 0xeacf018f, 0xde404141, 0x2322ae5d, 0xa36103b0, 0x9c7a1796, 0x61ff10b4, 0xad1500f2,
];

static mut HASH: HashArea = HashArea::default();
static mut ICM_REGION_DESC: Regions = Regions::default();

#[app(device = atsamd_hal::target_device, peripherals = true)]
mod app {
    use super::*;

    #[shared]
    struct SharedResources {
        uart: UART0<Pin<PA05, AlternateD>, Pin<PA04, AlternateD>, (), ()>,
        icm: Icm,
    }

    #[local]
    struct LocalResources {
        icm_region0: Region<Region0>,
        icm_region1: Region<Region1>,
        icm_region2: Region<Region2>,
        icm_region3: Region<Region3>,
        message_region0_sha1: [u32; 16],
        message_region1_sha1: [u32; 16],
        message_region2_sha224: [u32; 16],
        message_region3_sha256: [u32; 16],
    }

    #[init]
    fn init(cx: init::Context) -> (SharedResources, LocalResources, init::Monotonics()) {
        let mut device = cx.device;

        // Get the clocks & tokens
        let (gclk0, dfll, _osculp32k, tokens) = retrieve_clocks(
            device.OSCCTRL,
            device.OSC32KCTRL,
            device.GCLK,
            device.MCLK,
            &mut device.NVMCTRL,
        );

        // Get the pins
        let pins = Pins::new(device.PORT);

        let crystal = CrystalConfig::new(8.mhz()).unwrap();

        // Steal access to mclk for UART v1
        let (_, _, _, mut mclk) = unsafe { tokens.pac.steal() };

        // Enable pin PA14 and PA15 as an external source for XOSC0 at 8 MHz
        let xosc0 = Xosc::from_crystal(tokens.xosc0, pins.pa14, pins.pa15, crystal).enable();

        // Take DFLL 48 MHz, divide down to 2 MHz for Gclk1
        let (gclk1, dfll) = gclk::Gclk::new(tokens.gclks.gclk1, dfll);
        let _gclk1 = gclk1.div(Gclk1Div::Div(24)).enable();

        // Configure DPLL0 to 100 MHz fed from Xosc0
        let (dpll0, _xosc0) = Dpll::from_xosc(tokens.dpll0, xosc0, 1);

        // Use 4 as source predivider, 8 MHz / (2 * ( 1 + prediv) * 50 = 100 MHz,
        // where prediv = 1
        let dpll0 = unsafe { dpll0.set_source_div(1).set_loop_div(50, 0).force_enable() };

        // Change Gclk0 from Dfll to Dpll0, MCLK = 100 MHz
        let (gclk0, _dfll, _dpll0) = gclk0.swap(dfll, dpll0);

        // Output Gclk0 on pin PB14
        let (_gclk_out0, gclk0) =
            GclkOut::enable(tokens.gclk_io.gclk_out0, pins.pb14, gclk0, false);

        // Enable external 32k-oscillator
        let xosc32k =
            Xosc32k::from_crystal(tokens.xosc32k, pins.pa00, pins.pa01).set_gain_mode(true);
        let xosc32k = xosc32k.enable();
        let xosc32k = xosc32k.activate_1k();
        let _xosc32k = xosc32k.activate_32k();

        // Enable ICM apb clock
        // Clock v1
        //mclk.apbcmask.modify(|_, w| w.icm_().set_bit());
        // Clock v2
        tokens.apbs.icm.enable();

        // Create new ICM
        let mut icm = Icm::new(device.ICM);

        // Reset the ICM, clearing past error states
        icm.swrst();

        // End of Monitoring is permitted
        icm.set_eomdis(false);
        // Write Back is permitted
        icm.set_wbdis(false);
        // Secondary List branching is forbidden
        icm.set_slbdis(false);
        // Automatic Switch to Compare is disabled
        icm.set_ascd(false);

        // Region Descriptor create a new one with intention of
        // replacing ICM_REGION_DESC
        let mut icm_region_desc = Regions::default();

        // Get the interface for Region0 and enable monitoring
        let icm_region0: Region<Region0> = icm.enable_region();
        icm_region0.enable_monitoring();

        // Setup desired interrupts
        //
        // Region Hash Completed
        icm_region0.set_rhc_int();

        // Region0 raddr
        icm_region_desc
            .region0
            .set_region_address(MESSAGE_REF0.as_ptr());

        // Configure the RCFG

        // Some are default values, just as an example

        // Activate Write back (should be true when comparing memory)
        icm_region_desc.region0.rcfg.set_cdwbn(false);
        // Should the ICM controller loop back to DSCR after this region?
        icm_region_desc.region0.rcfg.set_wrap(false);
        // Set this as the end of descriptor linked list
        icm_region_desc.region0.rcfg.set_eom(false);
        // The RHC flag is set when the field NEXT = 0
        // in a descriptor of the main or second list
        icm_region_desc.region0.rcfg.set_rhien(false);
        // Set Algorithm to SHA1
        icm_region_desc.region0.rcfg.set_algo(icm_algorithm::SHA1);

        // Get the interface for region1
        let icm_region1 = icm.enable_region1();

        // Enable region monitoring
        icm_region1.enable_monitoring();

        // Setup desired interrupts
        //
        // Region Hash Completed
        icm_region1.set_rhc_int();

        // Region1 raddr
        icm_region_desc
            .region1
            .set_region_address(MESSAGE_REF1.as_ptr());

        // Configure the RCFG
        // The RHC flag is set when the field NEXT = 0
        // in a descriptor of the main or second list
        icm_region_desc.region1.rcfg.set_rhien(false);
        // Set Algorithm to SHA1
        icm_region_desc.region1.rcfg.set_algo(icm_algorithm::SHA1);

        // Get the interface for region2
        let icm_region2 = icm.enable_region2();

        // Enable region monitoring
        icm_region2.enable_monitoring();

        // Setup desired interrupts
        //
        // Region Hash Completed
        icm_region2.set_rhc_int();

        // Region2 raddr
        icm_region_desc
            .region2
            .set_region_address(MESSAGE_REF1.as_ptr());

        // Configure the RCFG
        // The RHC flag is set when the field NEXT = 0
        // in a descriptor of the main or second list
        icm_region_desc.region2.rcfg.set_rhien(false);
        // Set Algorithm to SHA224
        icm_region_desc.region2.rcfg.set_algo(icm_algorithm::SHA224);

        // Get the interface for region3
        let icm_region3 = icm.enable_region3();

        // Enable region monitoring
        icm_region3.enable_monitoring();

        // Setup desired interrupts
        //
        // Region Hash Completed
        icm_region3.set_rhc_int();

        // Region3 raddr
        icm_region_desc
            .region3
            .set_region_address(MESSAGE_REF1.as_ptr());

        // Configure the RCFG
        //
        // Set this as the end of descriptor linked list
        icm_region_desc.region3.rcfg.set_eom(true);
        // The RHC flag is set when the field NEXT = 0
        // in a descriptor of the main or second list
        icm_region_desc.region3.rcfg.set_rhien(false);
        // Set Algorithm to SHA256
        icm_region_desc.region3.rcfg.set_algo(icm_algorithm::SHA256);

        // Safe because Interrupts are disabled in RTIC Init
        unsafe {
            // Hash Area
            // Set HASH addr to the beginning of the Hash area
            icm.set_hash_addr(&HASH);
        }

        unsafe {
            // Move the icm_region_desc into static
            ICM_REGION_DESC = icm_region_desc;
            // Set DSCR to the beginning of the region descriptor
            icm.set_dscr_addr(&ICM_REGION_DESC.region0);
            // the same but via helper function
            //ICM_REGION_DESC.region0.set_dscr_addr(&icm);
        }

        // Start the ICM calculation
        icm.enable();

        // Setup the compare regions
        let message_region0_sha1 = MESSAGE_REF0;
        let message_region1_sha1 = MESSAGE_REF1;
        let message_region2_sha224 = MESSAGE_REF1;
        let message_region3_sha256 = MESSAGE_REF1;

        let (sercom_pclk, _gclk0) = Pclk::enable(tokens.pclks.sercom0, gclk0);
        let sercom_pclk = sercom_pclk.into();

        let mut uart = UART0::new(
            &sercom_pclk,
            115_200.hz(),
            device.SERCOM0,
            &mut mclk,
            (pins.pa05.into(), pins.pa04.into()),
        );
        uart.intenset(|w| {
            w.rxc().set_bit();
        });

        cortex_m::asm::bkpt();

        uart.write_str("\n\rBooted RTIC.\n\r").unwrap();

        (
            SharedResources { uart, icm },
            LocalResources {
                icm_region0,
                icm_region1,
                icm_region2,
                icm_region3,
                message_region0_sha1,
                message_region1_sha1,
                message_region2_sha224,
                message_region3_sha256,
            },
            init::Monotonics(),
        )
    }

    #[task(binds = SERCOM0_2, shared = [uart, icm])]
    fn uart(cx: uart::Context) {
        let mut uart = cx.shared.uart;

        // Basic echo
        let input = uart.lock(|u| u.read().unwrap());
        uart.lock(|u| write!(u, "{}", input as char).unwrap());
    }

    #[task(priority= 3, binds = ICM,
        shared = [uart, icm],
        local = [icm_region0, icm_region1, icm_region2, icm_region3,
        message_region0_sha1, message_region1_sha1, message_region2_sha224,
        message_region3_sha256
        ])]
    fn icm(cx: icm::Context) {
        let mut uart = cx.shared.uart;
        let mut icm = cx.shared.icm;
        let icm_region0 = cx.local.icm_region0;
        let icm_region1 = cx.local.icm_region1;
        let icm_region2 = cx.local.icm_region2;
        let icm_region3 = cx.local.icm_region3;

        uart.lock(|u| writeln!(u, "\rICM Interrupt!").unwrap());

        // Get a parseable copy of the interrupt status vector
        let icminterrupt = icm.lock(|i| i.get_interrupt_status());
        //cortex_m::asm::bkpt();

        // Check that all hashes has been computed
        if icminterrupt.get_rhc_int().is_all() {
            // Use the RHC-mask to toggle between Write Back
            // and Digest Compare modes
            if icm_region0.get_rhc_int_mask() {
                // Disable RHC interrupts
                icm_region0.disable_rhc_int();
                icm_region1.disable_rhc_int();
                icm_region2.disable_rhc_int();
                icm_region3.disable_rhc_int();
            }

            uart.lock(|u| {
                writeln!(u, "\rRegion 0: Expected,  Actual - SHA1 (should mismatch)").unwrap()
            });

            for (index, val) in MESSAGE_SHA1_RES.iter().enumerate() {
                unsafe {
                    let cmp = HASH.region0[index];
                    if *val == cmp {
                        uart.lock(|u| {
                            writeln!(u, "\r   Match! {:#010x} {:#010x}", *val, cmp).unwrap()
                        });
                    } else {
                        uart.lock(|u| {
                            writeln!(u, "\rmismatch! {:#010x} {:#010x}", *val, cmp).unwrap()
                        });
                    }
                }
            }
            uart.lock(|u| {
                writeln!(u, "\rRegion 1: Expected,  Actual - SHA1 (should match)").unwrap()
            });
            for (index, val) in MESSAGE_SHA1_RES.iter().enumerate() {
                unsafe {
                    let cmp = HASH.region1[index];
                    if *val == cmp {
                        uart.lock(|u| {
                            writeln!(u, "\r   Match! {:#010x} {:#010x}", *val, cmp).unwrap()
                        });
                    } else {
                        uart.lock(|u| {
                            writeln!(u, "\rmismatch! {:#010x} {:#010x}", *val, cmp).unwrap()
                        });
                    }
                }
            }
            uart.lock(|u| {
                writeln!(u, "\rRegion 2: Expected,  Actual - SHA224 (should match)").unwrap()
            });
            for (index, val) in MESSAGE_SHA224_RES.iter().enumerate() {
                unsafe {
                    let cmp = HASH.region2[index];
                    if *val == cmp {
                        uart.lock(|u| {
                            writeln!(u, "\r   Match! {:#010x} {:#010x}", *val, cmp).unwrap()
                        });
                    } else {
                        uart.lock(|u| {
                            writeln!(u, "\rmismatch! {:#010x} {:#010x}", *val, cmp).unwrap()
                        });
                    }
                }
            }

            uart.lock(|u| {
                writeln!(u, "\rRegion 3: Expected,  Actual - SHA256 (should match)").unwrap()
            });
            for (index, val) in MESSAGE_SHA256_RES.iter().enumerate() {
                unsafe {
                    let cmp = HASH.region3[index];
                    if *val == cmp {
                        uart.lock(|u| {
                            writeln!(u, "\r   Match! {:#010x} {:#010x}", *val, cmp).unwrap()
                        });
                    } else {
                        uart.lock(|u| {
                            writeln!(u, "\rmismatch! {:#010x} {:#010x}", *val, cmp).unwrap()
                        });
                    }
                }
            }

            // Reconfigure ICM to watch and compare memory instead
            uart.lock(|u| writeln!(u, "\rDone!").unwrap());

            uart.lock(|u| writeln!(u, "\r Switch to region monitoring mode",).unwrap());
            icm.lock(|i| i.swrst());

            // Create temporary Region
            let mut icm_region_desc = Regions::default();

            // Setup region 0 to monitor memory
            icm_region_desc
                .region0
                .set_region_address(cx.local.message_region0_sha1);
            icm_region_desc
                .region0
                .rcfg
                .reset_region_configuration_to_default();
            icm_region_desc.region0.rcfg.set_algo(icm_algorithm::SHA1);
            // Activate Compare Digest (should be true when comparing memory)
            icm_region_desc.region0.rcfg.set_cdwbn(true);
            // Digest Mismatch Interrupt Disable (enabled)
            icm_region_desc.region0.rcfg.set_dmien(false);

            // Set Region Mismatch Interrupt
            icm_region0.set_rdm_int();

            // Setup region 1 to monitor memory
            icm_region_desc
                .region1
                .set_region_address(cx.local.message_region1_sha1);
            icm_region_desc
                .region1
                .rcfg
                .reset_region_configuration_to_default();
            icm_region_desc.region1.rcfg.set_algo(icm_algorithm::SHA1);
            // Activate Compare Digest (should be true when comparing memory)
            icm_region_desc.region1.rcfg.set_cdwbn(true);
            // Digest Mismatch Interrupt Disable (enabled)
            icm_region_desc.region1.rcfg.set_dmien(false);

            // Set Region Mismatch Interrupt
            icm_region1.set_rdm_int();

            // Setup region 2 to monitor memory
            icm_region_desc
                .region2
                .set_region_address(cx.local.message_region2_sha224);
            icm_region_desc
                .region2
                .rcfg
                .reset_region_configuration_to_default();
            icm_region_desc.region2.rcfg.set_algo(icm_algorithm::SHA224);
            // Activate Compare Digest (should be true when comparing memory)
            icm_region_desc.region2.rcfg.set_cdwbn(true);
            // Digest Mismatch Interrupt Disable (enabled)
            icm_region_desc.region2.rcfg.set_dmien(false);

            // Set Region Mismatch Interrupt
            icm_region2.set_rdm_int();

            // Setup region 3 to monitor memory
            icm_region_desc
                .region3
                .set_region_address(cx.local.message_region3_sha256);
            icm_region_desc
                .region3
                .rcfg
                .reset_region_configuration_to_default();
            icm_region_desc.region3.rcfg.set_algo(icm_algorithm::SHA256);
            // Activate Compare Digest (should be true when comparing memory)
            icm_region_desc.region3.rcfg.set_cdwbn(true);
            // Digest Mismatch Interrupt Disable (enabled)
            icm_region_desc.region3.rcfg.set_dmien(false);
            // Wrap
            icm_region_desc.region3.rcfg.set_wrap(true);

            // Set Region Mismatch Interrupt
            icm_region3.set_rdm_int();

            // Modify regions to trigger interrupts
            uart.lock(|u| writeln!(u, "\rManually modify region0").unwrap());
            cx.local.message_region0_sha1[3] = 0xDEAD_BEEF;
            uart.lock(|u| writeln!(u, "\rManually modify region1").unwrap());
            cx.local.message_region1_sha1[4] = 0xDEAD_BEEF;
            uart.lock(|u| writeln!(u, "\rManually modify region2").unwrap());
            cx.local.message_region2_sha224[5] = 0xDEAD_BEEF;
            uart.lock(|u| writeln!(u, "\rManually modify region3").unwrap());
            cx.local.message_region3_sha256[6] = 0xDEAD_BEEF;

            // Copy the configured Regions into the static mut ICM is reading
            unsafe {
                ICM_REGION_DESC = icm_region_desc;
            }

            icm.lock(|i| i.enable());
        } else if icminterrupt.get_rdm_int().is_all() {
            if icminterrupt
                .get_rdm_int()
                .intersects(RegionDigestMismatch::R0)
            {
                uart.lock(|u| writeln!(u, "\rRegion0 digest mismatch!",).unwrap());
                // Disable the interrupt
                icm_region0.disable_rdm_int();

                uart.lock(|u| writeln!(u, "\rRegion 0: Expected,  Actual").unwrap());
                for (index, val) in MESSAGE_REF0.iter().enumerate() {
                    let cmp = cx.local.message_region0_sha1[index];
                    if *val == cmp {
                        uart.lock(|u| {
                            writeln!(u, "\r   Match! {:#010x} {:#010x}", *val, cmp).unwrap()
                        });
                    } else {
                        uart.lock(|u| {
                            writeln!(u, "\rmismatch! {:#010x} {:#010x}", *val, cmp).unwrap()
                        });
                    }
                }
            }
            if icminterrupt
                .get_rdm_int()
                .intersects(RegionDigestMismatch::R1)
            {
                uart.lock(|u| writeln!(u, "\rRegion1 digest mismatch!",).unwrap());
                // Disable the interrupt
                icm_region1.disable_rdm_int();

                uart.lock(|u| writeln!(u, "\rRegion 1: Expected,  Actual").unwrap());
                for (index, val) in MESSAGE_REF1.iter().enumerate() {
                    let cmp = cx.local.message_region1_sha1[index];
                    if *val == cmp {
                        uart.lock(|u| {
                            writeln!(u, "\r   Match! {:#010x} {:#010x}", *val, cmp).unwrap()
                        });
                    } else {
                        uart.lock(|u| {
                            writeln!(u, "\rmismatch! {:#010x} {:#010x}", *val, cmp).unwrap()
                        });
                    }
                }
            }
            if icminterrupt
                .get_rdm_int()
                .intersects(RegionDigestMismatch::R2)
            {
                uart.lock(|u| writeln!(u, "\rRegion2 digest mismatch!",).unwrap());
                // Disable the interrupt
                icm_region2.disable_rdm_int();

                uart.lock(|u| writeln!(u, "\rRegion 2: Expected,  Actual").unwrap());
                for (index, val) in MESSAGE_REF1.iter().enumerate() {
                    let cmp = cx.local.message_region2_sha224[index];
                    if *val == cmp {
                        uart.lock(|u| {
                            writeln!(u, "\r   Match! {:#010x} {:#010x}", *val, cmp).unwrap()
                        });
                    } else {
                        uart.lock(|u| {
                            writeln!(u, "\rmismatch! {:#010x} {:#010x}", *val, cmp).unwrap()
                        });
                    }
                }
            }
            if icminterrupt
                .get_rdm_int()
                .intersects(RegionDigestMismatch::R3)
            {
                uart.lock(|u| writeln!(u, "\rRegion3 digest mismatch!",).unwrap());
                // Disable the interrupt
                icm_region3.disable_rdm_int();

                uart.lock(|u| writeln!(u, "\rRegion 3: Expected,  Actual").unwrap());
                for (index, val) in MESSAGE_REF1.iter().enumerate() {
                    let cmp = cx.local.message_region3_sha256[index];
                    if *val == cmp {
                        uart.lock(|u| {
                            writeln!(u, "\r   Match! {:#010x} {:#010x}", *val, cmp).unwrap()
                        });
                    } else {
                        uart.lock(|u| {
                            writeln!(u, "\rmismatch! {:#010x} {:#010x}", *val, cmp).unwrap()
                        });
                    }
                }
            }

            // Get and clear
            let icminterrupt = icm.lock(|i| i.get_interrupt_status());

            let rdm_ints = icminterrupt.get_rdm_int();
            uart.lock(|u| writeln!(u, "\rRDM interrupt vector {:04b}", rdm_ints).unwrap());
        }
    }
}
