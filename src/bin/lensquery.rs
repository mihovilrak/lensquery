//! The `lensquery` binary — a byte-for-byte alias of `lq`, for anyone whose
//! `$PATH` already has a different `lq` on it. See `src/main.rs`.

fn main() -> std::process::ExitCode {
    lensquery::cli::main()
}
