// Put CUDA's import libraries on the linker search path.
//
// cudarc itself dlopens nvcuda.dll and so needs no import library, but the four
// cuGraphicsD3D11* symbols in main.rs are declared with #[link(name = "cuda")]
// and DO need cuda.lib at link time. Without this the build fails with
// `LNK1181: cannot open input file 'cuda.lib'`.
//
// Task 12 needs this same search path for the same reason.
fn main() {
    if !cfg!(windows) {
        return;
    }
    println!("cargo:rerun-if-env-changed=CUDA_PATH");
    println!("cargo:rerun-if-changed=src/nv12.cu");
    let cuda_path = std::env::var("CUDA_PATH")
        .expect("CUDA_PATH not set -- install the CUDA toolkit so cuda.lib can be found");
    println!("cargo:rustc-link-search=native={cuda_path}\\lib\\x64");

    // Precompile nv12.cu to PTX with nvcc rather than compiling it at runtime
    // with NVRTC.
    //
    // Originally this spike used `cudarc::nvrtc::compile_ptx`. On CUDA 12.6,
    // cudarc 0.16.6's NVRTC bindings abort at runtime with
    // `Expected symbol in library: GetProcAddress ... code 127` -- its nvrtc
    // sys layer expects symbols the installed nvrtc DLL does not export. Since
    // §8.3 requires build-time PTX for the real encoder anyway (so the host
    // needs no toolkit at runtime), compiling here both fixes the spike and
    // pre-validates exactly what Task 12 does.
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("nv12.ptx");
    let nvcc = format!("{cuda_path}\\bin\\nvcc.exe");

    // nvcc shells out to the MSVC host compiler and fails with
    // `Cannot find compiler 'cl.exe' in PATH` if it is not discoverable. A
    // plain SSH session has no Developer Command Prompt environment, so point
    // nvcc at cl.exe explicitly. The `cc` crate already knows how to locate the
    // MSVC toolchain, so reuse that rather than reimplementing vswhere.
    let mut cmd = std::process::Command::new(&nvcc);
    match cc::Build::new()
        .target(&std::env::var("TARGET").unwrap())
        .try_get_compiler()
    {
        Ok(compiler) if compiler.path().file_stem().is_some_and(|s| s == "cl") => {
            let cl_dir = compiler
                .path()
                .parent()
                .expect("cl.exe has a parent directory");
            cmd.arg("-ccbin").arg(cl_dir);
        }
        _ => {
            println!("cargo:warning=could not locate cl.exe; relying on nvcc finding it on PATH");
        }
    }

    let status = cmd
        .args(["--ptx", "src/nv12.cu", "-o"])
        .arg(&out)
        .status()
        .unwrap_or_else(|e| panic!("failed to run {nvcc}: {e}"));
    assert!(status.success(), "nvcc failed to compile nv12.cu");
}
