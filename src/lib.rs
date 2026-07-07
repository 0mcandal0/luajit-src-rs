use std::error::Error;
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs, io};

type DynError = Box<dyn Error + Send + Sync>;

/// Represents the configuration for building LuaJIT artifacts.
pub struct Build {
    out_dir: Option<PathBuf>,
    target: Option<String>,
    host: Option<String>,
    lua52compat: bool,
    debug: Option<bool>,
}

/// Represents the artifacts produced by the build process.
pub struct Artifacts {
    include_dir: PathBuf,
    lib_dir: PathBuf,
    libs: Vec<String>,
}

impl Default for Build {
    fn default() -> Self {
        Build {
            out_dir: env::var_os("OUT_DIR").map(PathBuf::from),
            target: env::var("TARGET").ok(),
            host: env::var("HOST").ok(),
            lua52compat: false,
            debug: None,
        }
    }
}

impl Build {
    /// Creates a new `Build` instance with default settings.
    pub fn new() -> Build {
        Build::default()
    }

    /// Sets the output directory for the build artifacts.
    ///
    /// This is required if called outside of a build script.
    pub fn out_dir<P: AsRef<Path>>(&mut self, path: P) -> &mut Build {
        self.out_dir = Some(path.as_ref().to_path_buf());
        self
    }

    /// Sets the target architecture for the build.
    ///
    /// This is required if called outside of a build script.
    pub fn target(&mut self, target: &str) -> &mut Build {
        self.target = Some(target.to_string());
        self
    }

    /// Sets the host architecture for the build.
    ///
    /// This is optional and will default to the environment variable `HOST` if not set.
    /// If called outside of a build script, it will default to the target architecture.
    pub fn host(&mut self, host: &str) -> &mut Build {
        self.host = Some(host.to_string());
        self
    }

    /// Enables or disables Lua 5.2 limited compatibility mode.
    pub fn lua52compat(&mut self, enabled: bool) -> &mut Build {
        self.lua52compat = enabled;
        self
    }

    /// Sets whether to build LuaJIT in debug mode.
    ///
    /// This is optional and will default to the value of `cfg!(debug_assertions)`.
    /// If set to `true`, it also enables Lua API checks.
    pub fn debug(&mut self, debug: bool) -> &mut Build {
        self.debug = Some(debug);
        self
    }

    fn cmd_make(&self) -> Command {
        match &self.host.as_ref().expect("HOST is not set")[..] {
            "x86_64-unknown-dragonfly" => Command::new("gmake"),
            "x86_64-unknown-freebsd" => Command::new("gmake"),
            _ => Command::new("make"),
        }
    }

    /// Builds the LuaJIT artifacts.
    pub fn build(&mut self) -> Artifacts {
        self.try_build().expect("LuaJIT build failed")
    }

    /// Attempts to build the LuaJIT artifacts.
    ///
    /// Returns an error if the build fails.
    pub fn try_build(&mut self) -> Result<Artifacts, DynError> {
        let target = &self.target.as_ref().expect("TARGET is not set")[..];

        if target.contains("msvc") {
            return self.build_msvc();
        }

        self.build_unix()
    }

    fn build_unix(&mut self) -> Result<Artifacts, DynError> {
        let target = &self.target.as_ref().expect("TARGET is not set")[..];
        let host = &self.host.as_ref().expect("HOST is not set")[..];
        let out_dir = self.out_dir.as_ref().expect("OUT_DIR is not set");
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let source_dir = manifest_dir.join("luajit2");
        let build_dir = out_dir.join("luajit-build");
        let lib_dir = out_dir.join("lib");
        let include_dir = out_dir.join("include");

        // Cleanup
        for dir in [&build_dir, &lib_dir, &include_dir] {
            if dir.exists() {
                fs::remove_dir_all(dir).context(|| format!("Cannot remove {}", dir.display()))?;
            }
            fs::create_dir_all(dir).context(|| format!("Cannot create {}", dir.display()))?;
        }
        cp_r(&source_dir, &build_dir)?;

        // Copy release version file
        let relver = build_dir.join(".relver");
        fs::copy(manifest_dir.join("luajit_relver.txt"), &relver)
            .context(|| "Cannot copy 'luajit_relver.txt'")?;

        // Fix permissions for certain build situations
        let mut perms = (fs::metadata(&relver).map(|md| md.permissions()))
            .context(|| format!("Cannot read permissions for '{}'", relver.display()))?;
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(&relver, perms)
            .context(|| format!("Cannot set permissions for '{}'", relver.display()))?;

        let mut cc = cc::Build::new();
        cc.warnings(false);
        let compiler = cc.get_compiler();
        let compiler_path = compiler.path().to_str().unwrap();

        let mut make = self.cmd_make();
        make.current_dir(build_dir.join("src"));
        make.arg("-e");

        match target {
            "x86_64-apple-darwin" if env::var_os("MACOSX_DEPLOYMENT_TARGET").is_none() => {
                make.env("MACOSX_DEPLOYMENT_TARGET", "10.14");
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
            which::which(compiler_path).context(|| format!("Cannot find {compiler_path}"))?;
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
        if self.lua52compat {
            xcflags.push("-DLUAJIT_ENABLE_LUA52COMPAT");
        }

        let debug = self.debug.unwrap_or(cfg!(debug_assertions));
        if debug {
            make.env("CCDEBUG", "-g");
            xcflags.push("-DLUA_USE_ASSERT");
            xcflags.push("-DLUA_USE_APICHECK");
        }

        make.env("BUILDMODE", "static");
        make.env("XCFLAGS", xcflags.join(" "));
        self.run_command(&mut make)
            .context(|| format!("Error running '{make:?}'"))?;

        Artifacts::make(&build_dir, &include_dir, &lib_dir, false)
    }

    // Overa: cross-compiling to windows-msvc from Linux has no real MSVC install for
    // luajit2's own msvcbuild.bat to run against (it needs a live `cl.exe`/vcvars
    // environment, which `cc::windows_registry::find_tool` can never provide on a
    // non-Windows host). We replicate what msvcbuild.bat does by hand with
    // clang-cl + lld-link + llvm-lib, running the intermediate `minilua`/`buildvm`
    // code-generator tools under Wine since they are Windows PE binaries that must
    // execute *during* the build to produce LuaJIT's generated sources.
    fn build_msvc(&mut self) -> Result<Artifacts, DynError> {
        let out_dir = self.out_dir.as_ref().expect("OUT_DIR is not set");
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let source_dir = manifest_dir.join("luajit2");
        let build_dir = out_dir.join("luajit-build");
        let lib_dir = out_dir.join("lib");
        let include_dir = out_dir.join("include");

        // Cleanup
        for dir in [&build_dir, &lib_dir, &include_dir] {
            if dir.exists() {
                fs::remove_dir_all(dir).context(|| format!("Cannot remove {}", dir.display()))?;
            }
            fs::create_dir_all(dir).context(|| format!("Cannot create {}", dir.display()))?;
        }
        cp_r(&source_dir, &build_dir)?;

        // Copy release version file
        let relver = build_dir.join(".relver");
        fs::copy(manifest_dir.join("luajit_relver.txt"), &relver)
            .context(|| "Cannot copy 'luajit_relver.txt'")?;

        let debug = self.debug.unwrap_or(cfg!(debug_assertions));
        let lua52compat = self.lua52compat;

        let minilua_exe = self.compile_minilua(&build_dir)?;
        self.generate_intermediate_files(&build_dir, &minilua_exe)?;
        let buildvm_exe = self.compile_buildvm(&build_dir, lua52compat)?;
        self.generate_vm_files(&build_dir, &buildvm_exe)?;
        self.compile_luajit_sources(&build_dir, lua52compat, debug)?;
        self.generate_static_library(&build_dir)?;

        Artifacts::make(&build_dir, &include_dir, &lib_dir, true)
    }

    fn compile_minilua(&self, build_dir: &Path) -> Result<PathBuf, DynError> {
        let host_dir = build_dir.join("src").join("host");
        let minilua_exe = build_dir.join("src").join("minilua.exe");

        let mut compile_minilua = Command::new("clang-cl");
        compile_minilua
            .current_dir(&host_dir)
            .arg("/c")
            .arg("/I")
            .arg(build_dir.join("src"))
            .arg("/D_CRT_SECURE_NO_DEPRECATE")
            .arg("/D_CRT_STDIO_INLINE=__declspec(dllexport)__inline")
            .arg("/O2")
            .arg("/W3")
            .arg("/MT")
            // clang-cl's cl.exe-compatible parser treats a bare argument starting with
            // "/" as an (unrecognized, silently dropped) flag rather than a positional
            // input -- and our absolute Unix paths all start with "/". `--` forces
            // everything after it to be parsed as a positional file.
            .arg("--")
            .arg(host_dir.join("minilua.c"));
        self.run_command(&mut compile_minilua)
            .context(|| format!("Error running '{compile_minilua:?}'"))?;

        let mut link_minilua = Command::new("lld-link");
        link_minilua
            .current_dir(build_dir.join("src"))
            .arg("/out:minilua.exe")
            .arg(host_dir.join("minilua.obj"));
        self.run_command(&mut link_minilua)
            .context(|| format!("Error running '{link_minilua:?}'"))?;

        Ok(minilua_exe)
    }

    fn generate_intermediate_files(
        &self,
        build_dir: &Path,
        minilua_exe: &Path,
    ) -> Result<(), DynError> {
        let mut generate_buildvm_arch = Command::new("wine64");
        generate_buildvm_arch
            .current_dir(build_dir.join("src"))
            .arg(minilua_exe)
            .arg("../dynasm/dynasm.lua")
            .arg("-LN")
            .arg("-D")
            .arg("WIN")
            .arg("-D")
            .arg("JIT")
            .arg("-D")
            .arg("FFI")
            .arg("-D")
            .arg("ENDIAN_LE")
            .arg("-D")
            .arg("FPU")
            .arg("-D")
            .arg("P64")
            .arg("-o")
            .arg("host/buildvm_arch.h")
            .arg("vm_x64.dasc");
        self.run_command(&mut generate_buildvm_arch)
            .context(|| format!("Error running '{generate_buildvm_arch:?}'"))?;

        let relver = build_dir.join(".relver");
        let luajit_relver = build_dir.join("src").join("luajit_relver.txt");
        fs::copy(&relver, &luajit_relver).context(|| {
            format!(
                "Cannot copy '{}' to '{}'",
                relver.display(),
                luajit_relver.display()
            )
        })?;

        let mut create_luajit_h = Command::new("wine64");
        create_luajit_h
            .current_dir(build_dir.join("src"))
            .arg(minilua_exe)
            .arg("host/genversion.lua");
        self.run_command(&mut create_luajit_h)
            .context(|| format!("Error running '{create_luajit_h:?}'"))?;

        Ok(())
    }

    fn compile_buildvm(&self, build_dir: &Path, lua52compat: bool) -> Result<PathBuf, DynError> {
        let src_dir = build_dir.join("src");
        let buildvm_exe = src_dir.join("buildvm.exe");

        let mut compile_buildvm = Command::new("clang-cl");
        compile_buildvm
            .current_dir(&src_dir)
            .arg("/c")
            .arg("/I")
            .arg(&src_dir)
            .arg("/D_CRT_SECURE_NO_DEPRECATE")
            .arg("/D_CRT_STDIO_INLINE=__declspec(dllexport)__inline")
            .arg("/O2")
            .arg("/W3")
            .arg("/MT");
        if lua52compat {
            compile_buildvm.arg("/DLUAJIT_ENABLE_LUA52COMPAT");
        }
        // See the comment on the minilua compile: forces positional parsing of the
        // absolute Unix paths that follow, which clang-cl would otherwise treat as
        // unrecognized "/"-prefixed flags and silently drop.
        compile_buildvm.arg("--");
        compile_buildvm.args([
            src_dir.join("host").join("buildvm.c"),
            src_dir.join("host").join("buildvm_asm.c"),
            src_dir.join("host").join("buildvm_fold.c"),
            src_dir.join("host").join("buildvm_lib.c"),
            src_dir.join("host").join("buildvm_peobj.c"),
        ]);
        self.run_command(&mut compile_buildvm)
            .context(|| format!("Error running '{compile_buildvm:?}'"))?;

        let mut link_buildvm = Command::new("lld-link");
        link_buildvm
            .current_dir(&src_dir)
            .arg("/out:buildvm.exe")
            .arg("buildvm.obj")
            .arg("buildvm_asm.obj")
            .arg("buildvm_fold.obj")
            .arg("buildvm_lib.obj")
            .arg("buildvm_peobj.obj");
        self.run_command(&mut link_buildvm)
            .context(|| format!("Error running '{link_buildvm:?}'"))?;

        Ok(buildvm_exe)
    }

    fn generate_vm_files(&self, build_dir: &Path, buildvm_exe: &Path) -> Result<(), DynError> {
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
                .arg("-m")
                .arg(mode)
                .arg("-o")
                .arg(output);

            if mode != "peobj" && mode != "folddef" {
                cmd.args([
                    "lib_base.c",
                    "lib_math.c",
                    "lib_bit.c",
                    "lib_string.c",
                    "lib_table.c",
                    "lib_io.c",
                    "lib_os.c",
                    "lib_package.c",
                    "lib_debug.c",
                    "lib_jit.c",
                    "lib_ffi.c",
                    "lib_buffer.c",
                ]);
            } else if mode == "folddef" {
                cmd.arg("lj_opt_fold.c");
            }

            self.run_command(&mut cmd)
                .context(|| format!("Error running '{cmd:?}'"))?;
        }

        Ok(())
    }

    fn compile_luajit_sources(
        &self,
        build_dir: &Path,
        lua52compat: bool,
        debug: bool,
    ) -> Result<(), DynError> {
        let src_dir = build_dir.join("src");
        let c_files: Vec<_> = fs::read_dir(&src_dir)
            .context(|| format!("Cannot read directory '{}'", src_dir.display()))?
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
        compile_luajit
            .current_dir(&src_dir)
            .arg("/c")
            .arg("/I")
            .arg(&src_dir)
            .arg("/D_CRT_SECURE_NO_DEPRECATE")
            .arg("/D_CRT_STDIO_INLINE=__declspec(dllexport)__inline")
            .arg("/O2")
            .arg("/W3");

        if debug {
            compile_luajit.arg("/MTd").arg("/Z7");
            compile_luajit
                .arg("/DLUA_USE_ASSERT")
                .arg("/DLUA_USE_APICHECK");
        } else {
            compile_luajit.arg("/MT");
        }
        if lua52compat {
            compile_luajit.arg("/DLUAJIT_ENABLE_LUA52COMPAT");
        }
        // See the comment on the minilua compile.
        compile_luajit.arg("--");
        compile_luajit.args(&c_files);

        self.run_command(&mut compile_luajit)
            .context(|| format!("Error running '{compile_luajit:?}'"))?;

        Ok(())
    }

    fn generate_static_library(&self, build_dir: &Path) -> Result<(), DynError> {
        let src_dir = build_dir.join("src");
        let obj_files: Vec<_> = fs::read_dir(&src_dir)
            .context(|| format!("Cannot read directory '{}'", src_dir.display()))?
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                let file_name = path.file_name()?.to_str()?.to_string();
                if (file_name.starts_with("lj_") || file_name.starts_with("lib_"))
                    && file_name.ends_with(".obj")
                {
                    Some(path)
                } else {
                    None
                }
            })
            .collect();

        let mut generate_lib = Command::new("llvm-lib");
        generate_lib
            .current_dir(&src_dir)
            .arg("/nologo")
            .arg("/nodefaultlib")
            .arg("/out:lua51.lib")
            .args(&obj_files);
        self.run_command(&mut generate_lib)
            .context(|| format!("Error running '{generate_lib:?}'"))?;

        Ok(())
    }

    fn run_command(&self, command: &mut Command) -> io::Result<()> {
        let status = command.status()?;
        if !status.success() {
            return Err(io::Error::other(format!("exited with status {status}")));
        }
        Ok(())
    }
}

fn cp_r(src: &Path, dst: &Path) -> Result<(), DynError> {
    for f in fs::read_dir(src).context(|| format!("Cannot read directory '{}'", src.display()))? {
        let f = f.context(|| format!("Cannot read entry in '{}'", src.display()))?;
        let path = f.path();
        let name = path.file_name().unwrap();

        // Skip git metadata
        if name.to_str() == Some(".git") {
            continue;
        }

        let dst = dst.join(name);
        if f.file_type().unwrap().is_dir() {
            fs::create_dir_all(&dst)
                .context(|| format!("Cannot create directory '{}'", dst.display()))?;
            cp_r(&path, &dst)?;
        } else {
            let _ = fs::remove_file(&dst);
            fs::copy(&path, &dst)
                .context(|| format!("Cannot copy '{}' to '{}'", path.display(), dst.display()))?;
        }
    }
    Ok(())
}

impl Artifacts {
    /// Returns the directory containing the LuaJIT headers.
    pub fn include_dir(&self) -> &Path {
        &self.include_dir
    }

    /// Returns the directory containing the LuaJIT libraries.
    pub fn lib_dir(&self) -> &Path {
        &self.lib_dir
    }

    /// Returns the names of the LuaJIT libraries built.
    pub fn libs(&self) -> &[String] {
        &self.libs
    }

    /// Prints the necessary Cargo metadata for linking the LuaJIT libraries.
    ///
    /// This method is typically called in a build script to inform Cargo
    /// about the location of the LuaJIT libraries and how to link them.
    pub fn print_cargo_metadata(&self) {
        println!("cargo:rerun-if-env-changed=HOST_CC");
        println!("cargo:rerun-if-env-changed=STATIC_CC");
        println!("cargo:rerun-if-env-changed=TARGET_LD");
        println!("cargo:rerun-if-env-changed=TARGET_AR");
        println!("cargo:rerun-if-env-changed=TARGET_STRIP");
        println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");

        println!("cargo:rustc-link-search=native={}", self.lib_dir.display());
        for lib in self.libs.iter() {
            println!("cargo:rustc-link-lib=static={lib}");
        }
    }

    fn make(
        build_dir: &Path,
        include_dir: &Path,
        lib_dir: &Path,
        is_msvc: bool,
    ) -> Result<Self, DynError> {
        for f in &["lauxlib.h", "lua.h", "luaconf.h", "luajit.h", "lualib.h"] {
            let from = build_dir.join("src").join(f);
            let to = include_dir.join(f);
            fs::copy(&from, &to)
                .context(|| format!("Cannot copy '{}' to '{}'", from.display(), to.display()))?;
        }

        let lib_name = if !is_msvc { "luajit" } else { "lua51" };
        let lib_file = if !is_msvc { "libluajit.a" } else { "lua51.lib" };
        if build_dir.join("src").join(lib_file).exists() {
            let from = build_dir.join("src").join(lib_file);
            let to = lib_dir.join(lib_file);
            fs::copy(&from, &to)
                .context(|| format!("Cannot copy '{}' to '{}'", from.display(), to.display()))?;
        }

        Ok(Artifacts {
            lib_dir: lib_dir.to_path_buf(),
            include_dir: include_dir.to_path_buf(),
            libs: vec![lib_name.to_string()],
        })
    }
}

trait ErrorContext<T> {
    fn context<D: Display>(self, f: impl FnOnce() -> D) -> Result<T, DynError>;
}

impl<T, E: Error> ErrorContext<T> for Result<T, E> {
    fn context<D: Display>(self, f: impl FnOnce() -> D) -> Result<T, DynError> {
        self.map_err(|e| format!("{}: {e}", f()).into())
    }
}
