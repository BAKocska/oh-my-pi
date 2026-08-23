//! Runtime-facing typed settings subscriptions.

use std::{marker, sync::Arc};

use crate::{
	SettingsDomain, SnapshotError, Subscription, TypedProjection, manager::SettingsManager,
};

/// Typed revision stream installed into one owning runtime.
pub struct DomainSubscription<D> {
	inner:   Subscription,
	_marker: marker::PhantomData<fn() -> D>,
}

impl<D: SettingsDomain> DomainSubscription<D> {
	/// Subscribes to `D` without introducing settings reads in the runtime loop.
	pub fn new(manager: &SettingsManager) -> Self {
		Self { inner: manager.subscribe::<D>(), _marker: marker::PhantomData }
	}

	/// Waits for and projects the next revision synchronously.
	pub fn recv(&mut self) -> Result<TypedProjection<D>, DomainSubscriptionError> {
		let snapshot = self.inner.recv()?;
		Ok(snapshot.project::<D>()?)
	}

	/// Waits for and projects the next revision asynchronously.
	pub async fn recv_async(&mut self) -> Result<TypedProjection<D>, DomainSubscriptionError> {
		let snapshot = self.inner.recv_async().await?;
		Ok(snapshot.project::<D>()?)
	}
}

/// Installs the current immutable projection and returns its revision stream.
pub fn install<D: SettingsDomain>(
	manager: &SettingsManager,
) -> Result<(Arc<D>, DomainSubscription<D>), DomainSubscriptionError> {
	let projection = manager.snapshot().project::<D>()?;
	Ok((projection.shared(), DomainSubscription::new(manager)))
}

/// Typed subscription failure.
#[derive(Debug, thiserror::Error)]
pub enum DomainSubscriptionError {
	/// The publisher closed.
	#[error(transparent)]
	Closed(#[from] flume::RecvError),
	/// The new snapshot did not decode as the owning domain.
	#[error(transparent)]
	Projection(#[from] SnapshotError),
}
