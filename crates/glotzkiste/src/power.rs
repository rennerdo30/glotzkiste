//! Keeps Windows from slowing the viewer down when it is not in front.
//!
//! Windows 11 puts background processes into power throttling (EcoQoS:
//! lower clocks, efficiency cores) and ignores their timer-resolution
//! requests while their windows are hidden or minimized. A viewer playing
//! dozens of streams behind another window must keep its frame timing, so
//! it opts out of both and asks for 1 ms timers itself (not relying on
//! libmpv doing it). The render threads additionally register with the
//! multimedia scheduler (gk-render).

#[cfg(windows)]
pub fn keep_full_speed() {
    use windows::Win32::Media::timeBeginPeriod;
    use windows::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling,
        SetProcessInformation,
    };
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        // Both mechanisms controlled by us, both off.
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };
    // SAFETY: a valid struct of the size passed, for our own process.
    unsafe {
        if let Err(e) = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const state).cast(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        ) {
            // Older Windows 10 lacks the timer flag; execution speed alone.
            let speed =
                PROCESS_POWER_THROTTLING_STATE { ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED, ..state };
            let again = SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                (&raw const speed).cast(),
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
            log::info!("power throttling opt-out: {e}; execution speed only: {}", again.is_ok());
        }
        // 1 ms timer resolution for the frame timing; ends with the process.
        let _ = timeBeginPeriod(1);
    }
}

#[cfg(not(windows))]
pub fn keep_full_speed() {}
