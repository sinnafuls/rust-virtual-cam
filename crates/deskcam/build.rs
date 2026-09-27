//! Compiles `src/nv12.hlsl` to DXBC with the system `d3dcompiler_47.dll`, so the app embeds
//! bytecode and never loads the shader compiler at runtime.

use std::{env, fs, path::PathBuf};

use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::core::PCSTR;

fn bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) }
}

fn main() {
    let src_path = "src/nv12.hlsl";
    println!("cargo:rerun-if-changed={src_path}");
    let source = fs::read(src_path).expect("read nv12.hlsl");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    for (entry, target) in [("vs_main\0", "vs_4_0\0"), ("ps_y\0", "ps_4_0\0"), ("ps_uv\0", "ps_4_0\0")] {
        let (mut code, mut errors) = (None, None);
        let result = unsafe {
            D3DCompile(
                source.as_ptr() as *const _,
                source.len(),
                PCSTR(c"nv12.hlsl".as_ptr() as *const u8),
                None,
                None,
                PCSTR(entry.as_ptr()),
                PCSTR(target.as_ptr()),
                D3DCOMPILE_OPTIMIZATION_LEVEL3,
                0,
                &mut code,
                Some(&mut errors),
            )
        };
        if let Err(e) = result {
            let msg = errors.map(|b| String::from_utf8_lossy(bytes(&b)).into_owned()).unwrap_or_default();
            panic!("compiling {entry} failed: {e}\n{msg}");
        }
        let name = entry.trim_end_matches('\0');
        fs::write(out.join(format!("{name}.dxbc")), bytes(&code.expect("shader bytecode"))).expect("write dxbc");
    }
}
