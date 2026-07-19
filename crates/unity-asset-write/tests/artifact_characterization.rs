use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;
use unity_asset_binary::bundle::{BundleLoadOptions, BundleParser};
use unity_asset_binary::webfile::WebFile;
use unity_asset_core::{AssetLoadBudget, DigestV1};
use unity_asset_write::PackingPolicy;
use unity_asset_write::bundle::{BundleEdits, BundleWriter};
use unity_asset_write::webfile::{WebFileEdits, WebFilePackingPolicy, WebFileWriter};

struct SamplingAllocator;

static SAMPLE_ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static SAMPLE_ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static SAMPLE_ACTIVE: AtomicBool = AtomicBool::new(false);

#[global_allocator]
static GLOBAL_ALLOCATOR: SamplingAllocator = SamplingAllocator;

// The sampler counts allocation requests only while one ignored characterization is active.
// Deallocations are deliberately excluded: peak process RSS is sampled separately, while this
// monotonic counter remains deterministic enough to compare allocation churn between adapters.
unsafe impl GlobalAlloc for SamplingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: Delegating the unchanged layout to the process allocator preserves its contract.
        let pointer = unsafe { System.alloc(layout) };
        record_allocation(pointer, layout.size());
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: Delegating the unchanged layout to the process allocator preserves its contract.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        record_allocation(pointer, layout.size());
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: The pointer and layout came from this allocator's System allocation methods.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: The pointer/layout pair came from System and new_size is forwarded unchanged.
        let pointer = unsafe { System.realloc(pointer, layout, new_size) };
        record_allocation(pointer, new_size);
        pointer
    }
}

fn record_allocation(pointer: *mut u8, bytes: usize) {
    if pointer.is_null() || !SAMPLE_ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    SAMPLE_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    SAMPLE_ALLOCATED_BYTES.fetch_add(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy)]
struct AllocationSample {
    allocations: u64,
    allocated_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct ProcessSample {
    cpu_time: Option<Duration>,
    peak_rss_bytes: Option<u64>,
}

impl ProcessSample {
    fn capture() -> Self {
        platform_process_sample().unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy)]
struct RuntimeSample {
    elapsed: Duration,
    process_before: ProcessSample,
    process_after: ProcessSample,
    allocations: AllocationSample,
}

fn sample<T>(operation: impl FnOnce() -> T) -> (T, RuntimeSample) {
    let process_before = ProcessSample::capture();
    SAMPLE_ALLOCATIONS.store(0, Ordering::Relaxed);
    SAMPLE_ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    SAMPLE_ACTIVE.store(true, Ordering::Release);
    let started = Instant::now();
    let output = operation();
    let elapsed = started.elapsed();
    SAMPLE_ACTIVE.store(false, Ordering::Release);
    let process_after = ProcessSample::capture();
    let allocations = AllocationSample {
        allocations: SAMPLE_ALLOCATIONS.load(Ordering::Relaxed),
        allocated_bytes: SAMPLE_ALLOCATED_BYTES.load(Ordering::Relaxed),
    };
    (
        output,
        RuntimeSample {
            elapsed,
            process_before,
            process_after,
            allocations,
        },
    )
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root should be two levels above unity-asset-write")
        .to_path_buf()
}

fn build_uncompressed_webfile(entries: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    let signature = b"UnityWebData1.0\0";
    let entry_table_len = entries
        .iter()
        .map(|(name, _)| 12_usize.saturating_add(name.len()))
        .sum::<usize>();
    let header_len = signature
        .len()
        .saturating_add(std::mem::size_of::<i32>())
        .saturating_add(entry_table_len);
    let mut encoded = Vec::with_capacity(
        header_len.saturating_add(entries.iter().map(|(_, bytes)| bytes.len()).sum::<usize>()),
    );
    encoded.extend_from_slice(signature);
    encoded.extend_from_slice(
        &i32::try_from(header_len)
            .expect("characterization WebFile header fits i32")
            .to_le_bytes(),
    );

    let mut cursor = header_len;
    for (name, bytes) in &entries {
        encoded.extend_from_slice(
            &i32::try_from(cursor)
                .expect("characterization WebFile offset fits i32")
                .to_le_bytes(),
        );
        encoded.extend_from_slice(
            &i32::try_from(bytes.len())
                .expect("characterization WebFile entry fits i32")
                .to_le_bytes(),
        );
        encoded.extend_from_slice(
            &i32::try_from(name.len())
                .expect("characterization WebFile name fits i32")
                .to_le_bytes(),
        );
        encoded.extend_from_slice(name.as_bytes());
        cursor = cursor
            .checked_add(bytes.len())
            .expect("characterization WebFile offset does not overflow");
    }
    for (_, bytes) in entries {
        encoded.extend_from_slice(&bytes);
    }
    encoded
}

fn emit_sample(
    fixture: &str,
    input_bytes: u64,
    source_bytes_read: u64,
    decompressed_bytes: u64,
    legacy_materializations: u64,
    output_bytes: u64,
    output_digest: DigestV1,
    runtime: RuntimeSample,
) {
    let cpu_ns = runtime
        .process_after
        .cpu_time
        .zip(runtime.process_before.cpu_time)
        .map(|(after, before)| duration_ns(after.saturating_sub(before)));
    let peak_rss_growth = runtime
        .process_after
        .peak_rss_bytes
        .zip(runtime.process_before.peak_rss_bytes)
        .map(|(after, before)| after.saturating_sub(before));
    println!(
        "{}",
        json!({
            "schema": "unity-asset.prepared-artifact-characterization.v1",
            "implementation": "legacy-contiguous-writers",
            "fixture": fixture,
            "input_bytes": input_bytes,
            "output_bytes": output_bytes,
            "output_digest": output_digest,
            "wall_time_ns": duration_ns(runtime.elapsed),
            "process_cpu_ns": cpu_ns,
            "peak_rss_before_bytes": runtime.process_before.peak_rss_bytes,
            "peak_rss_after_bytes": runtime.process_after.peak_rss_bytes,
            "peak_rss_growth_bytes": peak_rss_growth,
            "allocation_requests": runtime.allocations.allocations,
            "allocated_request_bytes": runtime.allocations.allocated_bytes,
            "source_bytes_read": source_bytes_read,
            "decompressed_bytes": decompressed_bytes,
            "known_image_materializations": legacy_materializations,
        })
    );
}

#[test]
fn legacy_characterization_fixture_contract_is_reproducible() {
    let input = build_uncompressed_webfile(vec![
        ("a.bin".to_owned(), vec![1; 17]),
        ("b.bin".to_owned(), vec![2; 31]),
        ("c.bin".to_owned(), vec![3; 47]),
    ]);
    let web = WebFile::from_bytes(input.clone()).expect("fixture should parse");
    let output = WebFileWriter::save(
        &web,
        &WebFileEdits::default(),
        WebFilePackingPolicy::Uncompressed,
    )
    .expect("legacy writer should encode fixture");
    let reparsed = WebFile::from_bytes(output.clone()).expect("legacy output should reparse");

    assert_eq!(reparsed.files().len(), 3);
    assert_eq!(reparsed.extract_file_slice("a.bin").unwrap(), &[1; 17]);
    assert_eq!(reparsed.extract_file_slice("b.bin").unwrap(), &[2; 31]);
    assert_eq!(reparsed.extract_file_slice("c.bin").unwrap(), &[3; 47]);
    assert_eq!(output, input);
    assert_eq!(legacy_webfile_materializations(&web), 4);
}

#[test]
#[ignore = "opt-in release characterization; emits allocation, timing, and process observations"]
fn prepared_artifact_legacy_sample_representative() {
    let input = std::fs::read(repo_root().join("tests/samples/char_118_yuki.ab"))
        .expect("representative bundle fixture should exist");
    let input_len = u64::try_from(input.len()).unwrap();
    let (summary, runtime) = sample(|| {
        let mut load_budget = AssetLoadBudget::default();
        let bundle = BundleParser::from_bytes_with_options_and_budget(
            input,
            BundleLoadOptions::complete(),
            &mut load_budget,
        )
        .expect("representative bundle should parse completely");
        let source_payload_bytes = bundle
            .nodes
            .iter()
            .filter(|node| node.is_file())
            .map(|node| node.size)
            .sum::<u64>();
        let file_count =
            u64::try_from(bundle.nodes.iter().filter(|node| node.is_file()).count()).unwrap();
        let output = BundleWriter::save(&bundle, &BundleEdits::default(), PackingPolicy::Preserve)
            .expect("legacy bundle writer should encode representative fixture");
        let summary = (
            u64::try_from(output.len()).unwrap(),
            DigestV1::hash_bytes(&output),
            input_len.saturating_add(source_payload_bytes),
            load_budget.usage().decompressed_bytes,
            file_count.saturating_add(3),
        );
        drop(output);
        summary
    });
    emit_sample(
        "representative-unityfs",
        input_len,
        summary.2,
        summary.3,
        summary.4,
        summary.0,
        summary.1,
        runtime,
    );
}

#[test]
#[ignore = "opt-in release characterization; emits allocation, timing, and process observations"]
fn prepared_artifact_legacy_sample_generated_large() {
    const ENTRY_COUNT: usize = 32;
    const ENTRY_BYTES: usize = 1024 * 1024;
    let entries = (0..ENTRY_COUNT)
        .map(|index| {
            (
                format!("generated/{index:04}.bin"),
                vec![u8::try_from(index).unwrap(); ENTRY_BYTES],
            )
        })
        .collect::<Vec<_>>();
    let input = build_uncompressed_webfile(entries);
    let input_len = u64::try_from(input.len()).unwrap();
    let (summary, runtime) = sample(|| {
        let mut load_budget = AssetLoadBudget::default();
        let web = WebFile::from_bytes_with_budget(input, &mut load_budget)
            .expect("generated large WebFile should parse");
        let source_payload_bytes = web.files().iter().map(|entry| entry.size).sum::<u64>();
        let output = WebFileWriter::save(
            &web,
            &WebFileEdits::default(),
            WebFilePackingPolicy::Uncompressed,
        )
        .expect("legacy WebFile writer should encode generated fixture");
        let summary = (
            u64::try_from(output.len()).unwrap(),
            DigestV1::hash_bytes(&output),
            input_len.saturating_add(source_payload_bytes),
            load_budget.usage().decompressed_bytes,
            legacy_webfile_materializations(&web),
        );
        drop(output);
        summary
    });
    emit_sample(
        "generated-large-webfile",
        input_len,
        summary.2,
        summary.3,
        summary.4,
        summary.0,
        summary.1,
        runtime,
    );
}

#[test]
#[ignore = "opt-in release characterization; emits allocation, timing, and process observations"]
fn prepared_artifact_legacy_sample_adversarial_wide() {
    const ENTRY_COUNT: usize = 8_192;
    let entries = (0..ENTRY_COUNT)
        .map(|index| {
            (
                format!("wide/{index:08}/member-{index:08}.bin"),
                vec![u8::try_from(index % 251).unwrap(); 17],
            )
        })
        .collect::<Vec<_>>();
    let input = build_uncompressed_webfile(entries);
    let input_len = u64::try_from(input.len()).unwrap();
    let (summary, runtime) = sample(|| {
        let mut load_budget = AssetLoadBudget::default();
        let web = WebFile::from_bytes_with_budget(input, &mut load_budget)
            .expect("adversarial wide WebFile should parse");
        let source_payload_bytes = web.files().iter().map(|entry| entry.size).sum::<u64>();
        let output = WebFileWriter::save(
            &web,
            &WebFileEdits::default(),
            WebFilePackingPolicy::Uncompressed,
        )
        .expect("legacy WebFile writer should encode adversarial fixture");
        let summary = (
            u64::try_from(output.len()).unwrap(),
            DigestV1::hash_bytes(&output),
            input_len.saturating_add(source_payload_bytes),
            load_budget.usage().decompressed_bytes,
            legacy_webfile_materializations(&web),
        );
        drop(output);
        summary
    });
    emit_sample(
        "adversarial-wide-webfile",
        input_len,
        summary.2,
        summary.3,
        summary.4,
        summary.0,
        summary.1,
        runtime,
    );
}

fn legacy_webfile_materializations(web: &WebFile) -> u64 {
    u64::try_from(web.files().len())
        .unwrap_or(u64::MAX)
        .saturating_add(1)
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(target_os = "windows")]
fn platform_process_sample() -> Option<ProcessSample> {
    use std::ffi::c_void;
    use std::mem::size_of;

    type Handle = *mut c_void;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "GetCurrentProcess"]
        fn get_current_process() -> Handle;
        #[link_name = "GetProcessTimes"]
        fn get_process_times(
            process: Handle,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }

    #[link(name = "psapi")]
    unsafe extern "system" {
        #[link_name = "GetProcessMemoryInfo"]
        fn get_process_memory_info(
            process: Handle,
            counters: *mut ProcessMemoryCounters,
            size: u32,
        ) -> i32;
    }

    let mut creation = FileTime::default();
    let mut exit = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    let mut memory = ProcessMemoryCounters {
        cb: u32::try_from(size_of::<ProcessMemoryCounters>()).ok()?,
        ..ProcessMemoryCounters::default()
    };
    let memory_size = memory.cb;

    // SAFETY: The pseudo-handle is process-local and every output pointer refers to an initialized
    // C-layout value that remains alive for both calls. The pseudo-handle must not be closed.
    let success = unsafe {
        let process = get_current_process();
        get_process_times(process, &mut creation, &mut exit, &mut kernel, &mut user) != 0
            && get_process_memory_info(process, &mut memory, memory_size) != 0
    };
    if !success {
        return None;
    }

    let kernel_ticks = (u64::from(kernel.high) << 32) | u64::from(kernel.low);
    let user_ticks = (u64::from(user.high) << 32) | u64::from(user.low);
    Some(ProcessSample {
        cpu_time: Some(Duration::from_nanos(
            kernel_ticks.saturating_add(user_ticks).saturating_mul(100),
        )),
        peak_rss_bytes: u64::try_from(memory.peak_working_set_size).ok(),
    })
}

#[cfg(target_os = "linux")]
fn platform_process_sample() -> Option<ProcessSample> {
    use std::process::Command;
    use std::sync::OnceLock;

    static TICKS_PER_SECOND: OnceLock<Option<u64>> = OnceLock::new();
    let ticks_per_second = TICKS_PER_SECOND
        .get_or_init(|| {
            Command::new("getconf")
                .arg("CLK_TCK")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
        })
        .as_ref()
        .copied()?;
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let fields = stat
        .get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .collect::<Vec<_>>();
    let ticks = fields
        .get(11)?
        .parse::<u64>()
        .ok()?
        .saturating_add(fields.get(12)?.parse::<u64>().ok()?);
    let cpu_ns = u128::from(ticks)
        .saturating_mul(1_000_000_000)
        .checked_div(u128::from(ticks_per_second))?;
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let peak_rss_kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    Some(ProcessSample {
        cpu_time: Some(Duration::from_nanos(
            u64::try_from(cpu_ns).unwrap_or(u64::MAX),
        )),
        peak_rss_bytes: peak_rss_kib.checked_mul(1024),
    })
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn platform_process_sample() -> Option<ProcessSample> {
    None
}
