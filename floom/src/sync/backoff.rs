pub struct Backoff {
    iteration: usize,
    pub max_spin: usize,
}

impl Backoff {
    pub fn new() -> Self {
        Self {
            iteration: 0,
            max_spin: Self::get_max_spin(),
        }
    }

    pub fn reset(&mut self) {
        self.iteration = 0;
    }

    pub fn spin(&mut self) -> bool {
        if self.iteration < self.max_spin {
            self.iteration += 1;
            if cfg!(unix) {
                std::thread::yield_now();
            } else {
                for _ in 0..(1 << self.iteration).max(100) {
                    std::sync::atomic::spin_loop_hint();
                }
            }
            true
        } else {
            false
        }
    }


    #[cfg(any(
        all(not(windows), not(posix)),
        all(windows, not(any(target_arch = "x86", target_arch = "x86_64"))),
    ))]
    fn get_max_spin() -> usize {
        6
    }

    #[cfg(posix)]
    fn get_max_spin() -> usize {
        40
    }

    #[cfg(all(windows, any(target_arch = "x86", target_arch = "x86_64")))]
    fn get_max_spin() -> usize {
        #[cfg(target_arch = "x86")]
        use std::arch::x86::{__cpuid, CpuidResult};
        #[cfg(target_arch = "x86_64")]
        use std::arch::x86_64::{__cpuid, CpuidResult};

        use std::{
            slice::from_raw_parts,
            str::from_utf8_unchecked,
            hint::unreachable_unchecked,
            sync::atomic::{AtomicUsize, Ordering},
        };

        static IS_AMD: AtomicUsize = AtomicUsize::new(0);
        let is_amd = unsafe {
            match IS_AMD.load(Ordering::Relaxed) {
                0 => {
                    let CpuidResult { ebx, ecx, edx, .. } = __cpuid(0);
                    let vendor = &[ebx, edx, ecx] as *const _ as *const u8;
                    let vendor = from_utf8_unchecked(from_raw_parts(vendor, 3 * 4));
                    let is_amd = vendor == "AuthenticAMD";
                    IS_AMD.store((is_amd as usize) + 1, Ordering::Relaxed);
                    is_amd
                },
                1 => false,
                2 => true,
                _ => unreachable_unchecked(),
            }
        };

        // Assuming AMD = Ryzen,
        // its best to block the thread instead of spinning on cpu.
        if is_amd {
            0
        } else {
            6
        }
    }
}