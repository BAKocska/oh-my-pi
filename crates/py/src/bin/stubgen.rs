//! Generates `_omp.pyi` from PyO3 metadata in the statically linked demo.

use std::{env, fs, path::Path};

use pyo3_introspection::{introspect_cdylib, module_stub_files};

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let mut args = env::args_os().skip(1);
	let binary = args.next().ok_or("missing binary path")?;
	let output = args.next().ok_or("missing output directory")?;
	if args.next().is_some() {
		return Err("stubgen takes exactly a binary path and output directory".into());
	}
	let module = introspect_cdylib(binary, "_omp")?;
	for (relative, contents) in module_stub_files(&module) {
		let relative = if relative == Path::new("__init__.pyi") {
			Path::new("_omp.pyi")
		} else {
			relative.as_path()
		};
		let path = Path::new(&output).join(relative);
		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent)?;
		}
		fs::write(path, contents)?;
	}
	Ok(())
}
