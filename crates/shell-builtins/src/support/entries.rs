//! User and group database lookups.

#[cfg(unix)]
use std::io;

#[cfg(unix)]
use nix::unistd::{Gid, Group, Uid, User};

/// Resolves a numeric user ID through the system user database.
#[cfg(unix)]
#[inline]
pub(crate) fn uid2usr(id: u32) -> io::Result<String> {
	User::from_uid(Uid::from_raw(id))
		.map_err(|error| io::Error::from_raw_os_error(error as i32))?
		.map(|user| user.name)
		.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("No such id: {id}")))
}

/// Resolves a numeric group ID through the system group database.
#[cfg(unix)]
#[inline]
pub(crate) fn gid2grp(id: u32) -> io::Result<String> {
	Group::from_gid(Gid::from_raw(id))
		.map_err(|error| io::Error::from_raw_os_error(error as i32))?
		.map(|group| group.name)
		.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("No such id: {id}")))
}

#[cfg(all(test, unix))]
mod tests {
	use nix::unistd::{Gid, Group, Uid, User};

	use super::{gid2grp, uid2usr};

	#[test]
	fn resolves_current_process_identity() {
		let uid = Uid::current();
		let expected_user = User::from_uid(uid)
			.expect("user database lookup failed")
			.expect("current user is absent from user database")
			.name;
		assert_eq!(uid2usr(uid.as_raw()).expect("uid lookup failed"), expected_user);

		let gid = Gid::current();
		let expected_group = Group::from_gid(gid)
			.expect("group database lookup failed")
			.expect("current group is absent from group database")
			.name;
		assert_eq!(gid2grp(gid.as_raw()).expect("gid lookup failed"), expected_group);
	}
}
