fn main() {
    // Embed assets/app.ico as resource ID 1: Windows shows it as the .exe icon
    // in Explorer/taskbar, and build_icon() loads it for the tray.
    //
    // FileDescription is what Task Manager shows as the process name in the
    // Processes tab (without it Windows falls back to the raw exe filename).
    let version = env!("CARGO_PKG_VERSION");
    winresource::WindowsResource::new()
        .set_icon("assets/app.ico")
        .set("FileDescription", "LG UltraGear RGB Control")
        .set("ProductName", "LG UltraGear RGB Control")
        .set("FileVersion", version)
        .set("ProductVersion", version)
        .set("OriginalFilename", "lg-ultragear-rgb-control.exe")
        .compile()
        .expect("failed to embed assets/app.ico");

    compile_compute_shader();
}

/// Compiles src/compute.hlsl to cs_5_0 bytecode at build time, so the runtime
/// never loads d3dcompiler_47 (a 4.5 MB DLL that would otherwise be loaded
/// once per session just for this). DXBC is vendor-neutral: each GPU driver
/// translates it to its own ISA, so one blob serves every NVIDIA/AMD/Intel
/// GPU with Direct3D feature level 11_0.
fn compile_compute_shader() {
    use windows::core::s;
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;

    let hlsl = std::fs::read("src/compute.hlsl").expect("read src/compute.hlsl");
    println!("cargo:rerun-if-changed=src/compute.hlsl");

    unsafe {
        let (mut shader, mut errors) = (None, None);
        D3DCompile(
            hlsl.as_ptr().cast(),
            hlsl.len(),
            s!("compute.hlsl"),
            None,
            None,
            s!("main"),
            s!("cs_5_0"),
            // D3DCOMPILE_ENABLE_STRICTNESS
            1 << 11,
            0,
            &mut shader,
            Some(&mut errors),
        )
        .map_err(|e| {
            let detail = errors.as_ref().map(|blob: &ID3DBlob| {
                let p = blob.GetBufferPointer() as *const u8;
                let len = blob.GetBufferSize();
                // SAFETY: the compiler wrote len bytes into the blob.
                String::from_utf8_lossy(std::slice::from_raw_parts(p, len)).into_owned()
            });
            match detail {
                Some(d) if !d.is_empty() => format!("shader compile failed: {e}\n{d}"),
                _ => format!("shader compile failed: {e}"),
            }
        })
        .unwrap();
        let blob = shader.expect("D3DCompile: no blob");
        // SAFETY: GetBufferPointer/GetBufferSize describe the blob.
        let bytecode =
            std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize());

        let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
        std::fs::write(out_dir.join("compute.cso"), bytecode).expect("write compute.cso");
    }
}
