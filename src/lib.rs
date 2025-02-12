use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Build {
    out_dir: Option<PathBuf>,
    target: Option<String>,
    host: Option<String>,
    options: Options,
}

pub struct Artifacts {
    include_dir: PathBuf,
    lib_dir: PathBuf,
    libs: Vec<String>,
}

#[derive(Default, Clone, Copy)]
struct Options {
    lua52compat: bool,
}

impl Build {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Build {
        Build {
            out_dir: env::var_os("OUT_DIR").map(|s| PathBuf::from(s).join("luajit-build")),
            target: env::var("TARGET").ok(),
            host: env::var("HOST").ok(),
            options: Options::default(),
        }
    }

    pub fn out_dir<P: AsRef<Path>>(&mut self, path: P) -> &mut Build {
        self.out_dir = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn target(&mut self, target: &str) -> &mut Build {
        self.target = Some(target.to_string());
        self
    }

    pub fn host(&mut self, host: &str) -> &mut Build {
        self.host = Some(host.to_string());
        self
    }

    pub fn lua52compat(&mut self, enabled: bool) -> &mut Build {
        self.options.lua52compat = enabled;
        self
    }

    fn cmd_make(&self) -> Command {
        match &self.host.as_ref().expect("HOST dir not set")[..] {
            "x86_64-unknown-dragonfly" => Command::new("gmake"),
            "x86_64-unknown-freebsd" => Command::new("gmake"),
            _ => Command::new("make"),
        }
    }

    pub fn build(&mut self) -> Artifacts {
        let target = &self.target.as_ref().expect("TARGET not set")[..];

        if target.contains("msvc") {
            return self.build_msvc();
        }

        self.build_unix()
    }

    pub fn build_unix(&mut self) -> Artifacts {
        let target = &self.target.as_ref().expect("TARGET not set")[..];
        let host = &self.host.as_ref().expect("HOST not set")[..];
        let out_dir = self.out_dir.as_ref().expect("OUT_DIR not set");
        let source_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("luajit2");
        let build_dir = out_dir.join("build");
        let lib_dir = out_dir.join("lib");
        let include_dir = out_dir.join("include");

        for dir in &[&build_dir, &lib_dir, &include_dir] {
            if dir.exists() {
                fs::remove_dir_all(dir)
                    .unwrap_or_else(|e| panic!("cannot remove {}: {}", dir.display(), e));
            }
            fs::create_dir_all(dir)
                .unwrap_or_else(|e| panic!("cannot create {}: {}", dir.display(), e));
        }
        cp_r(&source_dir, &build_dir);

        let mut cc = cc::Build::new();
        cc.target(target).host(host).warnings(false).opt_level(2);
        let compiler = cc.get_compiler();
        let compiler_path = compiler.path().to_str().unwrap();

        let mut make = self.cmd_make();
        make.current_dir(build_dir.join("src"));
        make.arg("-e");

        match target {
            "x86_64-apple-darwin" if env::var_os("MACOSX_DEPLOYMENT_TARGET").is_none() => {
                make.env("MACOSX_DEPLOYMENT_TARGET", "10.11");
            }
            "aarch64-apple-darwin" if env::var_os("MACOSX_DEPLOYMENT_TARGET").is_none() => {
                make.env("MACOSX_DEPLOYMENT_TARGET", "11.0");
            }
            _ if target.contains("linux") => {
                make.env("TARGET_SYS", "Linux");
            }
            _ if target.contains("windows") => {
                make.env("TARGET_SYS", "Windows");
            }
            _ => {}
        }

        let target_pointer_width = env::var("CARGO_CFG_TARGET_POINTER_WIDTH").unwrap();
        if target_pointer_width == "32" && env::var_os("HOST_CC").is_none() {
            // 32-bit cross-compilation?
            let host_cc = cc::Build::new().target(host).get_compiler();
            make.env("HOST_CC", format!("{} -m32", host_cc.path().display()));
        }

        // Infer ar/ranlib tools from cross compilers if the it looks like
        // we're doing something like `foo-gcc` route that to `foo-ranlib`
        // as well.
        let prefix = if compiler_path.ends_with("-gcc") {
            &compiler_path[..compiler_path.len() - 3]
        } else if compiler_path.ends_with("-clang") {
            &compiler_path[..compiler_path.len() - 5]
        } else {
            ""
        };

        let compiler_path =
            which::which(compiler_path).unwrap_or_else(|_| panic!("cannot find {compiler_path}"));
        let bindir = compiler_path.parent().unwrap();
        let compiler_path = compiler_path.to_str().unwrap();
        let compiler_args = compiler.cflags_env();
        let compiler_args = compiler_args.to_str().unwrap();
        if env::var_os("STATIC_CC").is_none() {
            make.env("STATIC_CC", format!("{compiler_path} {compiler_args}"));
        }
        if env::var_os("TARGET_LD").is_none() {
            make.env("TARGET_LD", format!("{compiler_path} {compiler_args}"));
        }

        // Find ar
        if env::var_os("TARGET_AR").is_none() {
            let mut ar = if bindir.join(format!("{prefix}ar")).is_file() {
                bindir.join(format!("{prefix}ar")).into_os_string()
            } else if compiler.is_like_clang() && bindir.join("llvm-ar").is_file() {
                bindir.join("llvm-ar").into_os_string()
            } else if compiler.is_like_gnu() && bindir.join("ar").is_file() {
                bindir.join("ar").into_os_string()
            } else if let Ok(ar) = which::which(format!("{prefix}ar")) {
                ar.into_os_string()
            } else {
                panic!("cannot find {prefix}ar")
            };
            ar.push(" rcus");
            make.env("TARGET_AR", ar);
        }

        // Find strip
        if env::var_os("TARGET_STRIP").is_none() {
            let strip = if bindir.join(format!("{prefix}strip")).is_file() {
                bindir.join(format!("{prefix}strip"))
            } else if compiler.is_like_clang() && bindir.join("llvm-strip").is_file() {
                bindir.join("llvm-strip")
            } else if compiler.is_like_gnu() && bindir.join("strip").is_file() {
                bindir.join("strip")
            } else if let Ok(strip) = which::which(format!("{prefix}strip")) {
                strip
            } else {
                panic!("cannot find {prefix}strip")
            };
            make.env("TARGET_STRIP", strip);
        }

        let mut xcflags = vec!["-fPIC"];
        if self.options.lua52compat {
            xcflags.push("-DLUAJIT_ENABLE_LUA52COMPAT");
        }

        make.env("BUILDMODE", "static");
        make.env("XCFLAGS", xcflags.join(" "));
        self.run_command(make, "building LuaJIT");

        for f in &["lauxlib.h", "lua.h", "luaconf.h", "luajit.h", "lualib.h"] {
            fs::copy(build_dir.join("src").join(f), include_dir.join(f)).unwrap();
        }
        fs::copy(
            build_dir.join("src").join("libluajit.a"),
            lib_dir.join("libluajit-5.1.a"),
        )
        .unwrap();

        Artifacts {
            lib_dir,
            include_dir,
            libs: vec!["luajit-5.1".to_string()],
        }
    }

    pub fn build_msvc(&mut self) -> Artifacts {
        let target = self.target.as_ref().expect("TARGET not set");
        let out_dir = self.out_dir.as_ref().expect("OUT_DIR not set");
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let source_dir = manifest_dir.join("luajit2");
        let extras_dir = manifest_dir.join("extras");
        let build_dir = out_dir.join("build");
        let lib_dir = out_dir.join("lib");
        let include_dir = out_dir.join("include");
    
        // Crear directorios necesarios
        self.create_directories(&[&build_dir, &lib_dir, &include_dir]);
    
        // Copiar fuentes de LuaJIT al directorio de construcción
        cp_r(&source_dir, &build_dir);
    
        // Paso 1: Compilar minilua.c
        let minilua_exe = self.compile_minilua(&build_dir);
    
        // Paso 2: Usar minilua.exe para generar archivos intermedios
        self.generate_intermediate_files(&build_dir, &minilua_exe);
    
        // Paso 3: Compilar buildvm.c
        let buildvm_exe = self.compile_buildvm(&build_dir);
    
        // Paso 4: Usar buildvm.exe para generar archivos adicionales
        self.generate_vm_files(&build_dir, &buildvm_exe);
    
        // Paso 5: Compilar los archivos .c de LuaJIT
        self.compile_luajit_sources(&build_dir);
    
        // Copiar archivos necesarios
        self.copy_headers(&build_dir, &include_dir);
    
        // Generar biblioteca estática
        self.generate_static_library(&build_dir, &lib_dir);
    
        Artifacts {
            lib_dir,
            include_dir,
            libs: vec!["luajit".to_string()],
        }
    }
    
    fn create_directories(&self,dirs: &[&Path]) {
        for dir in dirs {
            if dir.exists() {
                fs::remove_dir_all(dir).unwrap_or_else(|e| panic!("cannot remove {}: {}", dir.display(), e));
            }
            fs::create_dir_all(dir).unwrap_or_else(|e| panic!("cannot create {}: {}", dir.display(), e));
        }
    }
    
    fn compile_minilua(&self,build_dir: &Path) -> PathBuf {
        let host_dir = build_dir.join("src").join("host");
        let minilua_exe = build_dir.join("src").join("minilua.exe");
    
        let mut compile_minilua = Command::new("clang-cl");
        compile_minilua.current_dir(&host_dir)
            .arg("/c")
            .arg("/I").arg(build_dir.join("src"))
            .arg("/D_CRT_SECURE_NO_DEPRECATE")
            .arg("/D_CRT_STDIO_INLINE=__declspec(dllexport)__inline")
            .arg("/O2")
            .arg("/W3")
            .arg("/MT")
            .arg(host_dir.join("minilua.c"));
    
        self.run_command(compile_minilua, "compiling minilua.c");
    
        let mut link_minilua = Command::new("lld-link");
        link_minilua.current_dir(build_dir.join("src"))
            .arg("/out:minilua.exe")
            .arg(host_dir.join("minilua.obj"));
    
        self.run_command(link_minilua, "linking minilua.exe");
    
        minilua_exe
    }
    
    fn generate_intermediate_files(&self,build_dir: &Path, minilua_exe: &Path) {
        let mut generate_buildvm_arch = Command::new("wine64");
        generate_buildvm_arch.current_dir(build_dir.join("src"))
            .arg(minilua_exe)
            .arg("../dynasm/dynasm.lua")
            .arg("-LN")
            .arg("-D").arg("WIN")
            .arg("-D").arg("JIT")
            .arg("-D").arg("FFI")
            .arg("-D").arg("ENDIAN_LE")
            .arg("-D").arg("FPU")
            .arg("-D").arg("P64")
            .arg("-o").arg("host/buildvm_arch.h")
            .arg("vm_x64.dasc");
    
        self.run_command(generate_buildvm_arch, "generating buildvm_arch.h");
    
        let relver = build_dir.join(".relver");
        let luajit_relver = build_dir.join("src").join("luajit_relver.txt");
        fs::copy(&relver, &luajit_relver).unwrap();
    
        let mut create_luajit_h = Command::new("wine64");
        create_luajit_h.current_dir(build_dir.join("src"))
            .arg(minilua_exe)
            .arg("host/genversion.lua");
    
        self.run_command(create_luajit_h, "generating luajit_h");
    }
    
    fn compile_buildvm(&self,build_dir: &Path) -> PathBuf {
        let buildvm_exe = build_dir.join("src").join("buildvm.exe");
    
        let mut compile_buildvm = Command::new("clang-cl");
        compile_buildvm.current_dir(build_dir.join("src"))
            .arg("/c")
            .arg("/I").arg(build_dir.join("src"))
            .arg("/D_CRT_SECURE_NO_DEPRECATE")
            .arg("/D_CRT_STDIO_INLINE=__declspec(dllexport)__inline")
            .arg("/O2")
            .arg("/W3")
            .arg("/MT")
            .args(vec![
                build_dir.join("src").join("host").join("buildvm.c"),
                build_dir.join("src").join("host").join("buildvm_asm.c"),
                build_dir.join("src").join("host").join("buildvm_fold.c"),
                build_dir.join("src").join("host").join("buildvm_lib.c"),
                build_dir.join("src").join("host").join("buildvm_peobj.c"),
            ]);
    
        self.run_command(compile_buildvm, "compiling buildvm.c");
    
        let mut link_buildvm = Command::new("lld-link");
        link_buildvm.current_dir(build_dir.join("src"))
            .arg("/out:buildvm.exe")
            .arg("buildvm.obj")
            .arg("buildvm_asm.obj")
            .arg("buildvm_fold.obj")
            .arg("buildvm_lib.obj")
            .arg("buildvm_peobj.obj");
    
        self.run_command(link_buildvm, "linking buildvm.exe");
    
        buildvm_exe
    }
    
    fn generate_vm_files(&self,build_dir: &Path, buildvm_exe: &Path) {
        let files_to_generate = [
            ("peobj", "lj_vm.obj"),
            ("bcdef", "lj_bcdef.h"),
            ("ffdef", "lj_ffdef.h"),
            ("libdef", "lj_libdef.h"),
            ("recdef", "lj_recdef.h"),
            ("vmdef", "jit/vmdef.lua"),
            ("folddef", "lj_folddef.h"),
        ];
    
        for (mode, output) in files_to_generate {
            let mut cmd = Command::new("wine64");
            cmd.current_dir(build_dir.join("src"))
                .arg(buildvm_exe)
                .arg("-m").arg(mode)
                .arg("-o").arg(output);
    
            if mode != "peobj" && mode != "folddef" {
                cmd.args(&[
                    "lib_base.c", "lib_math.c", "lib_bit.c", "lib_string.c", "lib_table.c",
                    "lib_io.c", "lib_os.c", "lib_package.c", "lib_debug.c", "lib_jit.c",
                    "lib_ffi.c", "lib_buffer.c",
                ]);
            } else if mode == "folddef" {
                cmd.arg("lj_opt_fold.c");
            }
    
            self.run_command(cmd, &format!("generating {}", output));
        }
    }
    
    fn compile_luajit_sources(&self,build_dir: &Path) {
        let src_dir = build_dir.join("src");
        let c_files: Vec<_> = fs::read_dir(&src_dir)
            .expect("No se pudo leer el directorio src")
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                if path.extension().map_or(false, |ext| ext == "c") {
                    Some(path)
                } else {
                    None
                }
            })
            .collect();
    
        let mut compile_luajit = Command::new("clang-cl");
        compile_luajit.current_dir(&src_dir)
            .arg("/c")
            .arg("/I").arg(&src_dir)
            .arg("/D_CRT_SECURE_NO_DEPRECATE")
            .arg("/D_CRT_STDIO_INLINE=__declspec(dllexport)__inline")
            .arg("/O2")
            .arg("/W3")
            .arg("/MT")
            .args(&c_files);
    
        self.run_command(compile_luajit, "compiling LuaJIT sources");
    }
    
    fn copy_headers(&self,build_dir: &Path, include_dir: &Path) {
        for header in &["lauxlib.h", "lua.h", "luaconf.h", "luajit.h", "lualib.h"] {
            fs::copy(build_dir.join("src").join(header), include_dir.join(header)).unwrap();
        }
    }
    
    fn generate_static_library(&self,build_dir: &Path, lib_dir: &Path) {
        let obj_files: Vec<_> = fs::read_dir(build_dir.join("src"))
            .expect("No se pudo leer el directorio src")
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                let file_name = path.file_name()?.to_str()?;
                if (file_name.starts_with("lj_") && file_name.ends_with(".obj")) ||
                   (file_name.starts_with("lib_") && file_name.ends_with(".obj")) {
                    Some(path)
                } else {
                    None
                }
            })
            .collect();
    
        let mut generate_lib = Command::new("llvm-lib");
        generate_lib.current_dir(build_dir.join("src"))
            .arg("/nologo")
            .arg("/nodefaultlib")
            .arg("/out:lua51.lib")
            .args(&obj_files);
    
        self.run_command(generate_lib, "generate lua51.lib");
    
        fs::copy(
            build_dir.join("src").join("lua51.lib"),
            lib_dir.join("luajit.lib"),
        ).unwrap();
    }

    pub fn _build_msvc(&mut self) -> Artifacts {
        let target = &self.target.as_ref().expect("TARGET not set")[..];
        let out_dir = self.out_dir.as_ref().expect("OUT_DIR not set");
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let source_dir = manifest_dir.join("luajit2");
        let extras_dir = manifest_dir.join("extras");
        let build_dir = out_dir.join("build");
        let lib_dir = out_dir.join("lib");
        let include_dir = out_dir.join("include");

        for dir in &[&build_dir, &lib_dir, &include_dir] {
            if dir.exists() {
                fs::remove_dir_all(dir)
                    .unwrap_or_else(|e| panic!("cannot remove {}: {}", dir.display(), e));
            }
            fs::create_dir_all(dir)
                .unwrap_or_else(|e| panic!("cannot create {}: {}", dir.display(), e));
        }
        cp_r(&source_dir, &build_dir);

        let mut msvcbuild = Command::new(build_dir.join("src").join("msvcbuild.bat"));
        msvcbuild.current_dir(build_dir.join("src"));
        if self.options.lua52compat {
            cp_r(&extras_dir, &build_dir.join("src"));
            msvcbuild.arg("lua52c");
        }
        msvcbuild.arg("static");

        let cl = cc::windows_registry::find_tool(target, "cl.exe").expect("failed to find cl");
        for (k, v) in cl.env() {
            msvcbuild.env(k, v);
        }

        self.run_command(msvcbuild, "building LuaJIT");

        for f in &["lauxlib.h", "lua.h", "luaconf.h", "luajit.h", "lualib.h"] {
            fs::copy(build_dir.join("src").join(f), include_dir.join(f)).unwrap();
        }
        fs::copy(
            build_dir.join("src").join("lua51.lib"),
            lib_dir.join("luajit.lib"),
        )
        .unwrap();

        Artifacts {
            lib_dir,
            include_dir,
            libs: vec!["luajit".to_string()],
        }
    }

    fn run_command(&self, mut command: Command, desc: &str) {
        println!("running {:?}", command);
        let status = command.status().unwrap();
        if !status.success() {
            panic!(
                "
Error {}:
    Command: {:?}
    Exit status: {}
    ",
                desc, command, status
            );
        }
    }
}

fn cp_r(src: &Path, dst: &Path) {
    for f in fs::read_dir(src).unwrap() {
        let f = f.unwrap();
        let path = f.path();
        let name = path.file_name().unwrap();

        // Skip git metadata
        if name.to_str() == Some(".git") {
            continue;
        }

        let dst = dst.join(name);
        if f.file_type().unwrap().is_dir() {
            fs::create_dir_all(&dst).unwrap();
            cp_r(&path, &dst);
        } else {
            let _ = fs::remove_file(&dst);
            fs::copy(&path, &dst).unwrap();
        }
    }
}

impl Artifacts {
    pub fn include_dir(&self) -> &Path {
        &self.include_dir
    }

    pub fn lib_dir(&self) -> &Path {
        &self.lib_dir
    }

    pub fn libs(&self) -> &[String] {
        &self.libs
    }

    pub fn print_cargo_metadata(&self) {
        println!("cargo:rerun-if-env-changed=HOST_CC");
        println!("cargo:rerun-if-env-changed=STATIC_CC");
        println!("cargo:rerun-if-env-changed=TARGET_LD");
        println!("cargo:rerun-if-env-changed=TARGET_AR");
        println!("cargo:rerun-if-env-changed=TARGET_STRIP");
        println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");

        println!("cargo:rustc-link-search=native={}", self.lib_dir.display());
        for lib in self.libs.iter() {
            println!("cargo:rustc-link-lib=static={}", lib);
        }
        println!("cargo:include={}", self.include_dir.display());
        println!("cargo:lib={}", self.lib_dir.display());
    }
}
