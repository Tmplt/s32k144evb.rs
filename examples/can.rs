#![feature(used)]
#![no_std]

#[macro_use]
extern crate cortex_m;
extern crate s32k144;
extern crate s32k144evb;
extern crate embedded_types;

use cortex_m::asm;

use s32k144evb::{
    can,
    wdog,
    pc,
};

use s32k144evb::can::{
    ID,
    CanSettings,
};

use embedded_types::can::{
    DataFrame,
    BaseID,
};

fn main() {
    
    let mut wdog_settings = wdog::WatchdogSettings::default();
    wdog_settings.enable = false;
    wdog::configure(wdog_settings);

    let peripherals = unsafe{ s32k144::Peripherals::all() };

    let scg_config = pc::Config{
        system_oscillator: pc::SystemOscillatorInput::Crystal(8_000_000),
        soscdiv2: pc::SystemOscillatorOutput::Div1,
        .. Default::default()
    };
    
    let scg = pc::Pc::init(
        peripherals.SCG,
        peripherals.SMC,
        peripherals.PMC,
        scg_config
    ).unwrap();
    
    let mut can_settings = CanSettings::default();    
    can_settings.self_reception = false;

    // Enable and configure the system oscillator
    let porte = peripherals.PORTE;
    let pcc = peripherals.PCC;
            
    // Configure the can i/o pins
    pcc.pcc_porte.modify(|_, w| w.cgc()._1());
    porte.pcr4.modify(|_, w| w.mux()._101());
    porte.pcr5.modify(|_, w| w.mux()._101());
    
    pcc.pcc_flex_can0.modify(|_, w| w.cgc()._1());
    
    let can = can::Can::init(peripherals.CAN0, &scg, &can_settings).unwrap();

    loop {

        let loop_max = 100000;
        for n in 0..256 {
            let mut message = DataFrame::new(ID::BaseID(BaseID::new(n as u16)));
            message.set_data_length(8);
            for i in 0..8 {
                message.data_as_mut()[i] = i as u8;
            }
            for i in 0..loop_max {
                if i == 0 {
                    can.transmit(&message.into()).unwrap();           
                }
            }
        }
    }
}
