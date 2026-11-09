//! 构建脚本。
//!
//! `gui` feature 开启时为 fer-gui 嵌入 Windows manifest（RT_MANIFEST）——comctl32 v6
//! 视觉样式的唯一途径（缺失时 Button/ListView 退回 Win95 经典绘制）。本机走 GNU 链，
//! windres 来自 DevEnv\mingw64\bin（已在 PATH）；可用 WINDRES 环境变量覆盖。
//!
//! 注意：刻意不用静态库（ar + link-lib）—— GNU ld 会丢弃 .a 里无符号引用的成员，
//! 资源会静默丢失；必须把 .o 直接交给链接器。

use std::path::PathBuf;
use std::process::Command;

fn main() {
	// 默认构建（纯 CLI）完全不碰 windres：没有 gui feature 就直接返回。
	if std::env::var_os("CARGO_FEATURE_GUI").is_none() {
		return;
	}

	println!("cargo:rerun-if-changed=app.rc");
	println!("cargo:rerun-if-changed=app.manifest");
	println!("cargo:rerun-if-changed=fer.ico");

	let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
	let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

	let rc = manifest_dir.join("app.rc");
	let obj = out_dir.join("app_res.o");

	let windres = std::env::var("WINDRES").unwrap_or_else(|_| "windres".to_string());
	let status = Command::new(&windres)
		.arg("--input").arg(&rc)
		.arg("--output").arg(&obj)
		.arg("--output-format=coff")
		.arg("--include-dir").arg(&manifest_dir)
		.status()
		.unwrap_or_else(|e| {
			panic!(
				"failed to spawn {windres}: {e}\n\
				 hint: 本机 windres 位于 DevEnv\\mingw64\\bin（PATH 应已包含），\
				 或设 WINDRES 环境变量指向 windres.exe"
			)
		});
	assert!(status.success(), "{windres} failed on {rc:?}");
	assert!(obj.exists(), "{windres} produced no object file");

	// 正斜杠：直接透传给 GNU ld，避免反斜杠被当成转义。
	// 只链接到 fer-gui 这个 bin，绝不污染 `fer` 主 bin。
	println!(
		"cargo:rustc-link-arg-bin=fer-gui={}",
		obj.to_string_lossy().replace('\\', "/")
	);
}
