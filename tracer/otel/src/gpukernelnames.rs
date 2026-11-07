//! Bounded, idle-expiring GPU symbol cache, owned only by the report worker.
use cpp_demangle::{DemangleOptions, ParseOptions, Symbol};
use std::{
    collections::HashMap,
    fmt::{self, Write},
    hash::Hasher,
    sync::Arc,
    time::{Duration, Instant},
};

#[path = "gpukerneldisplay.rs"]
mod display;

const MAX_NAMES: usize = 1024;
const MAX_BYTES: usize = 1024 * 1024;
const MAX_DISPLAY: usize = 4096;
const IDLE: Duration = Duration::from_secs(60);

struct Entry {
    display: Arc<str>,
    used: Instant,
    bytes: usize,
}

#[derive(Default)]
pub(super) struct Cache {
    entries: HashMap<Arc<str>, Entry>,
    bytes: usize,
}

impl Cache {
    pub fn get(&mut self, raw: &Arc<str>, now: Instant) -> Arc<str> {
        if let Some(entry) = self.entries.get_mut(raw.as_ref()) {
            entry.used = now;
            return entry.display.clone();
        }
        // The adapter ABI already bounds names. Do not cache a larger name if
        // an internal caller bypasses it, nor feed oversized input to the parser.
        if raw.len() > super::symbols::MAX_RAW {
            return raw.clone();
        }
        let display = compact(raw).map_or_else(|| raw.clone(), Arc::from);
        let bytes = raw.len()
            + if Arc::ptr_eq(raw, &display) {
                0
            } else {
                display.len()
            };
        while self.entries.len() >= MAX_NAMES || self.bytes + bytes > MAX_BYTES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(name, _)| name.clone())
            else {
                break;
            };
            self.bytes -= self.entries.remove(&oldest).unwrap().bytes;
        }
        self.bytes += bytes;
        self.entries.insert(
            raw.clone(),
            Entry {
                display: display.clone(),
                used: now,
                bytes,
            },
        );
        display
    }

    // Called on each report, including empty ones, so an idle pipeline releases
    // its symbol strings. Report-local pprof frames do not keep this cache alive.
    pub fn expire(&mut self, now: Instant) {
        self.entries.retain(|_, entry| {
            let keep = now.saturating_duration_since(entry.used) < IDLE;
            if !keep {
                self.bytes -= entry.bytes;
            }
            keep
        });
    }
}

struct BoundedName(String);
impl Write for BoundedName {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if text.len() > MAX_DISPLAY.saturating_sub(self.0.len()) {
            return Err(fmt::Error);
        }
        self.0.push_str(text);
        Ok(())
    }
}

fn compact(raw: &str) -> Option<String> {
    if raw.len() > super::symbols::MAX_RAW {
        return None;
    }
    let mut display = if let Some(name) = display::format(raw) {
        BoundedName(name)
    } else {
        if !raw.starts_with("_Z") {
            return None;
        }
        let symbol =
            Symbol::new_with_options(raw, &ParseOptions::default().recursion_limit(64)).ok()?;
        let options = DemangleOptions::default()
            .no_params()
            .no_return_type()
            .recursion_limit(64);
        let mut display = BoundedName(String::new());
        symbol.structured_demangle(&mut display, &options).ok()?;
        display
    };
    if display.0.is_empty() {
        return None;
    }
    // Pyroscope flamegraphs group by display name, ignoring system_name. Keep
    // parameter overloads distinct after omitting arguments. Fixed FNV-1a makes
    // this identifier stable across reports, restarts and Rust versions.
    let mut identity = fnv::FnvHasher::default();
    identity.write(raw.as_bytes());
    write!(display, " [{:016x}]", identity.finish()).ok()?;
    Some(display.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOFTMAX: &str = "_ZN14cuSoftMaxLayer21nchwSoftmaxAxisWSmallI6__halfLj32EEEviPKT_PS2_";
    const DEPTHWISE: &str = "_ZN11cuDepthwise31depthwiseConvHMMAPrefetchKernelI6__halffLi3ELi3ELi1ELi1ELb1EEEvN8nvinfer15Dims4ES3_PKT_NS2_5Dims2ES7_S7_S6_S3_S3_PS4_NS2_2rt14ReducedDivisorESA_SA_iffS6_";

    #[test]
    fn cuda_templates_are_readable_without_return_types_or_arguments() {
        let softmax = compact(SOFTMAX).unwrap();
        assert!(softmax.starts_with("cuSoftMaxLayer::nchwSoftmaxAxisWSmall<__half, "));
        assert!(softmax.contains("32"));
        assert!(!softmax.contains("__half const*"));
        let depthwise = compact(DEPTHWISE).unwrap();
        assert!(depthwise.starts_with(
            "cuDepthwise::depthwiseConvHMMAPrefetchKernel<__half, float, 3, 3, 1, 1, true> ["
        ));
        assert!(!depthwise.contains("nvinfer1::Dims"));
        assert!(depthwise.len() < DEPTHWISE.len());
    }

    #[test]
    fn omitted_overloads_and_template_variants_keep_stable_distinct_names() {
        let int = compact("_Z6kernelPi").unwrap();
        let float = compact("_Z6kernelPf").unwrap();
        assert!(int.starts_with("kernel ["));
        assert!(float.starts_with("kernel ["));
        assert_ne!(int, float);
        assert_eq!(int, compact("_Z6kernelPi").unwrap());
        assert_ne!(
            compact(SOFTMAX),
            compact(&SOFTMAX.replace("Lj32E", "Lj64E"))
        );
    }

    #[test]
    fn generated_invalid_truncated_and_excessive_names_keep_the_raw_symbol() {
        for raw in [
            "generatedNativePointwise",
            "sm80_xmma_fprop_implicit_gemm_f16f16_f16f16",
            "_ZN14cuSoftMaxLayer21nchwSoftmaxAxisWSmallI6__halfLj32",
            "_Zinvalid",
            "",
        ] {
            assert!(compact(raw).is_none(), "{raw}");
        }
        assert!(compact(&format!("_Z{}v", "N".repeat(300))).is_none());
        // Substitutions can expand a short, valid symbol beyond the output cap.
        let expanded = format!("_Z6kernelI100{}{}Evv", "A".repeat(100), "S0_".repeat(48));
        assert!(Symbol::new(&expanded).unwrap().demangle().unwrap().len() > MAX_DISPLAY);
        assert!(compact(&expanded).is_none());
    }

    #[test]
    fn generated_variants_retain_distinct_identities_and_share_the_cache() {
        let raw: Arc<str> = "sm80_xmma_fprop_implicit_gemm_f16f16_f16f16_f16_nhwckrsc_nhwc_tilesize128x32x32_stage4_warpsize4x1x1_g1_tensor16x8x16_simple_t1r1s1_linkable_execute_kernel_trt".into();
        let other: Arc<str> = raw.replace("stage4", "stage3").into();
        let mut cache = Cache::default();
        let now = Instant::now();
        let first = cache.get(&raw, now);
        let second = cache.get(&other, now);
        assert!(first.starts_with("SM80 XMMA"));
        assert_eq!(first.split(" [").next(), second.split(" [").next());
        assert_ne!(first, second);
        assert!(Arc::ptr_eq(&first, &cache.get(&raw, now)));
    }

    #[test]
    fn cached_successes_and_failures_survive_reports_then_expire_when_idle() {
        let mut cache = Cache::default();
        let start = Instant::now();
        let raw: Arc<str> = SOFTMAX.into();
        let first = cache.get(&raw, start);
        cache.expire(start + Duration::from_secs(1));
        assert!(Arc::ptr_eq(
            &first,
            &cache.get(&raw, start + Duration::from_secs(2))
        ));
        let invalid: Arc<str> = "_Zinvalid".into();
        let failed = cache.get(&invalid, start);
        assert!(Arc::ptr_eq(&invalid, &failed));
        assert!(Arc::ptr_eq(
            &failed,
            &cache.get(&Arc::from("_Zinvalid"), start)
        ));
        cache.expire(start + IDLE);
        assert_eq!(cache.entries.len(), 1);
        cache.expire(start + IDLE + Duration::from_secs(2));
        assert!(cache.entries.is_empty());
        assert_eq!(cache.bytes, 0);
    }

    #[test]
    fn capacity_evicts_least_recently_used_names() {
        let mut cache = Cache::default();
        let start = Instant::now();
        for i in 0..MAX_NAMES {
            cache.get(
                &format!("kernel{i}").into(),
                start + Duration::from_millis(i as u64),
            );
        }
        let later = start + Duration::from_secs(2);
        cache.get(&Arc::from("kernel0"), later);
        cache.get(&Arc::from("another"), later);
        assert_eq!(cache.entries.len(), MAX_NAMES);
        assert!(cache.entries.contains_key("kernel0"));
        assert!(!cache.entries.contains_key("kernel1"));
        assert!(cache.bytes <= MAX_BYTES);
    }

    #[test]
    fn expanded_names_hit_the_byte_budget_before_the_entry_budget() {
        let mut cache = Cache::default();
        let start = Instant::now();
        for i in 0..MAX_NAMES {
            let identifier = format!("A{i:04}{}", "x".repeat(95));
            let raw: Arc<str> = format!("_Z6kernelI100{identifier}{}Evv", "S0_".repeat(8)).into();
            let display = cache.get(&raw, start + Duration::from_millis(i as u64));
            assert!(display.len() > 900 && display.len() <= MAX_DISPLAY);
            assert!(cache.bytes <= MAX_BYTES);
        }
        assert!(cache.entries.len() < MAX_NAMES);
        cache.expire(start + IDLE + Duration::from_secs(2));
        assert_eq!(cache.bytes, 0);
        assert!(cache.entries.is_empty());
    }
}
