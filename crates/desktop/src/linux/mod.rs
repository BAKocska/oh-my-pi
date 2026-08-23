mod actor;
pub mod ax;
pub mod wayland;
pub mod x11;

use std::env;

use self::{wayland::WaylandBackend, x11::X11Backend};
use super::{
	backend::Backend,
	error::{CoreResult, DesktopError},
	types::DisplaySelector,
};

pub fn new_backend(display: DisplaySelector) -> CoreResult<Box<dyn Backend>> {
	if env::var_os("WAYLAND_DISPLAY").is_some() {
		return Ok(Box::new(WaylandBackend::new(display)));
	}
	if env::var_os("DISPLAY").is_some() {
		return Ok(Box::new(X11Backend::new(display)?));
	}
	Err(DesktopError::capture_failed(
		"no display server (neither WAYLAND_DISPLAY nor DISPLAY is set)",
	))
}
