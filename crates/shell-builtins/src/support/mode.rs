//! GNU-compatible numeric and symbolic file-mode parsing.
#[cfg(unix)]
use rustix::fs;

/// Applies an octal mode expression to `current_mode`.
pub(crate) fn parse_numeric(
	current_mode: u32,
	mut mode: &str,
	considering_dir: bool,
) -> Result<u32, String> {
	let (operator, offset) =
		parse_operator(mode).map_or((None, 0), |(op, offset)| (Some(op), offset));
	mode = mode[offset..].trim();
	let change = if mode.is_empty() {
		0
	} else {
		u32::from_str_radix(mode, 8).map_err(|error| error.to_string())?
	};
	if change > 0o7777 {
		return Err(format!("mode is too large ({change:o} > 7777)"));
	}
	Ok(match operator {
		Some('+') => current_mode | change,
		Some('-') => current_mode & !change,
		None if considering_dir && mode.len() < 5 => change | (current_mode & 0o6000),
		None | Some('=') => change,
		Some(_) => unreachable!(),
	})
}

/// Applies one symbolic mode clause to `current_mode`.
pub(crate) fn parse_symbolic(
	mut current_mode: u32,
	mut mode: &str,
	umask: u32,
	considering_dir: bool,
) -> Result<u32, String> {
	let (levels, offset) = parse_levels(mode);
	if offset == mode.len() {
		return Err(format!("invalid mode ({mode})"));
	}
	let respect_umask = offset == 0;
	mode = &mode[offset..];
	while !mode.is_empty() {
		let (operator, offset) = parse_operator(mode)?;
		mode = &mode[offset..];
		let (mut change, offset) = parse_change(mode, current_mode, considering_dir);
		if respect_umask {
			change &= !umask;
		}
		mode = &mode[offset..];
		match operator {
			'+' => current_mode |= change & levels,
			'-' => current_mode &= !(change & levels),
			'=' => {
				if considering_dir {
					change |= current_mode & 0o6000;
				}
				current_mode = (current_mode & !levels) | (change & levels);
			},
			_ => unreachable!(),
		}
	}
	Ok(current_mode)
}

fn parse_levels(mode: &str) -> (u32, usize) {
	let mut levels = 0_u32;
	let mut offset = 0_usize;
	for byte in mode.bytes() {
		levels |= match byte {
			b'u' => 0o4700,
			b'g' => 0o2070,
			b'o' => 0o1007,
			b'a' => 0o7777,
			_ => break,
		};
		offset += 1;
	}
	if offset == 0 {
		levels = 0o7777;
	}
	(levels, offset)
}

fn parse_operator(mode: &str) -> Result<(char, usize), String> {
	let operator = mode
		.chars()
		.next()
		.ok_or_else(|| "unexpected end of mode".to_owned())?;
	if matches!(operator, '+' | '-' | '=') {
		Ok((operator, operator.len_utf8()))
	} else {
		Err(format!("invalid operator (expected +, -, or =, but found {operator})"))
	}
}

fn parse_change(mode: &str, current_mode: u32, considering_dir: bool) -> (u32, usize) {
	let mut change = 0_u32;
	let mut offset = 0_usize;
	for byte in mode.bytes() {
		match byte {
			b'r' => change |= 0o444,
			b'w' => change |= 0o222,
			b'x' => change |= 0o111,
			b'X' if considering_dir || current_mode & 0o111 != 0 => change |= 0o111,
			b'X' => {},
			b's' => change |= 0o6000,
			b't' => change |= 0o1000,
			b'u' => {
				change = (current_mode & 0o700)
					| ((current_mode >> 3) & 0o070)
					| ((current_mode >> 6) & 0o007);
				offset += 1;
				break;
			},
			b'g' => {
				change = ((current_mode << 3) & 0o700)
					| (current_mode & 0o070)
					| ((current_mode >> 3) & 0o007);
				offset += 1;
				break;
			},
			b'o' => {
				change = ((current_mode << 6) & 0o700)
					| ((current_mode << 3) & 0o070)
					| (current_mode & 0o007);
				offset += 1;
				break;
			},
			_ => break,
		}
		offset += 1;
	}
	(change, offset)
}

/// Applies a comma-separated chmod expression to `current_mode`.
pub(crate) fn parse_chmod(
	current_mode: u32,
	mode_string: &str,
	considering_dir: bool,
	umask: u32,
) -> Result<u32, String> {
	let mut mode = current_mode;
	for clause in mode_string
		.split(',')
		.map(str::trim)
		.filter(|part| !part.is_empty())
	{
		mode = if clause.bytes().any(|byte| byte.is_ascii_digit()) {
			parse_numeric(mode, clause, considering_dir)?
		} else {
			parse_symbolic(mode, clause, umask, considering_dir)?
		};
	}
	Ok(mode)
}

/// Reads the process umask by atomically restoring it immediately after
/// inspection.
pub(crate) fn get_umask() -> u32 {
	#[cfg(unix)]
	{
		let mask = rustix::process::umask(fs::Mode::empty());
		let _ = rustix::process::umask(mask);
		mask.bits().into()
	}
	#[cfg(not(unix))]
	{
		0o022
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn numeric_modes_match_chmod_rules() {
		assert_eq!(parse_numeric(0o6755, "755", true).unwrap(), 0o6755);
		assert_eq!(parse_numeric(0o6755, "=755", true).unwrap(), 0o755);
		assert_eq!(parse_numeric(0o644, "+111", false).unwrap(), 0o755);
		assert_eq!(parse_numeric(0o777, "-022", false).unwrap(), 0o755);
		assert!(parse_numeric(0, "10000", false).is_err());
	}

	#[test]
	fn symbolic_modes_cover_umask_copy_and_conditional_execute() {
		for (initial, expression, umask, directory, expected) in [
			(0o644, "u+rwx,g-w,o=rx", 0, false, 0o745),
			(0o666, "+x", 0o022, false, 0o777),
			(0o644, "a+X", 0, false, 0o644),
			(0o644, "a+X", 0, true, 0o755),
			(0o744, "g=u", 0, false, 0o774),
			(0o640, "o=g", 0, false, 0o644),
			(0o777, "=rw", 0o022, false, 0o644),
			(0o2755, "g=rx", 0, true, 0o2755),
		] {
			assert_eq!(
				parse_chmod(initial, expression, directory, umask).unwrap(),
				expected,
				"{expression}"
			);
		}
	}
}
