//! Video-decoder load per GPU, from Windows' GPU engine performance
//! counters (`GPU Engine(*engtype_VideoDecode)\Utilization Percentage`,
//! the numbers Task Manager shows), matched to adapters by LUID. Sampled on
//! its own thread every 2 s; reading costs about a millisecond.

use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Gpu {
    pub name: String,
    pub luid: (u32, i32),
    pub vendor: u32,
    /// The busiest video-decode engine, 0..100.
    pub decode_pct: f64,
}

/// Hardware adapters (no software renderer), in DXGI order.
#[cfg(windows)]
pub fn adapters() -> Vec<Gpu> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1};
    let mut out = Vec::new();
    // SAFETY: COM calls with valid out values; adapters are released on drop.
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return out };
        let mut i = 0;
        while let Ok(a) = factory.EnumAdapters1(i) {
            i += 1;
            let Ok(d) = a.GetDesc1() else { continue };
            if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let name = String::from_utf16_lossy(&d.Description).trim_end_matches('\0').to_owned();
            out.push(Gpu {
                name,
                luid: (d.AdapterLuid.LowPart, d.AdapterLuid.HighPart),
                vendor: d.VendorId,
                decode_pct: 0.0,
            });
        }
    }
    out
}

/// `pid_1234_luid_0x00000000_0x0000D1A2_phys_0_eng_3_engtype_VideoDecode`
/// -> (luid, engine), for video-decoding engines only.
pub fn parse_instance(name: &str) -> Option<((u32, i32), u32)> {
    let engtype = name.rsplit("engtype_").next()?.to_ascii_lowercase().replace(' ', "");
    if !name.contains("engtype_") || !(engtype.starts_with("videodecode") || engtype.starts_with("videocodec")) {
        return None;
    }
    let rest = &name[name.find("luid_")? + 5..];
    let mut parts = rest.split('_');
    let high = u32::from_str_radix(parts.next()?.trim_start_matches("0x"), 16).ok()? as i32;
    let low = u32::from_str_radix(parts.next()?.trim_start_matches("0x"), 16).ok()?;
    let eng = rest.split("eng_").nth(1)?.split('_').next()?.parse().ok()?;
    Some(((low, high), eng))
}

#[cfg(windows)]
struct Query {
    query: windows::Win32::System::Performance::PDH_HQUERY,
    counter: windows::Win32::System::Performance::PDH_HCOUNTER,
}

#[cfg(windows)]
impl Query {
    fn open() -> Option<Self> {
        use windows::Win32::System::Performance::*;
        use windows::core::w;
        let mut query = PDH_HQUERY::default();
        let mut counter = PDH_HCOUNTER::default();
        // SAFETY: out pointers are valid; the query is closed on drop.
        unsafe {
            if PdhOpenQueryW(None, 0, &mut query) != 0 {
                return None;
            }
            // Every engine: the video decoder is "VideoDecode" on NVIDIA and
            // Intel, "VideoCodec" on AMD (VCN works both directions).
            if PdhAddEnglishCounterW(query, w!("\\GPU Engine(*)\\Utilization Percentage"), 0, &mut counter) != 0 {
                PdhCloseQuery(query);
                return None;
            }
            PdhCollectQueryData(query);
        }
        Some(Self { query, counter })
    }

    /// Sum over processes per (adapter, engine) of decode utilization.
    fn sample(&self) -> Vec<((u32, i32), u32, f64)> {
        use windows::Win32::System::Performance::*;
        let mut out = Vec::new();
        // SAFETY: the buffer is sized by the first call and holds the items
        // and the strings they point to.
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return out;
            }
            let (mut size, mut count) = (0u32, 0u32);
            let r = PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, None);
            if r != PDH_MORE_DATA || size == 0 {
                return out;
            }
            let mut buf = vec![0u8; size as usize + 64];
            let items = buf.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items)) != 0 {
                return out;
            }
            for i in 0..count as usize {
                let it = &*items.add(i);
                let Ok(name) = it.szName.to_string() else { continue };
                let Some((luid, eng)) = parse_instance(&name) else { continue };
                let v = it.FmtValue.Anonymous.doubleValue;
                match out.iter_mut().find(|(l, e, _): &&mut ((u32, i32), u32, f64)| *l == luid && *e == eng) {
                    Some(x) => x.2 += v,
                    None => out.push((luid, eng, v)),
                }
            }
        }
        out
    }
}

#[cfg(windows)]
impl Drop for Query {
    fn drop(&mut self) {
        // SAFETY: opened in `open`.
        unsafe { windows::Win32::System::Performance::PdhCloseQuery(self.query) };
    }
}

pub type Shared = Arc<Mutex<Vec<Gpu>>>;

/// Starts the monitor thread; the list updates every 2 s.
pub fn start() -> Shared {
    let shared: Shared = Arc::default();
    #[cfg(windows)]
    {
        let s = shared.clone();
        let _ = std::thread::Builder::new().name("gk-gpu-monitor".into()).spawn(move || {
            let list = adapters();
            if let Ok(mut g) = s.lock() {
                *g = list.clone();
            }
            let Some(q) = Query::open() else {
                log::info!("GPU engine counters are not available");
                return;
            };
            loop {
                std::thread::sleep(Duration::from_secs(2));
                let samples = q.sample();
                let mut now = list.clone();
                for g in now.iter_mut() {
                    g.decode_pct = samples
                        .iter()
                        .filter(|(l, _, _)| *l == g.luid)
                        .map(|(_, _, v)| *v)
                        .fold(0.0, f64::max)
                        .min(100.0);
                }
                if let Ok(mut g) = s.lock() {
                    *g = now;
                }
                if Arc::strong_count(&s) == 1 {
                    return; // the app is gone
                }
            }
        });
    }
    shared
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_engine_instance_names() {
        let n = "pid_1234_luid_0x00000000_0x0000D1A2_phys_0_eng_3_engtype_VideoDecode";
        assert_eq!(parse_instance(n), Some(((0xD1A2, 0), 3)));
        assert_eq!(parse_instance("garbage"), None);
        let amd = "pid_8_luid_0x00000000_0x0001F2A3_phys_0_eng_9_engtype_VideoCodec";
        assert_eq!(parse_instance(amd), Some(((0x1F2A3, 0), 9)));
        assert_eq!(parse_instance("pid_8_luid_0x00000000_0x0001F2A3_phys_0_eng_0_engtype_3D"), None);
    }
}
