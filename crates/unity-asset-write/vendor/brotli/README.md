# Brotli encoder source

This directory contains the Google Brotli 1.2.0 C encoder sources under the MIT license.
`unity-asset-write` builds them with `BROTLI_ENCODER_CLEANUP_ON_OOM` so a custom allocator
failure is reported to Rust instead of terminating the host process.

The build prefixes every externally visible C symbol with
`unity_asset_write_brotli_1_2_0_`. This prevents another Brotli archive in a downstream link
from replacing the recoverable-OOM implementation through static-library link order.

Source: <https://github.com/google/brotli/releases/tag/v1.2.0>
