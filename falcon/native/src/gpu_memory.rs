//! Falcon's own graphics-memory use and the whole card's, measured once a second on Windows.
//!
//! Round 1, step 3 of [the sharing plan](../../../docs/planning/gpu-memory-sharing.md). The adapter
//! is the one Slint renders with: its LUID comes from the wgpu device itself (DirectX 12's
//! `GetAdapterLuid`, Vulkan's `PhysicalDeviceIDProperties`), never from a name or a second adapter
//! request. DXGI then reports Falcon's process use and Windows' allowance for it; the
//! `GPU Adapter Memory` performance counter reports the whole card (every process), which is the
//! number that moves when another application fills the card. A reading that fails is unavailable,
//! never zero. Round 1 only logs these and uses Falcon's own use for the Performance tier's target;
//! it never shrinks a cache in the foreground.
//!
//! Round 2c (the Mac background rule, plan `2026-10-26_mode_control_2b/PLAN.md`): on the Mac the
//! same sampler reads Falcon's footprint (what macOS charges the process, unified memory included),
//! Apple's recommended graphics budget (Metal's `recommendedMaxWorkingSetSize`, once), the memory
//! pressure level, swap in use and the pages swapped out each second, so Turbo's and Custom's share
//! can be kept there too, but only while macOS reports normal pressure and nothing is being swapped.
use crate::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sample {
    pub(crate) at: Instant,
    /// Falcon's process on the card (DXGI LOCAL `CurrentUsage`) and Windows' allowance for it. On
    /// the Mac: Falcon's footprint, and no allowance (0).
    pub(crate) own_local: u64,
    pub(crate) own_budget: u64,
    /// Falcon's use of system memory through the card (NON_LOCAL): diagnostic, never added to the above.
    pub(crate) own_nonlocal: u64,
    /// The whole card's dedicated use across every process; `None` when the counter is unavailable.
    pub(crate) card_used: Option<u64>,
    /// The card's dedicated memory. On the Mac: Apple's recommended graphics budget, 0 when Metal
    /// could not say.
    pub(crate) card_total: u64,
    /// Round 2c: the Mac's own readings; `None` for a Windows reading.
    pub(crate) mac: Option<MacMemory>,
}

/// macOS's memory pressure, as `kern.memorystatus_vm_pressure_level` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PressureLevel {
    Normal,
    Warning,
    Critical,
}

impl PressureLevel {
    /// The sysctl's values (the kernel's `NOTE_MEMORYSTATUS_PRESSURE_*` bits: 1, 2, 4); any other
    /// value is unreadable, never "normal".
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))] // the Mac sampler and the tests read it
    pub(crate) fn from_level(level: i32) -> Option<PressureLevel> {
        match level {
            1 => Some(PressureLevel::Normal),
            2 => Some(PressureLevel::Warning),
            4 => Some(PressureLevel::Critical),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            PressureLevel::Normal => "normal",
            PressureLevel::Warning => "warning",
            PressureLevel::Critical => "critical",
        }
    }
}

/// Round 2c: the Mac's readings beside Falcon's footprint. Each is `None` when it could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MacMemory {
    pub(crate) pressure: Option<PressureLevel>,
    /// Swap in use (`vm.swapusage`), bytes.
    pub(crate) swap_used: Option<u64>,
    /// Pages swapped out across the system since the previous reading (`host_statistics64`'s
    /// `swapouts`, a counter); `None` on the first reading or when either reading failed.
    pub(crate) swapouts: Option<u64>,
}

impl MacMemory {
    /// The owner's option a (26 October): a share is kept only while macOS reports normal pressure
    /// and nothing is being swapped. A reading that is missing does not report that.
    pub(crate) fn calm(&self) -> bool {
        self.pressure == Some(PressureLevel::Normal) && self.swapouts == Some(0)
    }
}

/// The pages swapped out between two readings of the counter; `None` unless both were read.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // the Mac sampler and the tests read it
pub(crate) fn swapouts_since(before: Option<u64>, now: Option<u64>) -> Option<u64> {
    Some(now?.saturating_sub(before?))
}

/// Round 2c: a Mac reading as the log names it; `None` for a Windows reading.
pub(crate) fn describe_mac(s: &Sample) -> Option<String> {
    Some(describe_mac_readings(Some(s.own_local), s.card_total, &s.mac?))
}

/// …and the same from its parts, so a failed footprint read does not hide the other readings.
pub(crate) fn describe_mac_readings(footprint: Option<u64>, budget: u64, m: &MacMemory) -> String {
    let mb = |v: u64| format!("{} MB", v / 1_000_000);
    format!(
        "Falcon's footprint {} · Apple's recommended graphics budget {} · pressure {} · swap used {} · swap-outs {}",
        footprint.map_or_else(|| "unavailable".to_string(), mb),
        if budget > 0 { mb(budget) } else { "unavailable".to_string() },
        m.pressure.map_or("unavailable", PressureLevel::name),
        m.swap_used.map_or_else(|| "unavailable".to_string(), mb),
        m.swapouts.map_or_else(|| "not measured".to_string(), |p| format!("{p} pages in the last second")),
    )
}

/// A reading older than this is ignored (the sampler runs once a second).
const STALE_AFTER: Duration = Duration::from_secs(3);

/// Shared between the sampler thread, the rendering notifier that starts it and the tick.
#[derive(Clone, Default)]
pub(crate) struct GpuMemory {
    latest: Arc<Mutex<Option<Sample>>>,
    started: Arc<AtomicBool>,
}

impl GpuMemory {
    pub(crate) fn latest(&self, now: Instant) -> Option<Sample> {
        let s = (*self.latest.lock().unwrap_or_else(|e| e.into_inner()))?;
        (now.saturating_duration_since(s.at) <= STALE_AFTER).then_some(s)
    }

    /// Start the sampler for the device Slint renders with: Windows', or since round 2c the Mac's
    /// (which needs no device: Metal names its own budget). Once per process.
    pub(crate) fn start(&self, device: &wgpu::Device) {
        if self.started.swap(true, Ordering::Relaxed) {
            return;
        }
        #[cfg(windows)]
        match windows_impl::device_luid(device) {
            Some((high, low)) => {
                let latest = self.latest.clone();
                let spawned = std::thread::Builder::new()
                    .name("falcon-gpu-memory".into())
                    .spawn(move || windows_impl::run(latest, high, low));
                if spawned.is_err() {
                    log_event("graphics memory: the sampler thread could not start — readings unavailable");
                }
            }
            None => log_event("graphics memory: the rendering adapter could not be identified — readings unavailable"),
        }
        #[cfg(target_os = "macos")]
        {
            let latest = self.latest.clone();
            let spawned = std::thread::Builder::new()
                .name("falcon-mac-memory".into())
                .spawn(move || mac_impl::run(latest));
            if spawned.is_err() {
                log_event("memory (Mac): the sampler thread could not start — readings unavailable");
            }
        }
        #[cfg(not(windows))]
        let _ = device;
    }
}

/// The performance-counter instance prefix for an adapter LUID, as Windows names it
/// (`luid_0x00000000_0x000169d6`), so the reading is the rendering adapter's and no other's.
#[cfg_attr(not(windows), allow(dead_code))] // the Windows sampler and the tests read it
pub(crate) fn counter_instance_prefix(high: i32, low: u32) -> String {
    format!("luid_0x{:08x}_0x{:08x}", high as u32, low)
}

/// Vulkan reports a LUID as the Windows structure's eight bytes: `LowPart`, then `HighPart`,
/// both little-endian.
#[cfg_attr(not(windows), allow(dead_code))] // the Windows sampler and the tests read it
pub(crate) fn luid_from_bytes(b: [u8; 8]) -> (i32, u32) {
    (i32::from_le_bytes([b[4], b[5], b[6], b[7]]), u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// The whole-card reading: the sum over every instance of the rendering adapter (one per physical
/// part), and `None` when no instance carries its LUID.
#[cfg_attr(not(windows), allow(dead_code))] // the Windows sampler and the tests read it
pub(crate) fn card_use_for(prefix: &str, instances: &[(String, i64)]) -> Option<u64> {
    let mut total = None;
    for (name, value) in instances {
        if name.to_ascii_lowercase().starts_with(prefix) && *value >= 0 {
            total = Some(total.unwrap_or(0) + *value as u64);
        }
    }
    total
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use windows::Win32::Foundation::LUID;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter3, IDXGIFactory4, DXGI_MEMORY_SEGMENT_GROUP_LOCAL,
        DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO,
    };
    use windows::Win32::System::Performance::{
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
        PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_LARGE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
    };

    /// The rendering device's adapter LUID, from the device itself.
    pub(super) fn device_luid(device: &wgpu::Device) -> Option<(i32, u32)> {
        // SAFETY: read-only queries on the live device's own handles; nothing is retained.
        unsafe {
            if let Some(hal) = device.as_hal::<wgpu::hal::api::Dx12>() {
                let luid = hal.raw_device().GetAdapterLuid();
                return Some((luid.HighPart, luid.LowPart));
            }
            if let Some(hal) = device.as_hal::<wgpu::hal::api::Vulkan>() {
                let mut id = ash::vk::PhysicalDeviceIDProperties::default();
                let mut props = ash::vk::PhysicalDeviceProperties2::default().push_next(&mut id);
                hal.shared_instance().raw_instance().get_physical_device_properties2(hal.raw_physical_device(), &mut props);
                return (id.device_luid_valid == ash::vk::TRUE).then(|| luid_from_bytes(id.device_luid));
            }
        }
        None
    }

    struct CardCounter {
        query: PDH_HQUERY,
        counter: PDH_HCOUNTER,
        prefix: String,
    }

    impl CardCounter {
        fn open(prefix: String) -> Option<CardCounter> {
            let mut query = PDH_HQUERY::default();
            let mut counter = PDH_HCOUNTER::default();
            // SAFETY: plain PDH calls on handles this thread owns; closed in `Drop`.
            unsafe {
                if PdhOpenQueryW(windows::core::PCWSTR::null(), 0, &mut query) != 0 {
                    return None;
                }
                let path = windows::core::w!(r"\GPU Adapter Memory(*)\Dedicated Usage");
                if PdhAddEnglishCounterW(query, path, 0, &mut counter) != 0 {
                    PdhCloseQuery(query);
                    return None;
                }
                PdhCollectQueryData(query); // the first collection primes the query (about 1 s)
            }
            Some(CardCounter { query, counter, prefix })
        }

        fn read(&self) -> Option<u64> {
            let mut size = 0u32;
            let mut count = 0u32;
            // SAFETY: the buffer is sized by PDH's own first call and aligned for its items.
            unsafe {
                if PdhCollectQueryData(self.query) != 0 {
                    return None;
                }
                if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_LARGE, &mut size, &mut count, None)
                    != PDH_MORE_DATA
                {
                    return None;
                }
                let n = (size as usize).div_ceil(std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>());
                let mut items = vec![PDH_FMT_COUNTERVALUE_ITEM_W::default(); n.max(1)];
                if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_LARGE, &mut size, &mut count,
                    Some(items.as_mut_ptr())) != 0
                {
                    return None;
                }
                let rows: Vec<(String, i64)> = items[..count as usize]
                    .iter()
                    .filter(|it| it.FmtValue.CStatus == 0 && !it.szName.is_null())
                    .map(|it| (it.szName.to_string().unwrap_or_default(), it.FmtValue.Anonymous.largeValue))
                    .collect();
                card_use_for(&self.prefix, &rows)
            }
        }
    }

    impl Drop for CardCounter {
        fn drop(&mut self) {
            // SAFETY: the query was opened by `open` and is closed once.
            unsafe { PdhCloseQuery(self.query) };
        }
    }

    fn own_use(adapter: &IDXGIAdapter3, local: bool) -> Option<DXGI_QUERY_VIDEO_MEMORY_INFO> {
        let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let group = if local { DXGI_MEMORY_SEGMENT_GROUP_LOCAL } else { DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL };
        // SAFETY: a query into a stack structure on an adapter this thread owns.
        unsafe { adapter.QueryVideoMemoryInfo(0, group, &mut info) }.ok().map(|()| info)
    }

    /// The sampler: once a second, for the life of the process.
    pub(super) fn run(latest: Arc<Mutex<Option<Sample>>>, high: i32, low: u32) {
        let luid = LUID { LowPart: low, HighPart: high };
        // SAFETY: DXGI factory and adapter lookups; both are owned by this thread.
        let adapter: Option<IDXGIAdapter3> = unsafe {
            CreateDXGIFactory1::<IDXGIFactory4>().ok().and_then(|f| f.EnumAdapterByLuid::<IDXGIAdapter3>(luid).ok())
        };
        let Some(adapter) = adapter else {
            log_event("graphics memory: DXGI has no adapter with the rendering device's LUID — readings unavailable");
            return;
        };
        let card_total = unsafe { adapter.GetDesc1() }.map(|d| d.DedicatedVideoMemory as u64).unwrap_or(0);
        let prefix = counter_instance_prefix(high, low);
        let counter = CardCounter::open(prefix.clone());
        log_event(&format!(
            "graphics memory: measuring the rendering adapter ({prefix}, {} MB); whole-card counter {}",
            card_total / 1_000_000,
            if counter.is_some() { "available" } else { "unavailable" }
        ));
        loop {
            let sample = own_use(&adapter, true).map(|l| Sample {
                at: Instant::now(),
                own_local: l.CurrentUsage,
                own_budget: l.Budget,
                own_nonlocal: own_use(&adapter, false).map_or(0, |n| n.CurrentUsage),
                card_used: counter.as_ref().and_then(CardCounter::read),
                card_total,
                mac: None,
            });
            *latest.lock().unwrap_or_else(|e| e.into_inner()) = sample;
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

/// Round 2c: the Mac's readings, all through `libc`'s own declarations except the host port.
#[cfg(target_os = "macos")]
mod mac_impl {
    use super::*;
    use std::ffi::c_int;

    extern "C" {
        // libc deprecates its binding in favour of the mach2 crate, which Falcon does not use;
        // falcon-decode's `avail_ram_bytes` declares it the same way.
        fn mach_host_self() -> libc::mach_port_t;
    }

    /// Falcon's footprint: what macOS charges this process, its unified memory included (Activity
    /// Monitor's "Memory" column).
    fn footprint() -> Option<u64> {
        // SAFETY: a zeroed plain-data structure, filled for this process by the flavour that names
        // its type.
        let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::proc_pid_rusage(
                std::process::id() as c_int,
                libc::RUSAGE_INFO_V2,
                (&mut info as *mut libc::rusage_info_v2).cast::<libc::rusage_info_t>(),
            )
        };
        (rc == 0 && info.ri_phys_footprint > 0).then_some(info.ri_phys_footprint)
    }

    /// One fixed-size sysctl value; `None` unless the kernel wrote exactly its size.
    fn sysctl<T: Copy>(name: &[u8]) -> Option<T> {
        let mut value = std::mem::MaybeUninit::<T>::zeroed();
        let mut len = std::mem::size_of::<T>();
        // SAFETY: `name` is NUL-terminated and the buffer holds `len` bytes of `T`.
        let rc = unsafe {
            libc::sysctlbyname(name.as_ptr().cast(), value.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0)
        };
        // SAFETY: zero-initialised, then written in full by the kernel (checked by `len`).
        (rc == 0 && len == std::mem::size_of::<T>()).then(|| unsafe { value.assume_init() })
    }

    fn pressure() -> Option<PressureLevel> {
        sysctl::<c_int>(b"kern.memorystatus_vm_pressure_level\0").and_then(PressureLevel::from_level)
    }

    fn swap_used() -> Option<u64> {
        sysctl::<libc::xsw_usage>(b"vm.swapusage\0").map(|x| x.xsu_used)
    }

    /// The system's swap-out counter, in pages.
    fn swapouts() -> Option<u64> {
        // One send right for the life of the process (each `mach_host_self` call makes another).
        static HOST: std::sync::OnceLock<libc::mach_port_t> = std::sync::OnceLock::new();
        // SAFETY: a plain mach call with no arguments.
        let host = *HOST.get_or_init(|| unsafe { mach_host_self() });
        // SAFETY: a zeroed plain-data structure of the flavour's own type, and its size in integers.
        let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let rc = unsafe {
            libc::host_statistics64(host, libc::HOST_VM_INFO64, (&mut stats as *mut libc::vm_statistics64).cast(), &mut count)
        };
        let total = stats.swapouts; // a copy out of the packed structure
        (rc == libc::KERN_SUCCESS).then_some(total)
    }

    /// The sampler: once a second, for the life of the process. Its start line and its first two
    /// readings are logged at once, so even a launch of a few seconds (the cloud run's checks) shows
    /// that every reading works: the first carries the raw swap-out counter (it has nothing to
    /// compare with yet), the second the first measured second (review of the build, M1/F1).
    pub(super) fn run(latest: Arc<Mutex<Option<Sample>>>) {
        let budget = crate::metal_recommended_working_set();
        log_event(&format!(
            "memory (Mac): measuring Falcon's footprint, memory pressure and swap once a second; Apple's \
             recommended graphics budget {}",
            budget.map_or_else(|| "unavailable".to_string(), |b| format!("{} MB", b / 1_000_000))
        ));
        let mut last_swapouts = None;
        let mut logged = 0u8;
        loop {
            let total = swapouts();
            let mac = MacMemory { pressure: pressure(), swap_used: swap_used(), swapouts: swapouts_since(last_swapouts, total) };
            if total.is_some() {
                last_swapouts = total;
            }
            let own = footprint();
            if logged < 2 {
                logged += 1;
                let readings = describe_mac_readings(own, budget.unwrap_or(0), &mac);
                log_event(&if logged == 1 {
                    format!(
                        "memory (Mac): first reading — {readings}; swap-out counter {}",
                        total.map_or_else(|| "unavailable".to_string(), |t| format!("{t} pages"))
                    )
                } else {
                    format!("memory (Mac): second reading — {readings}")
                });
            }
            let sample = own.map(|own| Sample {
                at: Instant::now(),
                own_local: own,
                own_budget: 0,
                own_nonlocal: 0,
                card_used: None,
                card_total: budget.unwrap_or(0),
                mac: Some(mac),
            });
            *latest.lock().unwrap_or_else(|e| e.into_inner()) = sample;
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fails if the LUID's halves are swapped or byte order reversed: the 25 October probe read the
    /// RTX 5090 as `00000000:000165d9` through DirectX 12 and as these bytes through Vulkan.
    #[test]
    fn a_vulkan_luid_names_the_same_counter_instance_as_directx() {
        let (high, low) = luid_from_bytes([0xd9, 0x65, 0x01, 0x00, 0, 0, 0, 0]);
        assert_eq!((high, low), (0, 0x0001_65d9));
        assert_eq!(counter_instance_prefix(high, low), "luid_0x00000000_0x000165d9");
        assert_eq!(counter_instance_prefix(-1, 1), "luid_0xffffffff_0x00000001");
    }

    /// Fails if another adapter's instance is counted, or if an absent adapter reads as zero.
    #[test]
    fn the_card_reading_counts_only_the_rendering_adapter() {
        let rows = vec![
            ("luid_0x00000000_0x000169d6_phys_0".to_string(), 3_000),
            ("LUID_0x00000000_0x000169D6_phys_1".to_string(), 500),
            ("luid_0x00000000_0x00017f01_phys_0".to_string(), 9_999),
        ];
        assert_eq!(card_use_for("luid_0x00000000_0x000169d6", &rows), Some(3_500));
        assert_eq!(card_use_for("luid_0x00000000_0x00012345", &rows), None);
    }

    /// Fails if a stale reading is served as current.
    #[test]
    fn an_old_reading_is_not_served() {
        let m = GpuMemory::default();
        let t0 = Instant::now();
        *m.latest.lock().unwrap() = Some(Sample { at: t0, own_local: 1, own_budget: 2, own_nonlocal: 0,
            card_used: None, card_total: 3, mac: None });
        assert!(m.latest(t0 + Duration::from_secs(2)).is_some());
        assert!(m.latest(t0 + Duration::from_secs(4)).is_none());
    }

    /// Round 2c. FALSIFIERS (L28): map an unknown level to "normal", or swap two levels, and the
    /// first row reddens; let a missing reading count as no swapping and the second does; drop a
    /// reading from the log line and the third does.
    #[test]
    fn the_mac_readings_are_read_as_macos_reports_them() {
        assert_eq!(PressureLevel::from_level(1), Some(PressureLevel::Normal));
        assert_eq!(PressureLevel::from_level(2), Some(PressureLevel::Warning));
        assert_eq!(PressureLevel::from_level(4), Some(PressureLevel::Critical));
        for unknown in [0, 3, 8, -1] {
            assert_eq!(PressureLevel::from_level(unknown), None, "{unknown} is unreadable, never normal");
        }
        assert_eq!(swapouts_since(Some(5), Some(9)), Some(4));
        assert_eq!(swapouts_since(Some(9), Some(9)), Some(0), "nothing swapped this second");
        assert_eq!(swapouts_since(None, Some(9)), None, "the first reading has nothing to compare with");
        assert_eq!(swapouts_since(Some(5), None), None);
        let s = Sample { at: Instant::now(), own_local: 2_500_000_000, own_budget: 0, own_nonlocal: 0, card_used: None,
            card_total: 21_000_000_000,
            mac: Some(MacMemory { pressure: Some(PressureLevel::Warning), swap_used: Some(1_000_000_000), swapouts: Some(12) }) };
        assert_eq!(
            describe_mac(&s).as_deref(),
            Some("Falcon's footprint 2500 MB · Apple's recommended graphics budget 21000 MB · pressure warning · swap used 1000 MB · swap-outs 12 pages in the last second")
        );
        let unread = Sample { mac: Some(MacMemory { pressure: None, swap_used: None, swapouts: None }), card_total: 0, ..s };
        assert_eq!(
            describe_mac(&unread).as_deref(),
            Some("Falcon's footprint 2500 MB · Apple's recommended graphics budget unavailable · pressure unavailable · swap used unavailable · swap-outs not measured")
        );
        assert_eq!(describe_mac(&Sample { mac: None, ..s }), None, "a Windows reading is described as before");
        // A failed footprint read does not hide the other readings (the sampler's first lines).
        assert_eq!(
            describe_mac_readings(None, 21_000_000_000, &s.mac.unwrap()),
            "Falcon's footprint unavailable · Apple's recommended graphics budget 21000 MB · pressure warning · swap used 1000 MB · swap-outs 12 pages in the last second"
        );
    }
}
