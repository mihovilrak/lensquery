//! The `lq` binary. All of it lives in `lensquery::cli`; this is the shim.
//!
//! The same shim exists as `src/bin/lensquery.rs` under the crate name, for
//! anyone whose `$PATH` already has a different `lq` on it.

fn main() -> std::process::ExitCode {
    lensquery::cli::main()
}
