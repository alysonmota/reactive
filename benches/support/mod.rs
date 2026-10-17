pub fn criterion() -> criterion::Criterion {
    let directory = std::env::var_os("REACTIVE_BENCH_LOG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/.log/local/criterion")
        });
    std::fs::create_dir_all(&directory).expect("benchmark log directory");
    std::fs::write(
        directory.join("build.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "panic_unwind": cfg!(panic = "unwind"),
            "diagnostics": cfg!(feature = "diagnostics"),
        "verification": cfg!(feature = "verification"),
        "experimental_inline_reads": cfg!(feature = "experimental-inline-reads"),
        }))
        .unwrap(),
    )
    .expect("benchmark build metadata");
    criterion::Criterion::default().output_directory(&directory)
}
