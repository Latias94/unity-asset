use std::ffi::c_void;
use std::io::{self, Write};
use std::mem::size_of;
use std::pin::pin;
use std::ptr::{self, NonNull};

use super::{ArtifactBudgetError, ArtifactBuildError, CodecScratchBudget, CodecScratchLease};

#[allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]
mod brotli_sys {
    use std::ffi::{c_int, c_void};

    pub type BrotliEncoderOperation = c_int;
    pub const BrotliEncoderOperation_BROTLI_OPERATION_PROCESS: BrotliEncoderOperation = 0;
    pub const BrotliEncoderOperation_BROTLI_OPERATION_FLUSH: BrotliEncoderOperation = 1;
    pub const BrotliEncoderOperation_BROTLI_OPERATION_FINISH: BrotliEncoderOperation = 2;

    pub type BrotliEncoderParameter = c_int;
    pub const BrotliEncoderParameter_BROTLI_PARAM_QUALITY: BrotliEncoderParameter = 1;
    pub const BrotliEncoderParameter_BROTLI_PARAM_LGWIN: BrotliEncoderParameter = 2;
    pub const BrotliEncoderParameter_BROTLI_PARAM_SIZE_HINT: BrotliEncoderParameter = 5;

    pub type BrotliAlloc = unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void;
    pub type BrotliFree = unsafe extern "C" fn(*mut c_void, *mut c_void);

    #[repr(C)]
    pub struct BrotliEncoderState {
        _private: [u8; 0],
    }

    unsafe extern "C" {
        #[link_name = "unity_asset_write_brotli_1_2_0_BrotliEncoderCreateInstance"]
        pub fn BrotliEncoderCreateInstance(
            alloc: Option<BrotliAlloc>,
            free: Option<BrotliFree>,
            opaque: *mut c_void,
        ) -> *mut BrotliEncoderState;
        #[link_name = "unity_asset_write_brotli_1_2_0_BrotliEncoderDestroyInstance"]
        pub fn BrotliEncoderDestroyInstance(state: *mut BrotliEncoderState);
        #[link_name = "unity_asset_write_brotli_1_2_0_BrotliEncoderSetParameter"]
        pub fn BrotliEncoderSetParameter(
            state: *mut BrotliEncoderState,
            parameter: BrotliEncoderParameter,
            value: u32,
        ) -> c_int;
        #[link_name = "unity_asset_write_brotli_1_2_0_BrotliEncoderCompressStream"]
        pub fn BrotliEncoderCompressStream(
            state: *mut BrotliEncoderState,
            operation: BrotliEncoderOperation,
            available_input: *mut usize,
            next_input: *mut *const u8,
            available_output: *mut usize,
            next_output: *mut *mut u8,
            total_output: *mut usize,
        ) -> c_int;
        #[link_name = "unity_asset_write_brotli_1_2_0_BrotliEncoderHasMoreOutput"]
        pub fn BrotliEncoderHasMoreOutput(state: *const BrotliEncoderState) -> c_int;
        #[link_name = "unity_asset_write_brotli_1_2_0_BrotliEncoderIsFinished"]
        pub fn BrotliEncoderIsFinished(state: *const BrotliEncoderState) -> c_int;
        #[link_name = "unity_asset_write_brotli_1_2_0_BrotliEncoderMaxCompressedSize"]
        pub fn BrotliEncoderMaxCompressedSize(input_size: usize) -> usize;
    }
}

const BROTLI_QUALITY: u32 = 11;
const BROTLI_LGWIN: u32 = 22;
const OUTPUT_BUFFER_BYTES: usize = 4096;
const ALLOCATION_ALIGNMENT: usize = 64;

/// Streams Brotli output through the caller's writer while every native allocation is charged to
/// the active artifact transaction.
pub(crate) fn encode_brotli<W, F>(
    scratch: CodecScratchBudget,
    output: W,
    input_len: u64,
    encode: F,
) -> Result<W, ArtifactBuildError>
where
    W: Write,
    F: FnOnce(&mut dyn Write) -> Result<(), ArtifactBuildError>,
{
    encode_brotli_inner(scratch, output, input_len, None, encode).map(|(output, _)| output)
}

fn encode_brotli_inner<W, F>(
    scratch: CodecScratchBudget,
    mut output: W,
    input_len: u64,
    fail_allocation: Option<u64>,
    encode: F,
) -> Result<(W, u64), ArtifactBuildError>
where
    W: Write,
    F: FnOnce(&mut dyn Write) -> Result<(), ArtifactBuildError>,
{
    let input_len =
        usize::try_from(input_len).map_err(|_| ArtifactBudgetError::ArithmeticOverflow {
            resource: "Brotli input length",
        })?;
    // A zero result for nonempty input is the native API's overflow sentinel.
    let maximum_output = unsafe { brotli_sys::BrotliEncoderMaxCompressedSize(input_len) };
    if input_len != 0 && maximum_output == 0 {
        return Err(ArtifactBudgetError::ArithmeticOverflow {
            resource: "Brotli maximum output length",
        }
        .into());
    }

    let output_lease =
        scratch.try_reserve(u64::try_from(OUTPUT_BUFFER_BYTES).map_err(|_| {
            ArtifactBudgetError::ArithmeticOverflow {
                resource: "Brotli output buffer",
            }
        })?)?;
    let mut allocator = pin!(BrotliAllocatorState::new(scratch, fail_allocation,));
    let opaque = allocator.as_mut().get_mut() as *mut BrotliAllocatorState as *mut c_void;
    let state = unsafe {
        brotli_sys::BrotliEncoderCreateInstance(Some(brotli_allocate), Some(brotli_free), opaque)
    };
    let Some(state) = NonNull::new(state) else {
        drop(output_lease);
        return Err(take_allocator_failure(&mut allocator).unwrap_or(
            ArtifactBuildError::CodecFailure {
                codec: "Brotli",
                operation: "create encoder state",
            },
        ));
    };
    let state = NativeEncoderState(state);

    let configured = unsafe {
        brotli_sys::BrotliEncoderSetParameter(
            state.0.as_ptr(),
            brotli_sys::BrotliEncoderParameter_BROTLI_PARAM_QUALITY,
            BROTLI_QUALITY,
        ) != 0
            && brotli_sys::BrotliEncoderSetParameter(
                state.0.as_ptr(),
                brotli_sys::BrotliEncoderParameter_BROTLI_PARAM_LGWIN,
                BROTLI_LGWIN,
            ) != 0
            && (input_len > u32::MAX as usize
                || brotli_sys::BrotliEncoderSetParameter(
                    state.0.as_ptr(),
                    brotli_sys::BrotliEncoderParameter_BROTLI_PARAM_SIZE_HINT,
                    input_len as u32,
                ) != 0)
    };

    let encode_result = if configured {
        let mut writer = NativeBrotliWriter::new(state.0, &mut output);
        encode(&mut writer).and_then(|()| writer.finish())
    } else {
        Err(ArtifactBuildError::CodecFailure {
            codec: "Brotli",
            operation: "configure encoder",
        })
    };

    drop(state);
    drop(output_lease);
    let allocation_attempts = allocator.allocation_attempts;
    if let Some(error) = take_allocator_failure(&mut allocator) {
        return Err(error);
    }
    encode_result?;
    Ok((output, allocation_attempts))
}

struct NativeEncoderState(NonNull<brotli_sys::BrotliEncoderState>);

impl Drop for NativeEncoderState {
    fn drop(&mut self) {
        unsafe { brotli_sys::BrotliEncoderDestroyInstance(self.0.as_ptr()) };
    }
}

struct NativeBrotliWriter<'output, W> {
    state: NonNull<brotli_sys::BrotliEncoderState>,
    output: &'output mut W,
    buffer: [u8; OUTPUT_BUFFER_BYTES],
    finished: bool,
}

impl<'output, W: Write> NativeBrotliWriter<'output, W> {
    fn new(state: NonNull<brotli_sys::BrotliEncoderState>, output: &'output mut W) -> Self {
        Self {
            state,
            output,
            buffer: [0; OUTPUT_BUFFER_BYTES],
            finished: false,
        }
    }

    fn finish(&mut self) -> Result<(), ArtifactBuildError> {
        while unsafe { brotli_sys::BrotliEncoderIsFinished(self.state.as_ptr()) } == 0 {
            let mut empty = &[][..];
            self.process(
                brotli_sys::BrotliEncoderOperation_BROTLI_OPERATION_FINISH,
                &mut empty,
            )?;
        }
        self.finished = true;
        self.output.flush().map_err(ArtifactBuildError::from)
    }

    fn process(
        &mut self,
        operation: brotli_sys::BrotliEncoderOperation,
        input: &mut &[u8],
    ) -> io::Result<()> {
        let before = input.len();
        let mut available_input = before;
        let mut next_input = input.as_ptr();
        let mut available_output = self.buffer.len();
        let mut next_output = self.buffer.as_mut_ptr();
        let mut total_output = 0;
        let success = unsafe {
            brotli_sys::BrotliEncoderCompressStream(
                self.state.as_ptr(),
                operation,
                &mut available_input,
                &mut next_input,
                &mut available_output,
                &mut next_output,
                &mut total_output,
            )
        } != 0;
        let consumed = before.saturating_sub(available_input);
        let produced = self.buffer.len().saturating_sub(available_output);
        if produced != 0 {
            self.output.write_all(&self.buffer[..produced])?;
        }
        *input = &input[consumed..];
        if !success {
            return Err(io::Error::other("Brotli encoder rejected the stream"));
        }
        if before != 0 && consumed == 0 && produced == 0 {
            return Err(io::Error::other("Brotli encoder made no progress"));
        }
        Ok(())
    }
}

impl<W: Write> Write for NativeBrotliWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.finished {
            return Err(io::Error::other("Brotli encoder is already finished"));
        }
        let mut remaining = bytes;
        while !remaining.is_empty() {
            self.process(
                brotli_sys::BrotliEncoderOperation_BROTLI_OPERATION_PROCESS,
                &mut remaining,
            )?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.finished {
            return self.output.flush();
        }
        loop {
            let mut empty = &[][..];
            self.process(
                brotli_sys::BrotliEncoderOperation_BROTLI_OPERATION_FLUSH,
                &mut empty,
            )?;
            if unsafe { brotli_sys::BrotliEncoderHasMoreOutput(self.state.as_ptr()) } == 0 {
                break;
            }
        }
        self.output.flush()
    }
}

struct BrotliAllocatorState {
    scratch: CodecScratchBudget,
    failure: Option<ArtifactBudgetError>,
    allocation_attempts: u64,
    fail_allocation: Option<u64>,
}

impl BrotliAllocatorState {
    fn new(scratch: CodecScratchBudget, fail_allocation: Option<u64>) -> Self {
        Self {
            scratch,
            failure: None,
            allocation_attempts: 0,
            fail_allocation,
        }
    }

    fn record_failure(&mut self, error: ArtifactBudgetError) {
        if self.failure.is_none() {
            self.failure = Some(error);
        }
    }
}

struct AllocationHeader {
    base: *mut c_void,
    _lease: CodecScratchLease,
}

unsafe extern "C" fn brotli_allocate(opaque: *mut c_void, size: usize) -> *mut c_void {
    if opaque.is_null() {
        return ptr::null_mut();
    }
    let state = unsafe { &mut *opaque.cast::<BrotliAllocatorState>() };
    let attempt = state.allocation_attempts;
    state.allocation_attempts = match attempt.checked_add(1) {
        Some(value) => value,
        None => {
            state.record_failure(ArtifactBudgetError::ArithmeticOverflow {
                resource: "Brotli allocation attempts",
            });
            return ptr::null_mut();
        }
    };

    let payload_bytes = size.max(1);
    let overhead = match size_of::<AllocationHeader>().checked_add(ALLOCATION_ALIGNMENT - 1) {
        Some(value) => value,
        None => {
            state.record_failure(ArtifactBudgetError::ArithmeticOverflow {
                resource: "Brotli allocation header",
            });
            return ptr::null_mut();
        }
    };
    let allocation_bytes = match payload_bytes.checked_add(overhead) {
        Some(value) => value,
        None => {
            state.record_failure(ArtifactBudgetError::ArithmeticOverflow {
                resource: "Brotli allocation size",
            });
            return ptr::null_mut();
        }
    };
    let charged_bytes = match u64::try_from(allocation_bytes) {
        Ok(value) => value,
        Err(_) => {
            state.record_failure(ArtifactBudgetError::ArithmeticOverflow {
                resource: "Brotli allocation size",
            });
            return ptr::null_mut();
        }
    };
    if state.fail_allocation == Some(attempt) {
        state.record_failure(ArtifactBudgetError::SystemAllocationFailed {
            resource: "Brotli codec scratch",
            requested: charged_bytes,
        });
        return ptr::null_mut();
    }
    let lease = match state.scratch.try_reserve(charged_bytes) {
        Ok(lease) => lease,
        Err(error) => {
            state.record_failure(error);
            return ptr::null_mut();
        }
    };
    let base = unsafe { libc::malloc(allocation_bytes) };
    if base.is_null() {
        drop(lease);
        state.record_failure(ArtifactBudgetError::SystemAllocationFailed {
            resource: "Brotli codec scratch",
            requested: charged_bytes,
        });
        return ptr::null_mut();
    }

    let payload_start = unsafe { base.cast::<u8>().add(size_of::<AllocationHeader>()) } as usize;
    let aligned_payload =
        (payload_start + (ALLOCATION_ALIGNMENT - 1)) & !(ALLOCATION_ALIGNMENT - 1);
    let header = (aligned_payload - size_of::<AllocationHeader>()) as *mut AllocationHeader;
    unsafe {
        ptr::write(
            header,
            AllocationHeader {
                base,
                _lease: lease,
            },
        )
    };
    aligned_payload as *mut c_void
}

unsafe extern "C" fn brotli_free(_opaque: *mut c_void, address: *mut c_void) {
    if address.is_null() {
        return;
    }
    let header = unsafe {
        address
            .cast::<u8>()
            .sub(size_of::<AllocationHeader>())
            .cast::<AllocationHeader>()
            .read()
    };
    let base = header.base;
    drop(header);
    unsafe { libc::free(base) };
}

fn take_allocator_failure(
    allocator: &mut std::pin::Pin<&mut BrotliAllocatorState>,
) -> Option<ArtifactBuildError> {
    allocator
        .as_mut()
        .get_mut()
        .failure
        .take()
        .map(ArtifactBuildError::from)
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;
    use crate::artifact::{ArtifactBudget, ArtifactLimits};

    fn encode_with_failure(
        fail_allocation: Option<u64>,
    ) -> (Result<(Vec<u8>, u64), ArtifactBuildError>, ArtifactBudget) {
        let input = vec![b'x'; 64 * 1024];
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let result = {
            let transaction = budget.transaction();
            let result = encode_brotli_inner(
                transaction.codec_scratch_budget(),
                Vec::new(),
                input.len() as u64,
                fail_allocation,
                |writer| {
                    writer.write_all(&input)?;
                    Ok(())
                },
            );
            drop(transaction);
            result
        };
        (result, budget)
    }

    #[test]
    fn native_encoder_round_trips_and_releases_all_scratch() {
        let (result, budget) = encode_with_failure(None);
        let (encoded, allocation_attempts) = result.unwrap();
        assert!(allocation_attempts > 0);
        let mut decoded = Vec::new();
        ::brotli::Decompressor::new(encoded.as_slice(), 4096)
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, vec![b'x'; 64 * 1024]);
        assert_eq!(budget.live_scratch_bytes(), 0);
    }

    #[test]
    fn every_native_allocation_failure_is_typed_and_releases_scratch() {
        let (result, _) = encode_with_failure(None);
        let (_, allocation_attempts) = result.unwrap();
        for failure in 0..allocation_attempts {
            let (result, budget) = encode_with_failure(Some(failure));
            assert!(matches!(
                result,
                Err(ArtifactBuildError::Budget(
                    ArtifactBudgetError::SystemAllocationFailed {
                        resource: "Brotli codec scratch",
                        ..
                    }
                ))
            ));
            assert_eq!(budget.live_scratch_bytes(), 0, "failure {failure}");
        }
    }

    #[test]
    fn caller_scratch_limit_fails_without_aborting() {
        let mut budget =
            ArtifactBudget::new(ArtifactLimits::default().with_max_scratch_bytes(64 * 1024))
                .unwrap();
        let input = vec![0x5a; 64 * 1024];
        let result = {
            let transaction = budget.transaction();
            let result = encode_brotli(
                transaction.codec_scratch_budget(),
                Vec::new(),
                input.len() as u64,
                |writer| {
                    writer.write_all(&input)?;
                    Ok(())
                },
            );
            drop(transaction);
            result
        };
        assert!(matches!(
            result,
            Err(ArtifactBuildError::Budget(ArtifactBudgetError::Exceeded {
                resource: "scratch_bytes",
                ..
            }))
        ));
        assert_eq!(budget.live_scratch_bytes(), 0);
    }
}
